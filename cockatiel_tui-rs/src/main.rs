mod app;
mod colors;
mod db;
mod event;
mod hotkeys;
mod layout;
mod plugins;
mod supervisor;
mod windows;
mod ws_client;
mod ws_server;

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Terminal;
use tokio::sync::{mpsc, broadcast};

use app::{AppState, CredentialSession, PendingPrompt, WindowId};
use cockatiel_client::proto::{Prompt, PromptType};
use cockatiel_client::PromptKind;
use colors::load_colors;
use event::AppEvent;
use hotkeys::{load_hotkeys, Action};
use windows::{LogoWindow, LogWindow, ModulesWindow, ChartWindow, PromptsWindow};
use ws_client::{WsClient, WsCommand, WsEvent};
use ws_server::WsServer;

fn find_engine_addr() -> (String, u16, u32) {
    // Try config.json first
    for path in &["../config.json", "config.json"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) {
                let ip = config.get("engine_ip").and_then(|v| v.as_str()).unwrap_or("127.0.0.1").to_string();
                let port = config.get("engine_port").and_then(|v| v.as_u64()).unwrap_or(1111) as u16;
                let pin = config.get("engine_pin").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                return (ip, port, pin);
            }
        }
    }
    // The engine's own config.json uses "port" and "paring_pin"
    for path in &["../cockatiel_engine-rs/config.json", "cockatiel_engine-rs/config.json"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) {
                let port = config.get("port").and_then(|v| v.as_u64()).unwrap_or(1111) as u16;
                let pin = config.get("paring_pin").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                return ("127.0.0.1".into(), port, pin);
            }
        }
    }
    // Try .env
    for path in &["../.env", ".env"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            let mut ip = "127.0.0.1".to_string();
            let mut port = 1111u16;
            let mut pin = 0u32;
            for line in content.lines() {
                let line = line.trim();
                if let Some(val) = line.strip_prefix("ENGINE_IP=") {
                    ip = val.to_string();
                } else if let Some(val) = line.strip_prefix("ENGINE_PORT=") {
                    port = val.parse().unwrap_or(1111);
                } else if let Some(val) = line.strip_prefix("PIN=") {
                    pin = val.parse().unwrap_or(0);
                }
            }
            return (ip, port, pin);
        }
    }
    ("127.0.0.1".into(), 1111, 0)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let mut detached_window = None;
    let mut ws_parent_addr = None;
    let mut ws_parent_token = None;
    let mut override_ip = None;
    let mut override_port = None;
    let mut override_pin = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--detached" {
            if let Some(name) = args.get(i + 1) {
                detached_window = Some(name.clone());
                i += 2;
            } else {
                i += 1;
            }
        } else if args[i] == "--ws-addr" {
            if let Some(addr) = args.get(i + 1) {
                ws_parent_addr = Some(addr.clone());
                i += 2;
            } else {
                i += 1;
            }
        } else if args[i] == "--ws-token" {
            if let Some(token) = args.get(i + 1) {
                ws_parent_token = Some(token.clone());
                i += 2;
            } else {
                i += 1;
            }
        } else if args[i] == "--ip" || args[i] == "-i" {
            if let Some(val) = args.get(i + 1) {
                override_ip = Some(val.clone());
                i += 2;
            } else {
                i += 1;
            }
        } else if args[i] == "--port" || args[i] == "-p" {
            if let Some(val) = args.get(i + 1) {
                override_port = val.parse().ok();
                i += 2;
            } else {
                i += 1;
            }
        } else if args[i] == "--pin" {
            if let Some(val) = args.get(i + 1) {
                override_pin = val.parse().ok();
                i += 2;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    let config_dir = std::env::current_dir().unwrap_or_default();
    let hotkeys = load_hotkeys(&config_dir.join("hotkey_config.json"));
    let colors = load_colors(&config_dir.join("color_config.json"));

    let (mut ip, mut port, mut pin) = find_engine_addr();
    if let Some(v) = override_ip { ip = v; }
    if let Some(v) = override_port { port = v; }
    if let Some(v) = override_pin { pin = v; }

    // ── Supervisor: launch engine + plugins (only in engine mode) ──
    let mut supervisor = crate::supervisor::ProcessTable::new();
    let mut plugins: Vec<crate::plugins::Plugin> = Vec::new();
    if detached_window.is_none() {
        // Prefer engine config values for port/pin.
        if let Some((ep, epin)) = supervisor::read_engine_addr() {
            port = ep;
            pin = epin;
        }

        // Databases are an expected core of the engine — always launch the
        // user database service first so the engine can connect to it.
        let user_db_up = std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", supervisor::USER_DB_DEFAULT_PORT).parse().unwrap(),
            std::time::Duration::from_millis(300),
        ).is_ok();
        if !user_db_up {
            match supervisor::launch_user_db() {
                Ok(child) => {
                    let pid = child.id();
                    supervisor.insert(
                        "user-database".to_string(),
                        Arc::new(Mutex::new(supervisor::ManagedProcess { child })),
                    );
                    eprintln!("[supervisor] Launched user database (pid {})", pid);
                }
                Err(e) => eprintln!("[supervisor] User DB launch failed: {}", e),
            }
        }

        // Ensure the engine is running (launch if not already reachable).
        let engine_up = std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            std::time::Duration::from_millis(300),
        ).is_ok();
        if !engine_up {
            match supervisor::launch_engine() {
                Ok(child) => {
                    let pid = child.id();
                    supervisor.insert(
                        "engine".to_string(),
                        Arc::new(Mutex::new(supervisor::ManagedProcess { child })),
                    );
                    eprintln!("[supervisor] Launched engine (pid {})", pid);
                    // Wait for the engine to write its config (port/pin) so
                    // plugins launch with the right credentials.
                    for _ in 0..20 {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        if let Some((ep, epin)) = supervisor::read_engine_addr() {
                            port = ep;
                            pin = epin;
                            break;
                        }
                    }
                }
                Err(e) => eprintln!("[supervisor] Engine launch failed: {}", e),
            }
        }

        // Discover plugins recursively from the current directory and the repo's
        // `modules/` folder (sibling to this crate, where the modules live).
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut discovered = crate::plugins::discover_plugins(&cwd);
        if let Some(parent) = cwd.parent() {
            let modules_dir = parent.join("modules");
            if modules_dir.exists() {
                for p in crate::plugins::discover_plugins(&modules_dir) {
                    if !discovered.iter().any(|x| x.manifest.name == p.manifest.name) {
                        discovered.push(p);
                    }
                }
            }
        }
        discovered.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        plugins = discovered;
        for plugin in &plugins {
            // Register + add to ordering so the engine approves and routes.
            // NOTE: modules are intentionally NOT auto-launched at startup —
            // every module starts disabled; start them from the modules window.
            let position = plugin.manifest.capabilities.clone();
            supervisor::register_module(&plugin.manifest.name, &position, 100);
            supervisor::add_to_ordering(&plugin.manifest.name, &position, 100);
        }
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // EnableBracketedPaste makes the terminal deliver pasted text as a single
    // `Event::Paste` (so pasting a key/stream id works reliably) instead of a
    // rapid burst of individual keys.
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = AppState::new(colors, hotkeys);

    if let Some(ref window_name) = detached_window {
        // Detached mode: only show one window
        match window_name.as_str() {
            "log" => state.windows.push(Box::new(LogWindow::new())),
            "modules" => state.windows.push(Box::new(ModulesWindow::new())),
            "chart" => state.windows.push(Box::new(ChartWindow::new())),
            "prompts" => state.windows.push(Box::new(PromptsWindow::new())),
            _ => state.windows.push(Box::new(LogoWindow)),
        }
    } else {
        state.windows.push(Box::new(LogoWindow));
        state.windows.push(Box::new(LogWindow::new()));
        state.windows.push(Box::new(ModulesWindow::new()));
        state.windows.push(Box::new(ChartWindow::new()));
        state.windows.push(Box::new(PromptsWindow::new()));
    }

    let (ws_event_tx, mut ws_event_rx) = mpsc::unbounded_channel::<WsEvent>();
    let (ws_command_tx, ws_command_rx) = mpsc::unbounded_channel::<WsCommand>();

    // WS server for sub-windows
    let ws_server = WsServer::new().await;
    let ws_addr = ws_server.addr;
    let ws_auth_token = ws_server.auth_token.clone();
    let (ws_broadcast_tx, _) = broadcast::channel::<WsEvent>(64);
    ws_server.start(ws_broadcast_tx.subscribe());

    // Create WS client — parent mode if --ws-addr provided, else engine mode
    let mut ws_client = if let (Some(ref parent_addr), Some(ref parent_token)) = (&ws_parent_addr, &ws_parent_token) {
        WsClient::new_as_child(parent_addr.clone(), parent_token.clone(), ws_event_tx, ws_command_rx)
    } else {
        WsClient::new(ip, port, pin, ws_event_tx, ws_command_rx)
    };
    tokio::spawn(async move {
        ws_client.run().await;
    });

    let result = run_app(
        &mut terminal,
        &mut state,
        &mut ws_event_rx,
        ws_command_tx,
        ws_broadcast_tx,
        ws_addr,
        ws_auth_token,
        &mut supervisor,
        plugins,
        port,
        pin,
    ).await;

    // Tear down everything the TUI owns before exiting.
    for (name, proc) in supervisor.drain() {
        let mut proc = proc.lock().unwrap();
        eprintln!("[supervisor] Killing {} (pid {})", name, proc.pid());
        proc.kill();
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture, DisableBracketedPaste)?;
    terminal.show_cursor()?;

    if let Err(err) = result {
        eprintln!("Error: {}", err);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
    ws_event_rx: &mut mpsc::UnboundedReceiver<WsEvent>,
    ws_command_tx: mpsc::UnboundedSender<WsCommand>,
    ws_broadcast_tx: broadcast::Sender<WsEvent>,
    ws_addr: std::net::SocketAddr,
    ws_auth_token: String,
    supervisor: &mut supervisor::ProcessTable,
    plugins: Vec<crate::plugins::Plugin>,
    port: u16,
    pin: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    // Input events arrive instantly from a background crossterm reader thread.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AppEvent>();
    event::spawn_event_reader(event_tx);

    // Periodic redraw so time-driven UI (prompt countdown, module-error
    // expiry, streaming logs) updates even without input.
    let mut redraw = tokio::time::interval(Duration::from_millis(100));
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            maybe = event_rx.recv() => {
                if let Some(ev) = maybe {
                    if handle_input_event(
                        ev,
                        terminal,
                        state,
                        supervisor,
                        &plugins,
                        port,
                        pin,
                        &ws_command_tx,
                        ws_addr,
                        &ws_auth_token,
                    ).await? {
                        return Ok(());
                    }
                    // Credential entry just finished via the prompt subwindow —
                    // relaunch the module now that its config is saved.
                    if let Some(name) = state.pending_launch.take() {
                        if !supervisor.contains_key(&name) {
                            launch_module(
                                state,
                                &name,
                                supervisor,
                                &plugins,
                                port,
                                pin,
                                &ws_command_tx,
                            )
                            .await;
                        }
                    }
                }
            }
            maybe_ws = ws_event_rx.recv() => {
                if let Some(ev) = maybe_ws {
                    handle_ws_event(ev, state, &ws_broadcast_tx);
                    while let Ok(ev) = ws_event_rx.try_recv() {
                        handle_ws_event(ev, state, &ws_broadcast_tx);
                    }
                }
            }
            _ = redraw.tick() => {}
        }

        // Auto-deny any prompts that the user never answered before their timeout.
        {
            let mut expired_ids: Vec<String> = Vec::new();
            for pending in &state.pending_prompt {
                if Instant::now() >= pending.deadline {
                    expired_ids.push(pending.prompt.prompt_id_uuid7.clone());
                }
            }
            for prompt_id in expired_ids {
                // Expired credential-session prompts cancel the whole session
                // (nothing is sent to the engine for these).
                if prompt_is_credential(state, &prompt_id) {
                    cancel_credential_session(state);
                    continue;
                }
                let _ = ws_command_tx.send(WsCommand::SendPromptResponse {
                    prompt_id: prompt_id.clone(),
                    accepted: false,
                    reason: String::new(),
                });
                state.pending_prompt.retain(|p| p.prompt.prompt_id_uuid7 != prompt_id);
            }
            if state.pending_prompt.is_empty() {
                state.selected_prompt = 0;
            } else {
                state.selected_prompt = state.selected_prompt.min(state.pending_prompt.len() - 1);
            }
        }

        // Drain lines captured from module stdout/stderr into the log window.
        {
            let drained: Vec<crate::windows::log::LogEntry> = {
                let mut shared = state.module_logs.lock().unwrap();
                shared.drain(..).collect()
            };
            if !drained.is_empty() {
                if let Some(window) = state.get_window_mut(WindowId::Log) {
                    for entry in drained {
                        window.push_log(entry);
                    }
                }
            }
        }

        // Overlay supervisor-side module lifecycle statuses (starting/connected/
        // crashed/stopped) over the engine-reported statuses before rendering.
        {
            let runs = state.module_runs.lock().unwrap();
            let mut stale: Vec<String> = Vec::new();
            for entry in &mut state.stats.module_entries {
                let Some(rs) = runs.get(&entry.name) else {
                    continue;
                };
                match rs.as_str() {
                    // Supervisor-owned states the engine can't express.
                    "starting" | "stopped" => entry.status = rs.clone(),
                    // Start failure: only show if the engine isn't already
                    // reporting a failure state.
                    "crashed" => {
                        if entry.status != "crashed" && entry.status != "disconnected" {
                            entry.status = rs.clone();
                        }
                    }
                    // Once connected, trust the engine's fresher status; drop
                    // our flag if the engine reports the module down.
                    "connected" => {
                        if entry.status != "connected" {
                            stale.push(entry.name.clone());
                        }
                    }
                    _ => {}
                }
            }
            drop(runs);
            if !stale.is_empty() {
                let mut runs = state.module_runs.lock().unwrap();
                for name in stale {
                    runs.remove(&name);
                }
            }
        }

        // Error overlay: a still-connected module that recently emitted an
        // error line shows "error" instead of "connected". The module keeps
        // running; it only clears once it stops erroring for a while.
        {
            let errors = state.module_errors.lock().unwrap();
            if !errors.is_empty() {
                let now = Instant::now();
                for entry in &mut state.stats.module_entries {
                    if entry.status == "connected" {
                        if let Some(at) = errors.get(&entry.name) {
                            if now.duration_since(*at) < Duration::from_secs(20) {
                                entry.status = "error".to_string();
                            }
                        }
                    }
                }
            }
        }

        terminal.draw(|frame| {
            let size = frame.area();
            let areas = state.layout.compute(size);

            for window in &mut state.windows {
                let (area, is_active) = match window.id() {
                    WindowId::Logo => (areas.logo, state.active_window == WindowId::Logo),
                    WindowId::Log => (areas.log, state.active_window == WindowId::Log),
                    WindowId::Modules => (areas.modules, state.active_window == WindowId::Modules),
                    WindowId::Chart => (areas.chart, state.active_window == WindowId::Chart),
                    WindowId::Prompts => (areas.prompts, state.active_window == WindowId::Prompts),
                };

                // The prompts window highlights the currently-selected queue item.
                window.set_prompt_selected(state.selected_prompt);

                // Render window normally
                window.render(
                    area,
                    frame.buffer_mut(),
                    is_active,
                    &state.stats,
                    &state.colors,
                    &state.hotkeys,
                    state.pending_prompt.make_contiguous(),
                );
            }

            // Status bar
            let conn_status = if state.connected { "connected" } else { "disconnected" };
            let conn_color = if state.connected { Color::Green } else { Color::Red };
            let border_color = state.colors.active_border_color(state.active_window.name());

            let status_hotkeys = state.hotkeys.format_global();
            let status_line = Line::from(vec![
                Span::styled(
                    format!(" {} ", state.active_window.title()),
                    Style::default().fg(Color::Black).bg(border_color),
                ),
                Span::styled(
                    format!(" {} ", conn_status),
                    Style::default().fg(conn_color),
                ),
                Span::styled(
                    format!("  {}", status_hotkeys),
                    Style::default().fg(Color::DarkGray).bg(Color::Black),
                ),
            ]);
            let status_para = Paragraph::new(status_line);
            status_para.render(areas.status_bar, frame.buffer_mut());

            })?;
    }
}

