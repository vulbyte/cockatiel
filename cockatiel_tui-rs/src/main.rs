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
use windows::{LogoWindow, LogWindow, ModulesWindow, ChartWindow, PromptsWindow, UsersWindow};
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
    // The engine's own config.json uses "port"; the PIN (a secret) lives in
    // the engine's .env as COCKATIEL_PIN, with a legacy config.json fallback.
    for path in &["../cockatiel_engine-rs/config.json", "cockatiel_engine-rs/config.json"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) {
                let port = config.get("port").and_then(|v| v.as_u64()).unwrap_or(1111) as u16;
                let env_path = path.replace("config.json", ".env");
                let pin = supervisor::read_env_value(std::path::Path::new(&env_path), "COCKATIEL_PIN")
                    .and_then(|v| v.parse().ok())
                    .or_else(|| config.get("paring_pin").and_then(|v| v.as_u64()).map(|v| v as u32))
                    .unwrap_or(0);
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
        // Prefer engine config values for port/pin, BUT explicit CLI overrides
        // (--port/--pin) win — a user pointing at a remote/renumbered engine
        // must be able to override the local config.json.
        if let Some((ep, epin)) = supervisor::read_engine_addr() {
            if override_port.is_none() {
                port = ep;
            }
            if override_pin.is_none() {
                pin = epin;
            }
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
                        Arc::new(Mutex::new(supervisor::ManagedProcess {
                            child,
                            terminal_window: None,
                            terminal_pidfile: None,
                        })),
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
                        Arc::new(Mutex::new(supervisor::ManagedProcess {
                            child,
                            terminal_window: None,
                            terminal_pidfile: None,
                        })),
                    );
                    eprintln!("[supervisor] Launched engine (pid {})", pid);
                    // Wait for the engine to write its config (port/pin) so
                    // plugins launch with the right credentials. Explicit CLI
                    // overrides still win over the freshly-written config.
                    for _ in 0..20 {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        if let Some((ep, epin)) = supervisor::read_engine_addr() {
                            if override_port.is_none() {
                                port = ep;
                            }
                            if override_pin.is_none() {
                                pin = epin;
                            }
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

    // Auto-rebuild channel: a crashed module's name is sent here and the main
    // loop rebuilds + relaunches it (capped to avoid infinite loops).
    let (rebuild_tx, rebuild_rx) = mpsc::unbounded_channel::<String>();
    state.rebuild_tx = Some(rebuild_tx);

    // Runtime-crash channel: a module that died AFTER connecting is recovered
    // through the crash ladder (restart → rebuild → rollback, unlimited retries).
    let (restart_tx, restart_rx) = mpsc::unbounded_channel::<String>();
    state.restart_tx = Some(restart_tx);

    // Launch channel: background tasks resolve module launches (building if
    // needed) and report back, so a cold build never blocks the UI loop.
    let (launch_tx, launch_rx) =
        mpsc::unbounded_channel::<(String, Result<(String, Vec<String>), String>)>();
    state.launch_tx = Some(launch_tx);

    if let Some(ref window_name) = detached_window {
        // Detached mode: only show one window. The window's id becomes the
        // active window so its border/title colors render as focused.
        let wid = match window_name.as_str() {
            "log" => { state.windows.push(Box::new(LogWindow::new())); WindowId::Log }
            "modules" => { state.windows.push(Box::new(ModulesWindow::new())); WindowId::Modules }
            "chart" => { state.windows.push(Box::new(ChartWindow::new())); WindowId::Chart }
            "prompts" => { state.windows.push(Box::new(PromptsWindow::new())); WindowId::Prompts }
            "users" => { state.windows.push(Box::new(UsersWindow::new())); WindowId::Users }
            _ => { state.windows.push(Box::new(LogoWindow)); WindowId::Logo }
        };
        state.active_window = wid;
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
    let ws_server_cmd_tx = ws_command_tx.clone();
    ws_server.start(ws_broadcast_tx.subscribe(), ws_server_cmd_tx);

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
        rebuild_rx,
        restart_rx,
        launch_rx,
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
    mut rebuild_rx: mpsc::UnboundedReceiver<String>,
    mut restart_rx: mpsc::UnboundedReceiver<String>,
    mut launch_rx: mpsc::UnboundedReceiver<(String, Result<(String, Vec<String>), String>)>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Input events arrive instantly from a background crossterm reader thread.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AppEvent>();
    event::spawn_event_reader(event_tx);

    // Periodic redraw so time-driven UI (prompt countdown, module-error
    // expiry, streaming logs) updates even without input.
    let mut redraw = tokio::time::interval(Duration::from_millis(100));
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Unresponsive watchdog cadence (only runs when no input is pending).
    let mut watchdog_last = Instant::now();

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
                    // Credential entry just finished via the prompt subwindow.
                    // Launch only once the engine's `set_credentials`
                    // QueryResult confirms the save (pending_launch_confirmed)
                    // — a failed save never launches the module.
                    if state.pending_launch_confirmed {
                        if let Some(name) = state.pending_launch.take() {
                            state.pending_launch_confirmed = false;
                            if !supervisor.contains_key(&name) {
                                request_launch(
                                    state,
                                    &name,
                                    supervisor,
                                    &plugins,
                                    port,
                                    pin,
                                    supervisor::LaunchMode::Prebuilt,
                                );
                            }
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
            maybe_rebuild = rebuild_rx.recv() => {
                if let Some(name) = maybe_rebuild {
                    // A module crashed before connecting (e.g. a corrupt or
                    // mismatched prebuilt binary). Rebuild it from source and
                    // relaunch — but cap attempts so we don't loop forever.
                    let attempts = {
                        let mut m = state.rebuild_attempts.lock().unwrap();
                        let n = m.get(&name).copied().unwrap_or(0);
                        m.insert(name.clone(), n + 1);
                        n
                    };
                    if attempts < 3 {
                        supervisor_log(state, format!("[supervisor] {} crashed — rebuilding and relaunching (attempt {})", name, attempts + 1));
                        supervisor.remove(&name);
                        request_launch(
                            state,
                            &name,
                            supervisor,
                            &plugins,
                            port,
                            pin,
                            supervisor::LaunchMode::Rebuild,
                        );
                    } else {
                        supervisor_log(state, format!("[supervisor] {} crashed {}x — giving up", name, attempts + 1));
                    }
                }
            }
            maybe_restart = restart_rx.recv() => {
                if let Some(name) = maybe_restart {
                    handle_crash(
                        state,
                        &name,
                        supervisor,
                        &plugins,
                        port,
                        pin,
                    )
                    .await;
                }
            }
            maybe_launch = launch_rx.recv() => {
                if let Some((name, result)) = maybe_launch {
                    handle_launch_result(
                        state,
                        &name,
                        result,
                        supervisor,
                        &plugins,
                        &ws_command_tx,
                    )
                    .await;
                }
            }
            _ = redraw.tick() => {
                // Watchdog: recover locally-launched modules that the engine
                // flagged unresponsive (hung, but the process may still be
                // alive) or that dropped off the live-session list.
                if watchdog_last.elapsed() >= Duration::from_secs(1) {
                    watchdog_last = Instant::now();
                    let dead: Vec<String> = state
                        .module_runs
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(name, status)| {
                            status.as_str() == "connected" && supervisor.contains_key(*name)
                        })
                        .filter_map(|(name, _)| {
                            let entry = state
                                .stats
                                .module_entries
                                .iter()
                                .find(|e| &e.name == name);
                            match entry {
                                Some(e) if !e.alive || e.status != "connected" => Some(name.clone()),
                                _ => None,
                            }
                        })
                        .collect();
                    for name in dead {
                        supervisor_log(state, format!("[supervisor] {} unresponsive — restarting", name));
                        handle_crash(state, &name, supervisor, &plugins, port, pin).await;
                    }
                }
            }
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
                    supervisor_log(state, "[supervisor] credential entry timed out — module was not launched");
                    continue;
                }
                // TUI-local prompts (e.g. "disable autostart?") just expire.
                if prompt_is_local(&prompt_id) {
                    state.pending_prompt.retain(|p| p.prompt.prompt_id_uuid7 != prompt_id);
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
            let mut drained: Vec<crate::windows::log::LogEntry> = {
                let mut shared = state.module_logs.lock().unwrap();
                shared.drain(..).collect()
            };
            // Supervisor messages (start/stop/build/crash) go to the log window.
            if let Some(q) = crate::app::SUPERVISOR_LOGS.get() {
                drained.extend(q.lock().unwrap().drain(..));
            }
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
            // Detached pop-out windows run the same render loop over a single
            // window; give it the FULL main area (the one-row status bar stays
            // at the bottom) instead of its embedded layout sub-rect.
            let single = state.windows.len() == 1;

            for window in &mut state.windows {
                let (area, is_active) = if single {
                    (
                        Rect {
                            x: size.x,
                            y: size.y,
                            width: size.width,
                            height: size.height.saturating_sub(1),
                        },
                        true,
                    )
                } else {
                    match window.id() {
                        WindowId::Logo => (areas.logo, state.active_window == WindowId::Logo),
                        WindowId::Log => (areas.log, state.active_window == WindowId::Log),
                        WindowId::Modules => (areas.modules, state.active_window == WindowId::Modules),
                        WindowId::Chart => (areas.chart, state.active_window == WindowId::Chart),
                        WindowId::Prompts => (areas.prompts, state.active_window == WindowId::Prompts),
                        // Never embedded — belt-and-suspenders fallback.
                        WindowId::Users => (areas.modules, state.active_window == WindowId::Users),
                    }
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
        WsEvent::QueryResult { query_id, result } => {
            // Surface query failures (notably set_credentials) into the log
            // window instead of silently dropping them. On a successful
            // credential save, signal the main loop to launch the module; on
            // failure, clear any pending launch so it never fires.
            if query_id == "set_credentials" {
                if result.success {
                    // The main loop launches `pending_launch` only once this
                    // flag is set, so a failed save never launches the module.
                    state.pending_launch_confirmed = true;
                } else {
                    state.pending_launch = None;
                    state.pending_launch_confirmed = false;
                    let msg = if result.error.is_empty() {
                        "set_credentials failed (no error detail)".to_string()
                    } else {
                        format!("set_credentials failed: {}", result.error)
                    };
                    supervisor_log(state, msg);
                }
            }
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
            // A window in config-editor mode consumes every key.
            let hotkeys = state.hotkeys.clone();
            if let Some(window) = state.get_window_mut(state.active_window) {
                if window.in_editor() {
                    if window.editor_key(key, &hotkeys) {
                        if let Some(saved) = window.take_saved_module() {
                            let running = state
                                .module_runs
                                .lock()
                                .unwrap()
                                .get(&saved)
                                .map(|s| s == "connected" || s == "starting")
                                .unwrap_or(false);
                            if running {
                                supervisor_log(state, format!("[supervisor] {} config saved — restart the module (x) to apply", saved));
                            }
                        }
                        return Ok(false);
                    }
                }
            }

            // Prompts are answered in the dedicated prompts window (Tab to
            // focus it). While another window is focused, prompts wait in the
            // queue and the rest of the TUI stays fully navigable. Left/right
            // cycle the queue; y/n (or typed text + Enter) answers the focused
            // prompt.
            if state.active_window == WindowId::Prompts && !state.pending_prompt.is_empty() {
                if handle_prompt_key(state, key, plugins, supervisor, ws_command_tx) {
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

            // Ctrl+Shift + the module "start" key → force-rebuild and start the
            // selected module (overrides any prebuilt binary path).
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::SHIFT) {
                let base = KeyEvent::new(key.code, KeyModifiers::empty());
                let is_start = state
                    .hotkeys
                    .window_actions
                    .get(&window_name)
                    .and_then(|m| m.get(&base))
                    .map(|a| matches!(a, Action::StartModule(_)))
                    .unwrap_or(false);
                if is_start {
                    let name = selected_module_name(state);
                    if !name.is_empty() {
                        request_launch(
                            state,
                            &name,
                            supervisor,
                            plugins,
                            port,
                            pin,
                            supervisor::LaunchMode::Rebuild,
                        );
                        return Ok(false);
                    }
                }
            }

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
            // Paste into the active config editor first.
            if let Some(window) = state.get_window_mut(state.active_window) {
                if window.editor_paste(&text) {
                    return Ok(false);
                }
            }
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
                        // Only focus windows that actually exist (a detached
                        // child has a single window; the layout sub-rects of
                        // the other ids must not steal focus).
                        if state.windows.iter().any(|w| w.id() == window_id) {
                            state.active_window = window_id;
                        }
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
fn handle_prompt_key(
    state: &mut AppState,
    key: KeyEvent,
    plugins: &[crate::plugins::Plugin],
    supervisor: &mut supervisor::ProcessTable,
    ws_command_tx: &mpsc::UnboundedSender<WsCommand>,
) -> bool {
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
        } else if prompt_is_local(&prompt_id) {
            // TUI-local prompts are resolved here (no engine round-trip).
            if let Some(mod_name) = prompt_id.strip_prefix("tui-local:disable-autostart:") {
                // "Yes" disables autostart AND stops the crash loop — the
                // module stays dead instead of being relaunched over and over.
                if accepted {
                    set_autostart(plugins, mod_name, false);
                    if let Some(proc) = supervisor.remove(mod_name) {
                        let mut proc = proc.lock().unwrap();
                        supervisor_log(state, format!("[supervisor] Killing {} (pid {}) — crash loop stopped", mod_name, proc.pid()));
                        proc.kill();
                    }
                    state.module_runs.lock().unwrap().insert(mod_name.to_string(), "stopped".to_string());
                    supervisor_log(state, format!("[supervisor] {} disabled: autostart off + stopped (no more relaunches)", mod_name));
                } else {
                    supervisor_log(state, format!("[supervisor] {} left running (crash loop continues)", mod_name));
                }
            } else if let Some(mod_name) = prompt_id.strip_prefix("tui-local:clear-config:") {
                // On "yes" empty the module's `.env` + `config.json` values
                // (keys kept).
                if accepted {
                    if let Some(plugin) = plugins.iter().find(|p| p.manifest.name == mod_name) {
                        match supervisor::clear_module_config(&plugin.directory) {
                            Ok(()) => supervisor_log(
                                state,
                                format!("[supervisor] cleared all values from {}'s config (.env + config.json)", mod_name),
                            ),
                            Err(e) => supervisor_log(
                                state,
                                format!("[supervisor] failed to clear {}'s config: {}", mod_name, e),
                            ),
                        }
                    } else {
                        supervisor_log(state, format!("[supervisor] cannot clear {}'s config: module not found", mod_name));
                    }
                }
            } else if let Some(mod_name) = prompt_id.strip_prefix("tui-local:delete-module:") {
                // On "yes" kill + fully unregister the module (process,
                // modules.json, config.json ordering). Mirrors the clear-config
                // confirm flow — deleting is destructive and needs a confirm.
                if accepted {
                    delete_module(state, supervisor, &mod_name);
                }
            }
            remove_prompt_at(state, state.selected_prompt);
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
        // Launch only after the engine's QueryResult confirms the save (see
        // WsEvent::QueryResult) — a failed save must never launch the module.
        state.pending_launch = Some(module_name);
        state.pending_launch_confirmed = false;
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
            | Action::EditConfig(_)
            | Action::ClearModuleConfig(_)
            | Action::RunTests
            | Action::UserQuery(_, _)
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
        Action::EditConfig(_) => Action::EditConfig(name),
        Action::ClearModuleConfig(_) => Action::ClearModuleConfig(name),
        // PopOut carries its own window name when the binding set one (e.g.
        // `u` → PopOut("users")); an empty payload falls back to the current
        // window (the `w` per-window popout behavior).
        Action::PopOut(inner) => {
            if inner.is_empty() {
                Action::PopOut(window_name.to_string())
            } else {
                Action::PopOut(inner)
            }
        }
        other => other,
    }
}

/// When the engine reports a module as connected, promote any "starting" run
/// status to "connected".
fn sync_module_runs(state: &mut AppState) {
    // 1. Promote starting → connected once the engine reports the session.
    {
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

    // 2. Overlay TUI-side lifecycle status ("building"/"starting"/...): the
    // engine only reports offline/connected, so a module mid-build or
    // mid-restart would otherwise look "offline".
    {
        let runs = state.module_runs.lock().unwrap();
        for entry in &mut state.stats.module_entries {
            if let Some(cur) = runs.get(&entry.name) {
                match cur.as_str() {
                    "building" | "starting" | "restarting" | "crashed" | "stopped" => {
                        entry.status = cur.clone();
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Request a module launch: resolve the launch command (building if needed) on
/// a BACKGROUND task so a cold build never blocks the UI loop, then report the
/// outcome back through `launch_rx` where `handle_launch_result` spawns it.
/// Returns true when the request was accepted.
fn request_launch(
    state: &mut AppState,
    name: &str,
    supervisor: &mut supervisor::ProcessTable,
    plugins: &[crate::plugins::Plugin],
    port: u16,
    pin: u32,
    mode: supervisor::LaunchMode,
) -> bool {
    let Some(plugin) = plugins.iter().find(|p| p.manifest.name == name) else {
        return false;
    };
    if supervisor.contains_key(name) {
        return false;
    }
    // Don't stack a launch while one is already resolving.
    {
        let runs = state.module_runs.lock().unwrap();
        match runs.get(name).map(|s| s.as_str()) {
            Some("building") | Some("starting") | Some("restarting") => return false,
            _ => {}
        }
    }
    state
        .module_runs
        .lock()
        .unwrap()
        .insert(name.to_string(), "building".to_string());

    let Some(launch_tx) = state.launch_tx.clone() else {
        return false;
    };
    let plugin = plugin.clone();
    tokio::spawn(async move {
        let result = supervisor::resolve_launch(&plugin, port, pin, mode).await;
        let _ = launch_tx.send((plugin.manifest.name, result));
    });
    true
}

/// Handle a completed background launch: spawn the resolved command, wire up
/// the pipes + monitor, and set the run status. Runs on the main loop — the
/// slow part (build) already happened in the background task.
async fn handle_launch_result(
    state: &mut AppState,
    name: &str,
    result: Result<(String, Vec<String>), String>,
    supervisor: &mut supervisor::ProcessTable,
    plugins: &[crate::plugins::Plugin],
    ws_command_tx: &mpsc::UnboundedSender<WsCommand>,
) {
    // The module may have been stopped (or relaunched) while the build ran.
    {
        let runs = state.module_runs.lock().unwrap();
        if runs.get(name).map(|s| s.as_str()) == Some("stopped") {
            return;
        }
    }
    if result.is_err() {
        let e = result.unwrap_err();
        state
            .module_runs
            .lock()
            .unwrap()
            .insert(name.to_string(), "crashed".to_string());
        supervisor_log(state, format!("[supervisor] Failed to launch {}: {}", name, e));
        return;
    }
    let (cmd, args) = result.unwrap();
    let Some(plugin) = plugins.iter().find(|p| p.manifest.name == name) else {
        return;
    };
    if supervisor.contains_key(name) {
        return;
    }

    let spawned = if plugin.manifest.terminal {
        supervisor::spawn_terminal_from_parts(plugin, &cmd, &args)
    } else {
        supervisor::spawn_from_parts(plugin, &cmd, &args).map(|c| (c, None, None))
    };
    let (mut child, terminal_window, terminal_pidfile) = match spawned {
        Ok(pair) => pair,
        Err(e) => {
            state
                .module_runs
                .lock()
                .unwrap()
                .insert(name.to_string(), "crashed".to_string());
            supervisor_log(state, format!("[supervisor] Failed to spawn {}: {}", name, e));
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let pid = child.id();
    let proc = Arc::new(Mutex::new(supervisor::ManagedProcess {
        child,
        terminal_window,
        terminal_pidfile,
    }));
    supervisor.insert(name.to_string(), proc.clone());
    supervisor_log(
        state,
        format!("[supervisor] Launched {} (pid {}) — waiting to connect", name, pid),
    );
    state
        .module_runs
        .lock()
        .unwrap()
        .insert(name.to_string(), "starting".to_string());

    let restart_tx = state.restart_tx.clone().unwrap_or_else(|| {
        let (tx, _rx) = mpsc::unbounded_channel();
        tx
    });
    spawn_monitor(
        name.to_string(),
        proc,
        state.module_runs.clone(),
        plugin.manifest.terminal,
        state.rebuild_tx.clone().unwrap_or_else(|| {
            let (tx, _rx) = mpsc::unbounded_channel();
            tx
        }),
        restart_tx,
    );
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

/// Permanently remove a module: kill its process, drop its run state, remove it
/// from the engine's config.json ordering and from modules.json. Called after
/// the operator confirms the delete prompt.
fn delete_module(state: &mut AppState, supervisor: &mut supervisor::ProcessTable, name: &str) {
    if let Some(proc) = supervisor.remove(name) {
        let mut proc = proc.lock().unwrap();
        supervisor_log(state, format!("[supervisor] Killing {} (pid {})", name, proc.pid()));
        proc.kill();
    }
    state.module_runs.lock().unwrap().remove(name);
    supervisor::remove_from_ordering(name);
    // Also remove from modules.json via the engine registry file.
    if let Ok(data) = std::fs::read_to_string(supervisor::modules_registry_path()) {
        if let Ok(mut registry) = serde_json::from_str::<serde_json::Value>(&data) {
            if let Some(arr) = registry.as_array_mut() {
                arr.retain(|e| e.get("name").and_then(|v| v.as_str()) != Some(name));
                if let Ok(pretty) = serde_json::to_string_pretty(&registry) {
                    let _ = std::fs::write(supervisor::modules_registry_path(), pretty);
                }
            }
        }
    }
    supervisor_log(state, format!("[supervisor] deleted {}", name));
}

/// Crash-recovery ladder (unlimited retries, stability over everything):
/// 1. restart with the prebuilt binary — a system issue may be the cause
/// 2. a repeat crash within the 30-minute window escalates to a rebuild
/// 3. if the rebuild fails (or there's no source), roll back to the prebuilt
///    binary (a failed `cargo build` never clobbers the old binary)
/// 4. no binary provided → rebuild anyway
/// 5. after `CONSECUTIVE_CRASH_PROMPT` consecutive crashes, ask the operator
///    whether to disable autostart to save system resources.
async fn handle_crash(
    state: &mut AppState,
    name: &str,
    supervisor: &mut supervisor::ProcessTable,
    plugins: &[crate::plugins::Plugin],
    port: u16,
    pin: u32,
) {
    // Respect an explicit stop, and don't stack two recoveries for one module.
    {
        let runs = state.module_runs.lock().unwrap();
        match runs.get(name).map(|s| s.as_str()) {
            Some("stopped") | Some("restarting") => return,
            _ => {}
        }
    }

    // Crash bookkeeping: a module that survived the whole window resets its
    // history, so the next one-off crash goes back to prebuilt-first. A repeat
    // crash within the window escalates to a rebuild.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (rebuild_mode, consecutive) = {
        let mut cs = state.crash_state.lock().unwrap();
        cs.entry(name.to_string()).or_default().record_crash(now_ms)
    };

    {
        let mut runs = state.module_runs.lock().unwrap();
        runs.insert(name.to_string(), "restarting".to_string());
    }

    // Kill any leftover process before relaunching.
    if let Some(proc) = supervisor.remove(name) {
        let mut proc = proc.lock().unwrap();
        supervisor_log(state, format!("[supervisor] Killing {} (pid {})", name, proc.pid()));
        proc.kill();
    }

    // Backoff so a crash loop doesn't thrash the machine.
    tokio::time::sleep(crate::app::RESTART_BACKOFF).await;

    // Ladder (mode decided above; the rebuild→rollback fallback happens inside
    // resolve_launch). The launch itself runs in the background so a cold
    // rebuild doesn't freeze the UI.
    let mode = if rebuild_mode {
        supervisor_log(state, format!("[supervisor] {} crashed again — rebuilding from source", name));
        supervisor::LaunchMode::Rebuild
    } else {
        // Prebuilt first; the stale-binary check in module_run_parts will
        // rebuild anyway if the source is newer than the binary.
        supervisor::LaunchMode::Prebuilt
    };
    request_launch(state, name, supervisor, plugins, port, pin, mode);

    // Crash-loop prompt (no retry cap — just ask about autostart).
    if consecutive >= crate::app::CONSECUTIVE_CRASH_PROMPT {
        push_crash_prompt(state, name);
    }
}

/// Set a module's autostart flag in its manifest file on disk.
fn set_autostart(plugins: &[crate::plugins::Plugin], name: &str, enabled: bool) {
    if let Some(plugin) = plugins.iter().find(|p| p.manifest.name == name) {
        let manifest_path = plugin.directory.join(crate::plugins::MANIFEST_FILENAME);
        if let Ok(data) = std::fs::read_to_string(&manifest_path) {
            if let Ok(mut manifest) = serde_json::from_str::<serde_json::Value>(&data) {
                manifest["autostart"] = serde_json::json!(enabled);
                if let Ok(pretty) = serde_json::to_string_pretty(&manifest) {
                    let _ = std::fs::write(&manifest_path, pretty);
                }
            }
        }
    }
}

/// A module keeps crashing. Ask (in the prompts window, locally — no engine
/// round-trip) whether to disable autostart to save system resources.
fn push_crash_prompt(state: &mut AppState, name: &str) {
    let prompt_id = format!("tui-local:disable-autostart:{}", name);
    if state.pending_prompt.iter().any(|p| p.prompt.prompt_id_uuid7 == prompt_id) {
        return;
    }
    let prompt = cockatiel_client::proto::Prompt {
        prompt_id_uuid7: prompt_id,
        prompt: format!("Module {} keeps crashing", name),
        details: "It is being restarted automatically (no retry cap). Would you like to disable autostart to save system resources?".to_string(),
        yes_dialog: "Yes — disable autostart".to_string(),
        no_dialog: "Keep autostart".to_string(),
        timeout: 60,
        origin: "tui".to_string(),
        origin_uuid7: String::new(),
        instructions: String::new(),
        link: String::new(),
        input_label: String::new(),
        prompt_type: 0, // unspecified → Boolean (y/n)
    };
    state.pending_prompt.push_back(crate::app::PendingPrompt {
        deadline: Instant::now() + Duration::from_secs(60),
        prompt,
        text_input: String::new(),
    });
}

/// True when a prompt is a TUI-local prompt (resolved here, not the engine).
fn prompt_is_local(prompt_id: &str) -> bool {
    prompt_id.starts_with("tui-local:")
}

/// Read lines from a module's stdout/stderr pipe: show them in the TUI log
/// window, forward them to the engine so they persist in the timeline DB, and
/// record when a module emits an error so its status can show "error".
/// Route a supervisor message into the log window (instead of the terminal),
/// so starting/stopping modules never scribbles clear text over the ratatui
/// screen.
fn supervisor_log(_state: &AppState, message: impl Into<String>) {
    crate::app::supervisor_log_global(message.into());
}

/// Strip ANSI escape sequences and carriage returns from a line, so module /
/// cargo output (progress bars, colors) never corrupts the ratatui screen or
/// the log window.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    // CSI: consume params until the final byte (0x40..=0x7e).
                    chars.next();
                    for n in chars.by_ref() {
                        if (0x40..=0x7e).contains(&(n as u32)) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: consume until BEL or ST (ESC \).
                    chars.next();
                    for n in chars.by_ref() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    // Bare ESC + one char.
                    let _ = chars.next();
                }
            }
        } else if c == '\r' {
            continue;
        } else {
            out.push(c);
        }
    }
    out
}

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
            let line = strip_ansi(&line).trim().to_string();
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

/// Spawn a background monitor for a just-launched module. It polls the child
/// for the process's lifetime:
///  - exits while "starting" → startup crash → auto-rebuild from source
///  - exits while "connected" → runtime crash → the crash ladder restarts it
///  - exits while "restarting"/"stopped" → deliberate (the ladder is relaunching
///    it, or the user stopped it) → silent
/// A process that is still alive but hasn't connected yet is left as
/// "starting" — sync_module_runs promotes it to "connected" the moment the
/// engine reports it. For terminal modules the launcher process (e.g.
/// osascript) detaches immediately, so exit is never a crash signal.
fn spawn_monitor(
    name: String,
    proc: Arc<Mutex<supervisor::ManagedProcess>>,
    runs: Arc<Mutex<HashMap<String, String>>>,
    is_terminal: bool,
    rebuild_tx: mpsc::UnboundedSender<String>,
    restart_tx: mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        loop {
            // Crashed: the child process exited. For terminal modules the
            // launcher (e.g. osascript) detaches immediately, so instead we
            // probe the pidfile's real pid — if the module died before it ever
            // connected, treat it as a startup crash (recoverable), same as a
            // non-terminal module exiting while "starting".
            if is_terminal {
                let pidfile = proc.lock().unwrap().terminal_pidfile.clone();
                if let Some(pf) = pidfile {
                    if let Ok(pid_str) = std::fs::read_to_string(&pf) {
                        if let Ok(pid) = pid_str.trim().parse::<i32>() {
                            if !supervisor::pid_alive(pid) {
                                let status = runs
                                    .lock()
                                    .unwrap()
                                    .get(&name)
                                    .map(|s| s.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let startup = status == "starting";
                                let connected = status == "connected";
                                if startup {
                                    runs.lock().unwrap().insert(name.clone(), "crashed".to_string());
                                    // Died before the engine saw it connect —
                                    // an unexpected failure, rebuild + relaunch.
                                    let _ = rebuild_tx.send(name.clone());
                                } else if connected {
                                    runs.lock().unwrap().insert(name.clone(), "crashed".to_string());
                                    let _ = restart_tx.send(name.clone());
                                }
                                break;
                            }
                        }
                    }
                }
            } else {
                let mut guard = proc.lock().unwrap();
                match guard.child.try_wait() {
                    Ok(Some(_)) => {
                        let status = runs
                            .lock()
                            .unwrap()
                            .get(&name)
                            .map(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let startup = status == "starting";
                        let connected = status == "connected";
                        if startup {
                            let mut r = runs.lock().unwrap();
                            r.insert(name.clone(), "crashed".to_string());
                            drop(r);
                            // A startup crash (before the engine saw it connect)
                            // is an unexpected failure — rebuild from source.
                            let _ = rebuild_tx.send(name.clone());
                        } else if connected {
                            let mut r = runs.lock().unwrap();
                            r.insert(name.clone(), "crashed".to_string());
                            drop(r);
                            // Died while running — the ladder recovers it.
                            let _ = restart_tx.send(name.clone());
                        }
                        // deliberate (restarting/stopped) → silent
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
            // Stop polling a module the user stopped.
            {
                let r = runs.lock().unwrap();
                if r.get(&name).map(|s| s.as_str()) == Some("stopped") {
                    break;
                }
            }
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
                    request_launch(
                        state,
                        &name,
                        supervisor,
                        plugins,
                        port,
                        pin,
                        supervisor::LaunchMode::Prebuilt,
                    );
                }
            }
        }
        Action::StopModule(name) => {
            if let Some(proc) = supervisor.remove(&name) {
                let mut proc = proc.lock().unwrap();
                supervisor_log(state, format!("[supervisor] Killing {} (pid {})", name, proc.pid()));
                proc.kill();
            }
            state
                .module_runs
                .lock()
                .unwrap()
                .insert(name, "stopped".to_string());
        }
        Action::DeleteModule(name) => {
            // Confirm before destroying the module's registration.
            let prompt_id = format!("tui-local:delete-module:{}", name);
            if !state.pending_prompt.iter().any(|p| p.prompt.prompt_id_uuid7 == prompt_id) {
                let prompt = cockatiel_client::proto::Prompt {
                    prompt_id_uuid7: prompt_id,
                    prompt: format!("Delete {}?", name),
                    details: "This KILLS the module process and REMOVES it from modules.json + the engine's config.json ordering. The module will no longer be known to Cockatiel (you'd have to re-add it).".to_string(),
                    yes_dialog: "Yes — delete module".to_string(),
                    no_dialog: "Cancel".to_string(),
                    timeout: 60,
                    origin: "tui".to_string(),
                    origin_uuid7: String::new(),
                    instructions: String::new(),
                    link: String::new(),
                    input_label: String::new(),
                    prompt_type: 0, // unspecified → Boolean (y/n)
                };
                state.pending_prompt.push_back(crate::app::PendingPrompt {
                    deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
                    prompt,
                    text_input: String::new(),
                });
                state.active_window = WindowId::Prompts;
            }
            supervisor_log(state, format!("[supervisor] delete requested for {} — awaiting confirmation", name));
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
        Action::EditConfig(name) => {
            // Open the inline config editor (edits `.env` + `config.json`).
            if let Some(plugin) = plugins.iter().find(|p| p.manifest.name == name) {
                if let Some(window) = state.get_window_mut(WindowId::Modules) {
                    window.start_config_editor(&name, plugin.directory.clone());
                    state.active_window = WindowId::Modules;
                    supervisor_log(state, format!("[supervisor] editing config for {} (j/k move, type to edit, Esc save+exit)", name));
                }
            }
        }
        Action::ClearModuleConfig(name) => {
            // Ask the operator first (prompts window, locally — no engine
            // round-trip), then empty the module's config values.
            let prompt_id = format!("tui-local:clear-config:{}", name);
            if !state.pending_prompt.iter().any(|p| p.prompt.prompt_id_uuid7 == prompt_id) {
                let prompt = cockatiel_client::proto::Prompt {
                    prompt_id_uuid7: prompt_id,
                    prompt: format!("Clear {}'s config?", name),
                    details: "This will empty every value in the module's .env and config.json (keys and structure stay). You will need to re-enter credentials/settings before the module can run.".to_string(),
                    yes_dialog: "Yes — clear all values".to_string(),
                    no_dialog: "Cancel".to_string(),
                    timeout: 60,
                    origin: "tui".to_string(),
                    origin_uuid7: String::new(),
                    instructions: String::new(),
                    link: String::new(),
                    input_label: String::new(),
                    prompt_type: 0, // unspecified → Boolean (y/n)
                };
                state.pending_prompt.push_back(crate::app::PendingPrompt {
                    deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
                    prompt,
                    text_input: String::new(),
                });
                state.active_window = WindowId::Prompts;
            }
            supervisor_log(state, format!("[supervisor] clear config requested for {} — awaiting confirmation", name));
        }
        Action::RunTests => {
            // Run the compliance suite against the selected module.
            supervisor_log(state, "[supervisor] test run requested");
            if !state.connected {
                supervisor_log(state, "[supervisor] cannot run tests — engine disconnected");
                return Ok(false);
            }
            let selected_name = selected_module_name(state);
            let payload = serde_json::json!({
                "suite": "all",
                "module": selected_name,
                "iterations": 20,
            });
            send_engine_query(
                ws_command_tx,
                "test_run".to_string(),
                payload.to_string(),
            );
        }
        Action::UserQuery(query_id, sql) => {
            // One-shot user-database query from the detached users window.
            send_engine_query(ws_command_tx, query_id, sql);
        }
        _ => {}
    }
    Ok(false)
}

/// Send a one-shot query to the engine (via the WebSocket command channel).
fn send_engine_query(ws_command_tx: &mpsc::UnboundedSender<WsCommand>, query_id: String, sql: String) {
    let _ = ws_command_tx.send(WsCommand::SendQuery { query_id, sql });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_carriage_returns() {
        // CSI color sequence.
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        // OSC (hyperlink) sequence up to BEL.
        assert_eq!(strip_ansi("\u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{7}"), "link");
        // Cargo-style progress with \r and inline escapes.
        assert_eq!(strip_ansi("\u{1b}[2K\u{1b}[1GCompiling foo\rFinished"), "Compiling fooFinished");
        // Bare ESC.
        assert_eq!(strip_ansi("a\u{1b}Kb"), "ab");
    }

    #[test]
    fn user_query_dispatches_send_query() {
        let (tx, mut rx) = mpsc::unbounded_channel::<WsCommand>();
        send_engine_query(
            &tx,
            "userdb_get_user".to_string(),
            r#"{"uuid7":"u1"}"#.to_string(),
        );
        match rx.try_recv() {
            Ok(WsCommand::SendQuery { query_id, sql }) => {
                assert_eq!(query_id, "userdb_get_user");
                assert_eq!(sql, r#"{"uuid7":"u1"}"#);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn user_query_is_dispatchable() {
        assert!(is_dispatchable(&Action::UserQuery(
            "userdb_list_users".to_string(),
            String::new(),
        )));
    }

    #[test]
    fn fill_window_action_preserves_popout_payload() {
        let colors = crate::colors::load_colors(&std::path::PathBuf::from(""));
        let state = AppState::new(colors, crate::hotkeys::default_hotkeys());
        let a = fill_window_action(&state, "modules", Action::PopOut("users".to_string()));
        assert_eq!(a, Action::PopOut("users".to_string()));
        // An empty payload still pops out the current window (per-window `w`).
        let b = fill_window_action(&state, "modules", Action::PopOut(String::new()));
        assert_eq!(b, Action::PopOut("modules".to_string()));
    }
}