/// Handle an engine (WebSocket) event. Broadcasts to sub-windows, updates the
/// log/stats/prompt state.
fn handle_ws_event(
    event: WsEvent,
    state: &mut AppState,
    ws_broadcast_tx: &broadcast::Sender<WsEvent>,
) {
    let _ = ws_broadcast_tx.send(event.clone());
    match event {
        WsEvent::Connected => {
            state.connected = true;
            state.stats.engine_status = "connected".to_string();
        }
        WsEvent::Disconnected => {
            state.connected = false;
            state.stats.engine_status = "disconnected".to_string();
        }
        WsEvent::Log { source, message, event_type } => {
            if let Some(window) = state.get_window_mut(WindowId::Log) {
                window.push_log(crate::windows::log::LogEntry {
                    timestamp: String::new(),
                    source,
                    message,
                    event_type,
                });
            }
        }
        WsEvent::StatsUpdate(mut stats) => {
            stats.engine_status = state.stats.engine_status.clone();
            stats.connection = state.stats.connection.clone();
            state.stats = stats;
            sync_module_runs(state);
        }
        WsEvent::ConnectionInfo { ip, port, pin } => {
            state.stats.connection = db::ConnectionInfo { ip, port, pin };
        }
        WsEvent::Prompt(prompt) => {
            let timeout = if prompt.timeout > 0 {
                prompt.timeout as u64
            } else {
                30
            };
            state.pending_prompt.push_back(crate::app::PendingPrompt {
                deadline: Instant::now() + Duration::from_secs(timeout),
                prompt,
                text_input: String::new(),
            });
        }
        _ => {}
    }
}

/// Handle a single input event (key / mouse / resize). Returns true when the
/// app should quit.
#[allow(clippy::too_many_arguments)]
async fn handle_input_event(
    ev: AppEvent,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
    supervisor: &mut supervisor::ProcessTable,
    plugins: &[crate::plugins::Plugin],
    port: u16,
    pin: u32,
    ws_command_tx: &mpsc::UnboundedSender<WsCommand>,
    ws_addr: std::net::SocketAddr,
    ws_auth_token: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    match ev {
        AppEvent::Key(key) => {
            // Prompts are answered in the dedicated prompts window (Tab to
            // focus it). While another window is focused, prompts wait in the
            // queue and the rest of the TUI stays fully navigable. Left/right
            // cycle the queue; y/n (or typed text + Enter) answers the focused
            // prompt.
            if state.active_window == WindowId::Prompts && !state.pending_prompt.is_empty() {
                if handle_prompt_key(state, key, ws_command_tx) {
                    return Ok(false);
                }
            }

            // Double-Esc quits. Ctrl+C is deliberately NOT bound to quit so
            // that copy/paste stays safe; a single Esc just records the time
            // (an Esc consumed by a prompt/form never reaches this point).
            if key.code == KeyCode::Esc {
                let now = Instant::now();
                let double_esc = state
                    .last_esc_press
                    .map(|t| now.duration_since(t) < Duration::from_secs(2))
                    .unwrap_or(false);
                state.last_esc_press = Some(now);
                if double_esc {
                    return Ok(true);
                }
            }

            if let Some(Action::Quit) = state.handle_global_key(key) {
                return Ok(true);
            }

            let active_id = state.active_window;
            let window_name = active_id.name().to_string();

            // Config-driven window actions (start/stop/del/auto/creds/test/...)
            if let Some(action) = state
                .hotkeys
                .window_actions
                .get(&window_name)
                .and_then(|m| m.get(&key))
                .cloned()
            {
                if is_dispatchable(&action) {
                    let action = fill_window_action(state, &window_name, action);
                    if dispatch_action(
                        state,
                        action,
                        supervisor,
                        plugins,
                        port,
                        pin,
                        ws_command_tx,
                        ws_addr,
                        ws_auth_token,
                    )
                    .await?
                    {
                        return Ok(true);
                    }
                    return Ok(false);
                }
            }

            let mut stats = std::mem::take(&mut state.stats);
            if let Some(window) = state.get_window_mut(active_id) {
                let action = window.handle_key(key, &mut stats);
                state.stats = stats;
                if let Some(action) = action {
                    if dispatch_action(
                        state,
                        action,
                        supervisor,
                        plugins,
                        port,
                        pin,
                        ws_command_tx,
                        ws_addr,
                        ws_auth_token,
                    )
                    .await?
                    {
                        return Ok(true);
                    }
                }
            } else {
                state.stats = stats;
            }
            Ok(false)
        }
        AppEvent::Paste(text) => {
            // Pasting makes sense into a focused free-text prompt (e.g. an API
            // key or stream id). Ignore it everywhere else.
            if state.active_window == WindowId::Prompts && !state.pending_prompt.is_empty() {
                let idx = state.selected_prompt.min(state.pending_prompt.len() - 1);
                if state.pending_prompt[idx].prompt.kind() != PromptKind::Boolean {
                    state.pending_prompt[idx].text_input.push_str(&text);
                }
            }
            Ok(false)
        }
        AppEvent::Mouse(mouse) => {
            let size = terminal.size()?;
            let full_area = Rect::new(0, 0, size.width, size.height);

            match mouse.kind {
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                    // Clicking a prompt's link opens it in the browser.
                    if !state.pending_prompt.is_empty() {
                        if let Some((rect, url)) = state.windows.iter().find_map(|w| w.pending_link()) {
                            let click = Rect { x: mouse.column, y: mouse.row, width: 1, height: 1 };
                            if rect.intersects(click) {
                                let _ = open::that(url);
                            }
                        }
                    }

                    // Focus-on-click: map click to window area
                    let areas = state.layout.compute(full_area);
                    let click_rect = Rect { x: mouse.column, y: mouse.row, width: 1, height: 1 };
                    let clicked_window = [
                        (WindowId::Logo, areas.logo),
                        (WindowId::Log, areas.log),
                        (WindowId::Modules, areas.modules),
                        (WindowId::Chart, areas.chart),
                        (WindowId::Prompts, areas.prompts),
                    ].iter().find(|(_, area)| area.intersects(click_rect)).map(|(id, _)| *id);

                    if let Some(window_id) = clicked_window {
                        state.active_window = window_id;
                    }

                    // Start drag if near a border
                    if let Some(edge) = state.layout.hit_test_border(full_area, mouse.column, mouse.row) {
                        state.layout.dragging = Some(edge);
                        state.layout.drag_start = Some((mouse.column, mouse.row));
                    }
                }
                crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                    if let Some(edge) = state.layout.dragging {
                        state.layout.update_from_drag(full_area, edge, mouse.column, mouse.row);
                    }
                }
                crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                    state.layout.dragging = None;
                    state.layout.drag_start = None;
                }
                _ => {}
            }

            let active_id = state.active_window;
            if let Some(window) = state.get_window_mut(active_id) {
                if let Some(Action::Quit) = window.handle_mouse(mouse, full_area) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        AppEvent::Resize(w, h) => {
            if w > 0 && h > 0 {
                let _ = terminal.resize(Rect::new(0, 0, w, h));
            }
            Ok(false)
        }
    }
}

/// Handle a keypress for the prompt currently focused in the prompts window.
/// Returns true if the key was consumed by the prompt (so the caller skips
/// normal navigation/actions). Left/right cycle through the queue; y/n or
/// typed text + Enter answers the focused prompt; Esc cancels it.
fn handle_prompt_key(state: &mut AppState, key: KeyEvent, ws_command_tx: &mpsc::UnboundedSender<WsCommand>) -> bool {
    let len = state.pending_prompt.len();
    if len == 0 {
        return false;
    }
    if state.selected_prompt >= len {
        state.selected_prompt = len - 1;
    }

    // Left/right cycle the queue regardless of prompt type.
    if key.code == KeyCode::Left || key.code == KeyCode::Right {
        state.selected_prompt = if key.code == KeyCode::Left {
            (state.selected_prompt + len - 1) % len
        } else {
            (state.selected_prompt + 1) % len
        };
        return true;
    }

    let prompt_id = state.pending_prompt[state.selected_prompt].prompt.prompt_id_uuid7.clone();

    // Boolean prompts answer y/n; string/credential prompts accept typed text
    // (credentials are masked on display, but the plaintext still lives in
    // `text_input` so it can travel in PromptResponse.reason). Prompts that
    // belong to a credential session are collected locally instead of being
    // answered against the engine.
    if state.pending_prompt[state.selected_prompt].prompt.kind() != PromptKind::Boolean {
        match key.code {
            KeyCode::Char(c)
                if key.kind != KeyEventKind::Release
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                state.pending_prompt[state.selected_prompt].text_input.push(c);
                return true;
            }
            KeyCode::Backspace => {
                state.pending_prompt[state.selected_prompt].text_input.pop();
                return true;
            }
            KeyCode::Enter => {
                let reason = state.pending_prompt[state.selected_prompt].text_input.clone();
                if !finish_credential_field(state, &prompt_id, reason.clone(), ws_command_tx) {
                    let _ = ws_command_tx.send(WsCommand::SendPromptResponse {
                        prompt_id,
                        accepted: true,
                        reason,
                    });
                    remove_prompt_at(state, state.selected_prompt);
                }
                return true;
            }
            KeyCode::Esc => {
                if prompt_is_credential(state, &prompt_id) {
                    cancel_credential_session(state);
                } else {
                    let _ = ws_command_tx.send(WsCommand::SendPromptResponse {
                        prompt_id,
                        accepted: false,
                        reason: String::new(),
                    });
                    remove_prompt_at(state, state.selected_prompt);
                }
                return true;
            }
            _ => return false,
        }
    }

    // y/n prompt.
    let answer = match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
        _ => None,
    };
    if let Some(accepted) = answer {
        if prompt_is_credential(state, &prompt_id) {
            if accepted {
                finish_credential_field(state, &prompt_id, "true".to_string(), ws_command_tx);
            } else {
                cancel_credential_session(state);
            }
        } else {
            let _ = ws_command_tx.send(WsCommand::SendPromptResponse {
                prompt_id,
                accepted,
                reason: String::new(),
            });
            remove_prompt_at(state, state.selected_prompt);
        }
        return true;
    }
    false
}

/// Remove the prompt at `idx`, keeping `selected_prompt` valid.
fn remove_prompt_at(state: &mut AppState, idx: usize) {
    state.pending_prompt.remove(idx);
    if state.pending_prompt.is_empty() {
        state.selected_prompt = 0;
    } else {
        state.selected_prompt = state.selected_prompt.min(state.pending_prompt.len() - 1);
    }
}

/// True when `prompt_id` belongs to the active credential-entry session.
fn prompt_is_credential(state: &AppState, prompt_id: &str) -> bool {
    state
        .credential_session
        .as_ref()
        .map(|s| s.fields.iter().any(|(_, id)| id == prompt_id))
        .unwrap_or(false)
}

/// Record an answered credential field and, once every field is answered, send
/// the collected values to the engine as a `set_credentials` query. Returns
/// true if the prompt was a credential field (fully handled here).
fn finish_credential_field(
    state: &mut AppState,
    prompt_id: &str,
    value: String,
    ws_command_tx: &mpsc::UnboundedSender<WsCommand>,
) -> bool {
    if !prompt_is_credential(state, prompt_id) {
        return false;
    }
    // Record the value for this field.
    if let Some(key) = state
        .credential_session
        .as_ref()
        .and_then(|s| s.fields.iter().find(|(_, id)| id == prompt_id))
        .map(|(k, _)| k.clone())
    {
        if let Some(s) = state.credential_session.as_mut() {
            s.collected.insert(key, value);
        }
    }

    // Remove the answered prompt from the queue.
    if let Some(idx) = state
        .pending_prompt
        .iter()
        .position(|p| p.prompt.prompt_id_uuid7 == prompt_id)
    {
        remove_prompt_at(state, idx);
    }

    // All fields answered → send set_credentials and clear the session.
    let all_done = state
        .credential_session
        .as_ref()
        .map(|s| s.fields.iter().all(|(key, _)| s.collected.contains_key(key)))
        .unwrap_or(false);
    if all_done {
        let (module_name, values, ids) = {
            let s = state.credential_session.as_ref().unwrap();
            (
                s.module_name.clone(),
                s.collected.clone(),
                s.fields.iter().map(|(_, id)| id.clone()).collect::<Vec<_>>(),
            )
        };
        state.credential_session = None;
        state.pending_prompt.retain(|p| !ids.contains(&p.prompt.prompt_id_uuid7));
        if state.pending_prompt.is_empty() {
            state.selected_prompt = 0;
        } else {
            state.selected_prompt = state.selected_prompt.min(state.pending_prompt.len() - 1);
        }
        let payload = serde_json::json!({ "module_name": module_name, "values": values });
        let _ = ws_command_tx.send(WsCommand::SendQuery {
            query_id: "set_credentials".to_string(),
            sql: payload.to_string(),
        });
        // The engine saves the config; ask the supervisor to launch the module.
        state.pending_launch = Some(module_name);
    }
    true
}

/// Abort a credential-entry session: remove all of its prompts from the queue
/// and drop the session without sending anything.
fn cancel_credential_session(state: &mut AppState) {
    if let Some(s) = state.credential_session.take() {
        let ids: Vec<String> = s.fields.iter().map(|(_, id)| id.clone()).collect();
        state.pending_prompt.retain(|p| !ids.contains(&p.prompt.prompt_id_uuid7));
        if state.pending_prompt.is_empty() {
            state.selected_prompt = 0;
        } else {
            state.selected_prompt = state.selected_prompt.min(state.pending_prompt.len() - 1);
        }
    }
}

/// Ask for a module's credentials through the prompt subwindow: one PendingPrompt
/// per credential field (sensitive fields are Credential-kind / masked). When
/// all are answered, `finish_credential_field` sends `set_credentials`.
fn start_credential_session(state: &mut AppState, module: crate::db::ModuleStatus) {
    if module.credentials.is_empty() {
        return;
    }
    cancel_credential_session(state);

    let mut fields = Vec::new();
    let mut new_prompts = Vec::new();
    for field in &module.credentials {
        let prompt_id = uuid::Uuid::now_v7().to_string();
        let kind = if field.sensitive {
            PromptKind::Credential
        } else {
            PromptKind::String
        };
        let prompt_type = match kind {
            PromptKind::Boolean => PromptType::Boolean,
            PromptKind::String => PromptType::String,
            PromptKind::Credential => PromptType::Credential,
        };
        let existing = module
            .credential_values
            .get(&field.key)
            .cloned()
            .unwrap_or_default();
        let mut details = format!(
            "Enter the value for '{}' to configure {}.{}",
            field.label,
            module.name,
            if field.optional { " (optional — leave empty to skip)" } else { "" }
        );
        if !module.description.is_empty() {
            details.push_str(&format!("\n\nAbout this module: {}", module.description));
        }
        let prompt = Prompt {
            prompt_id_uuid7: prompt_id.clone(),
            prompt: format!("credentials: {}", module.name),
            details,
            yes_dialog: "Submit".to_string(),
            no_dialog: "Cancel".to_string(),
            timeout: 300,
            origin: module.name.clone(),
            origin_uuid7: String::new(),
            instructions: String::new(),
            link: String::new(),
            input_label: field.label.clone(),
            prompt_type: prompt_type as i32,
        };
        new_prompts.push(PendingPrompt {
            deadline: Instant::now() + Duration::from_secs(300),
            prompt,
            text_input: existing,
        });
        fields.push((field.key.clone(), prompt_id));
    }

    state.pending_prompt.extend(new_prompts);
    state.credential_session = Some(CredentialSession {
        module_name: module.name,
        fields,
        collected: HashMap::new(),
    });
    // Bring the prompts window into focus so the user can answer.
    state.active_window = WindowId::Prompts;
}

/// Actions that the supervisor/engine actually dispatches (as opposed to
/// window-internal actions like time-window toggles, which stay in the window).
fn is_dispatchable(action: &Action) -> bool {
    matches!(
        action,
        Action::Quit
            | Action::PopOut(_)
            | Action::StartModule(_)
            | Action::StopModule(_)
            | Action::DeleteModule(_)
            | Action::ToggleAutostart(_)
            | Action::EditCredentials(_)
            | Action::RunTests
    )
}

/// The module currently selected in the active window.
fn selected_module_name(state: &AppState) -> String {
    for window in &state.windows {
        if let Some(name) = window.selected_module_name(&state.stats) {
            return name;
        }
    }
    state
        .stats
        .module_entries
        .first()
        .map(|m| m.name.clone())
        .unwrap_or_default()
}

/// Fill in the module/window name for name-bearing actions resolved from the
/// hotkey config (which stores them with an empty payload).
fn fill_window_action(state: &AppState, window_name: &str, action: Action) -> Action {
    let name = selected_module_name(state);
    match action {
        Action::StartModule(_) => Action::StartModule(name),
        Action::StopModule(_) => Action::StopModule(name),
        Action::DeleteModule(_) => Action::DeleteModule(name),
        Action::ToggleAutostart(_) => Action::ToggleAutostart(name),
        Action::EditCredentials(_) => Action::EditCredentials(name),
        Action::PopOut(_) => Action::PopOut(window_name.to_string()),
        other => other,
    }
}

/// When the engine reports a module as connected, promote any "starting" run
/// status to "connected".
fn sync_module_runs(state: &AppState) {
    let mut runs = state.module_runs.lock().unwrap();
    for entry in &state.stats.module_entries {
        if entry.status == "connected" {
            if let Some(cur) = runs.get(&entry.name) {
                if cur == "starting" {
                    runs.insert(entry.name.clone(), "connected".to_string());
                }
            }
        }
    }
}

/// Launch a module via the supervisor: set its run status to "starting", spawn
/// the plugin, watch for it to connect or exit, and surface its stdout/stderr
/// in the log window + the engine's timeline database.
async fn launch_module(
    state: &mut AppState,
    name: &str,
    supervisor: &mut supervisor::ProcessTable,
    plugins: &[crate::plugins::Plugin],
    port: u16,
    pin: u32,
    ws_command_tx: &mpsc::UnboundedSender<WsCommand>,
) {
    let Some(plugin) = plugins.iter().find(|p| p.manifest.name == name) else {
        return;
    };
    if supervisor.contains_key(name) {
        return;
    }
    state
        .module_runs
        .lock()
        .unwrap()
        .insert(name.to_string(), "starting".to_string());
    match supervisor::launch_plugin(plugin, port, pin) {
        Ok(mut child) => {
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let pid = child.id();
            let proc = Arc::new(Mutex::new(supervisor::ManagedProcess { child }));
            supervisor.insert(name.to_string(), proc.clone());
            eprintln!(
                "[supervisor] Launched {} (pid {}) — waiting to connect",
                name, pid
            );
            spawn_monitor(name.to_string(), proc, state.module_runs.clone(), plugin.manifest.terminal);
            if let Some(pipe) = stdout {
                spawn_log_reader(
                    state.module_logs.clone(),
                    state.module_errors.clone(),
                    ws_command_tx.clone(),
                    name.to_string(),
                    1,
                    pipe,
                );
            }
            if let Some(pipe) = stderr {
                spawn_log_reader(
                    state.module_logs.clone(),
                    state.module_errors.clone(),
                    ws_command_tx.clone(),
                    name.to_string(),
                    3,
                    pipe,
                );
            }
        }
        Err(e) => {
            state
                .module_runs
                .lock()
                .unwrap()
                .insert(name.to_string(), "crashed".to_string());
            eprintln!("[supervisor] Failed to launch {}: {}", name, e);
        }
    }
}

/// Read lines from a module's stdout/stderr pipe: show them in the TUI log
/// window, forward them to the engine so they persist in the timeline DB, and
/// record when a module emits an error so its status can show "error".
fn spawn_log_reader(
    logs: Arc<Mutex<VecDeque<crate::windows::log::LogEntry>>>,
    errors: Arc<Mutex<HashMap<String, Instant>>>,
    ws_tx: mpsc::UnboundedSender<WsCommand>,
    source: String,
    event_type: i32,
    pipe: impl std::io::Read + Send + 'static,
) {
    tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(pipe);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            {
                let mut logs = logs.lock().unwrap();
                logs.push_back(crate::windows::log::LogEntry {
                    timestamp: String::new(),
                    source: source.clone(),
                    message: line.clone(),
                    event_type,
                });
                while logs.len() > 500 {
                    logs.pop_front();
                }
            }
            // Error-level lines (tracing "ERROR", error: / failed / panic ...)
            // mark the module as currently erroring.
            if line_indicates_error(&line) {
                errors.lock().unwrap().insert(source.clone(), Instant::now());
            }
            let _ = ws_tx.send(WsCommand::SendLog {
                source: source.clone(),
                message: line,
            });
        }
    });
}

/// Best-effort detection of an error line from a module's stderr.
fn line_indicates_error(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error") || lower.contains("panic") || lower.contains("failed to") || lower.contains("rejected")
}

/// Spawn a background monitor for a just-launched module: it flips the run
/// status to "crashed" ONLY when the child process actually exits (or the
/// launch failed). A process that is still alive but hasn't connected yet is
/// left as "starting" — sync_module_runs promotes it to "connected" the moment
/// the engine reports it. For terminal modules the launcher process (e.g.
/// osascript) detaches immediately, so exit is never a crash signal.
fn spawn_monitor(
    name: String,
    proc: Arc<Mutex<supervisor::ManagedProcess>>,
    runs: Arc<Mutex<HashMap<String, String>>>,
    is_terminal: bool,
) {
    tokio::spawn(async move {
        loop {
            // Crashed: the child process exited (not applicable to terminal
            // modules, whose launcher detaches immediately).
            if !is_terminal {
                let mut guard = proc.lock().unwrap();
                match guard.child.try_wait() {
                    Ok(Some(_)) => {
                        let mut r = runs.lock().unwrap();
                        if r.get(&name).map(|s| s.as_str()) == Some("starting") {
                            r.insert(name.clone(), "crashed".to_string());
                        }
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        // Can't inspect the child anymore — stop monitoring
                        // without misreporting a crash.
                        break;
                    }
                }
            }
            // Connected: the engine reported it (set by sync_module_runs).
            {
                let r = runs.lock().unwrap();
                if r.get(&name).map(|s| s.as_str()) == Some("connected") {
                    break;
                }
            }
            // Still running but not connected yet — keep waiting. We do NOT
            // report a crash for a live process (it may be compiling or
            // waiting on config); sync_module_runs will flip it when it lands.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });
}

/// Dispatch a supervisor/engine action. Returns true when the app should quit.
#[allow(clippy::too_many_arguments)]
async fn dispatch_action(
    state: &mut AppState,
    action: Action,
    supervisor: &mut supervisor::ProcessTable,
    plugins: &[crate::plugins::Plugin],
    port: u16,
    pin: u32,
    ws_command_tx: &mpsc::UnboundedSender<WsCommand>,
    ws_addr: std::net::SocketAddr,
    ws_auth_token: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    match action {
        Action::Quit => return Ok(true),
        Action::PopOut(window_name) => {
            state.popped_out.insert(window_name.clone());
            let exe = std::env::current_exe().unwrap_or_default();
            let _ = std::process::Command::new(&exe)
                .args([
                    "--detached",
                    &window_name,
                    "--ws-addr",
                    &ws_addr.to_string(),
                    "--ws-token",
                    ws_auth_token,
                ])
                .spawn();
        }
        Action::StartModule(name) => {
            if plugins.iter().any(|p| p.manifest.name == name) && !supervisor.contains_key(&name) {
                // If the module declares credentials it doesn't have yet,
                // ask for them via the prompt subwindow instead of launching a
                // process that would hang waiting for interactive input.
                let needs_creds = state
                    .stats
                    .module_entries
                    .iter()
                    .find(|m| m.name == name)
                    .map(|m| !m.config_complete && !m.credentials.is_empty())
                    .unwrap_or(false);
                if needs_creds {
                    if let Some(module) = state
                        .stats
                        .module_entries
                        .iter()
                        .find(|m| m.name == name)
                        .cloned()
                    {
                        start_credential_session(state, module);
                    }
                } else {
                    launch_module(state, &name, supervisor, plugins, port, pin, ws_command_tx).await;
                }
            }
        }
        Action::StopModule(name) => {
            if let Some(proc) = supervisor.remove(&name) {
                let mut proc = proc.lock().unwrap();
                eprintln!("[supervisor] Killing {} (pid {})", name, proc.pid());
                proc.kill();
            }
            state
                .module_runs
                .lock()
                .unwrap()
                .insert(name, "stopped".to_string());
        }
        Action::DeleteModule(name) => {
            if let Some(proc) = supervisor.remove(&name) {
                let mut proc = proc.lock().unwrap();
                eprintln!("[supervisor] Killing {} (pid {})", name, proc.pid());
                proc.kill();
            }
            state.module_runs.lock().unwrap().remove(&name);
            supervisor::remove_from_ordering(&name);
            // Also remove from modules.json via the engine registry file.
            if let Ok(data) = std::fs::read_to_string(supervisor::modules_registry_path()) {
                if let Ok(mut registry) = serde_json::from_str::<serde_json::Value>(&data) {
                    if let Some(arr) = registry.as_array_mut() {
                        arr.retain(|e| e.get("name").and_then(|v| v.as_str()) != Some(name.as_str()));
                        if let Ok(pretty) = serde_json::to_string_pretty(&registry) {
                            let _ = std::fs::write(supervisor::modules_registry_path(), pretty);
                        }
                    }
                }
            }
        }
        Action::ToggleAutostart(name) => {
            // Flip autostart in the plugin's manifest file directly.
            if let Some(plugin) = plugins.iter().find(|p| p.manifest.name == name) {
                let manifest_path = plugin.directory.join(crate::plugins::MANIFEST_FILENAME);
                if let Ok(data) = std::fs::read_to_string(&manifest_path) {
                    if let Ok(mut manifest) = serde_json::from_str::<serde_json::Value>(&data) {
                        let cur = manifest.get("autostart").and_then(|v| v.as_bool()).unwrap_or(false);
                        manifest["autostart"] = serde_json::json!(!cur);
                        if let Ok(pretty) = serde_json::to_string_pretty(&manifest) {
                            let _ = std::fs::write(&manifest_path, pretty);
                        }
                    }
                }
            }
        }
        Action::EditCredentials(name) => {
            // Open the credential form for the selected module
            let module = state
                .stats
                .module_entries
                .iter()
                .find(|m| m.name == name)
                .cloned();
            if let Some(module) = module {
                if !module.credentials.is_empty() {
                    start_credential_session(state, module);
                }
            }
        }
        Action::RunTests => {
            // Run the compliance suite against the selected module.
            let selected_name = selected_module_name(state);
            let payload = serde_json::json!({
                "suite": "all",
                "module": selected_name,
                "iterations": 20,
            });
            let _ = ws_command_tx.send(WsCommand::SendQuery {
                query_id: "test_run".to_string(),
                sql: payload.to_string(),
            });
        }
        _ => {}
    }
    Ok(false)
}
