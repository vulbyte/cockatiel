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
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
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
use hotkeys::{action_label, load_hotkeys, Action};
use windows::{LogoWindow, LogWindow, ModulesWindow, ChartWindow, PromptsWindow, UsersWindow};
use ws_client::{WsClient, WsCommand, WsEvent};
use ws_server::WsServer;

fn find_engine_addr() -> (String, u16, u32) {
    // Try config.json first
    for path in &["../config.json", "config.json"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) {
                // A `config.json` that names NONE of these is not this file. The
                // TUI keeps its own `config.json` in the same directory (for
                // `launch_engine`), and answering from it would return the
                // built-in defaults — the port the engine actually bound would
                // never be discovered, and the failure is silent because the
                // defaults look plausible.
                let has_addr_key = ["engine_ip", "engine_port", "engine_pin"]
                    .iter()
                    .any(|k| config.get(*k).is_some());
                if !has_addr_key {
                    continue;
                }
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

/// The command line, parsed.
///
/// Hand-rolled on purpose (no arg crate) and kept in the same shape the loop
/// always had: a flag that takes a value consumes the NEXT argument, a flag that
/// does not consumes only itself, and an unrecognised argument is stepped over
/// rather than treated as an error. `None` is meaningful throughout — it is how
/// "not given on the command line" is spelled, which is what lets a config
/// default decide. In particular there is no short flag for either engine
/// switch: `-p` is `--pin` and `-i` is `--ip`, and the long names are the ones
/// an operator reads in `--help`-less help.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CliArgs {
    pub detached_window: Option<String>,
    pub ws_parent_addr: Option<String>,
    pub ws_parent_token: Option<String>,
    pub override_ip: Option<String>,
    pub override_port: Option<u16>,
    pub override_pin: Option<u32>,
    /// `--with-engine` → `Some(true)`, `--no-engine` → `Some(false)`, neither →
    /// `None` (the config decides). The LAST one given wins, so
    /// `--no-engine --with-engine` means what it reads as.
    pub engine_launch: Option<bool>,
}

fn parse_cli(args: &[String]) -> CliArgs {
    let mut out = CliArgs::default();
    let mut i = 0;
    while i < args.len() {
        // A flag with a value: the value is the next argument, if there is one.
        // A trailing `--port` with nothing after it is stepped over rather than
        // panicking, exactly as before.
        let take = |i: &mut usize| -> Option<String> {
            match args.get(*i + 1) {
                Some(v) => {
                    *i += 2;
                    Some(v.clone())
                }
                None => {
                    *i += 1;
                    None
                }
            }
        };
        if args[i] == "--detached" {
            out.detached_window = take(&mut i);
        } else if args[i] == "--ws-addr" {
            out.ws_parent_addr = take(&mut i);
        } else if args[i] == "--ws-token" {
            out.ws_parent_token = take(&mut i);
        } else if args[i] == "--ip" || args[i] == "-i" {
            out.override_ip = take(&mut i);
        } else if args[i] == "--port" || args[i] == "-p" {
            out.override_port = take(&mut i).and_then(|v| v.parse().ok());
        } else if args[i] == "--pin" {
            out.override_pin = take(&mut i).and_then(|v| v.parse().ok());
        } else if args[i] == "--with-engine" {
            // No value: these two SWITCH, they are not `--flag value`.
            out.engine_launch = Some(true);
            i += 1;
        } else if args[i] == "--no-engine" {
            out.engine_launch = Some(false);
            i += 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Whether the TUI should LAUNCH the engine at startup.
///
/// The precedence is the whole contract: the flag wins (an operator who typed it
/// meant it), then the TUI's own `config.json`, then the built-in default.
/// The default is `true` and it is not negotiable by omission — starting
/// without the engine is the special case, and it is what the flag and the
/// config key are for.
///
/// Pure so the precedence is pinned by tests instead of only being observable by
/// starting the app and looking for a process.
pub fn should_launch_engine(flag: Option<bool>, config_default: Option<bool>) -> bool {
    flag.unwrap_or(config_default.unwrap_or(true))
}

/// The startup launch decision: launch only when we intend to AND the port is
/// not already answering.
///
/// Both inputs are needed. `already_up` alone would launch over an engine
/// somebody else is running (two engines, one port, the second crash-looping);
/// `should_launch` alone would fight a `--no-engine` operator by starting the
/// very process they said not to start.
pub fn engine_start_decision(already_up: bool, should_launch: bool) -> bool {
    should_launch && !already_up
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let cli = parse_cli(&args[1..]);
    let detached_window = cli.detached_window;
    let ws_parent_addr = cli.ws_parent_addr;
    let ws_parent_token = cli.ws_parent_token;
    let override_ip = cli.override_ip;
    let override_port = cli.override_port;
    let override_pin = cli.override_pin;

    let config_dir = std::env::current_dir().unwrap_or_default();
    // The TUI's own config, next to hotkey_config.json / color_config.json. The
    // writer runs first so a fresh install (an empty directory, or one whose
    // config.json predates a key) gets the default rather than silently
    // falling back to it.
    let tui_config_path = config_dir.join("config.json");
    supervisor::ensure_tui_config(&tui_config_path);
    let config_launch_engine = supervisor::read_launch_engine_default(&tui_config_path);
    // The flag beats the config, which beats the built-in default (launch the
    // engine). Resolved ONCE, here, so the flag's effect is a value the launch
    // site reads rather than a second launch site.
    let should_launch = should_launch_engine(cli.engine_launch, config_launch_engine);

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
        // Harden secrets left world-readable by older non-atomic writers before
        // anything reads or rewrites them.
        supervisor::remediate_secret_file_permissions();
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
        //
        // TWO independent questions, deliberately not collapsed: the probe is
        // about the PORT (is somebody's engine already listening?) and
        // `should_launch` is about INTENT (`--with-engine` / `--no-engine` /
        // the config default). So `--no-engine` does not stop the TUI from
        // CONNECTING to an engine someone else is running — it only stops this
        // process from starting one — and `--with-engine` still does not launch
        // a second engine over a live one.
        let engine_up = std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            std::time::Duration::from_millis(300),
        ).is_ok();
        // The gate is on THIS `if` and nothing else: the user database above and
        // the module registration below are unaffected by it. A TUI pointed at
        // somebody else's engine still needs its user database, and the engine
        // itself still needs the database and the modules.
        if engine_start_decision(engine_up, should_launch) {
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
                    // Wait for the engine to write its config (port/pin) AND its
                    // TLS cert, so plugins + the TUI itself can connect over WSS.
                    // Explicit CLI overrides still win over the fresh config.
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

        // Point the TUI's own engine connection (and any module it launches)
        // at the engine's TLS cert so everything speaks WSS. Set for this
        // process — the supervisor also sets it on each module's Command.
        if let Some(cert) = supervisor::engine_tls_cert_path() {
            std::env::set_var("COCKATIEL_TLS_CERT", cert);
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

    // A panic must never leave the operator's terminal stuck in raw mode /
    // alternate screen: restore it best-effort from a panic hook so even a
    // stack-unwinding crash (which skips the normal teardown below) gets a
    // usable terminal back. The restore calls are process-global and safe to
    // run even if raw mode was never entered (disable_raw_mode on a normal
    // terminal is a no-op).
    std::panic::set_hook(Box::new(|info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        let _ = std::io::stdout().flush();
        let _ = execute!(std::io::stdout(), Show);
        let payload = info
            .payload()
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("<non-string panic payload>");
        eprintln!("\n[Cockatiel] TUI panicked: {}", payload);
        if let Some(loc) = info.location() {
            eprintln!("[Cockatiel] at {}:{}:{}", loc.file(), loc.line(), loc.column());
        }
        eprintln!("[Cockatiel] run with RUST_BACKTRACE=1 for a backtrace");
    }));

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

    // Crash-ladder relaunch channel: `handle_crash` schedules the relaunch on a
    // background task (exponential backoff), which reports back here with the
    // module + launch mode when the backoff elapses — so the main loop never
    // blocks for the backoff.
    let (retry_tx, retry_rx) =
        mpsc::unbounded_channel::<(String, supervisor::LaunchMode)>();
    state.retry_tx = Some(retry_tx);

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
    // ONE switch, two ends: the app raises it when the operator removes the
    // engine, the client task reads it between connection attempts. Both
    // constructors make their own `Arc`, so handing the app's over here is what
    // makes them the same flag — without this the removal would clear the TUI's
    // state and the client would keep dialling, which is the orphan the whole
    // mechanism exists to prevent.
    ws_client.stopped = state.engine_detached.clone();
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
        retry_rx,
        launch_rx,
    ).await;

    // Tear down everything the TUI owns before exiting.
    for (name, proc) in supervisor.drain() {
        let mut proc = proc.lock().unwrap();
        eprintln!("[supervisor] Killing {} (pid {})", name, proc.pid());
        proc.kill();
    }

    // Close any pop-out windows BEFORE releasing the terminal: each detached
    // window is its own process talking to this one, so leaving them open means
    // they immediately start failing to reconnect (and the ws client now exits
    // on its own, but the window should not linger regardless).
    {
        let popouts: Vec<String> = state.popped_out.iter().cloned().collect();
        if !popouts.is_empty() {
            crate::supervisor::close_popout_windows(&popouts);
        }
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
    mut plugins: Vec<crate::plugins::Plugin>,
    port: u16,
    pin: u32,
    mut rebuild_rx: mpsc::UnboundedReceiver<String>,
    mut restart_rx: mpsc::UnboundedReceiver<String>,
    mut retry_rx: mpsc::UnboundedReceiver<(String, supervisor::LaunchMode)>,
    mut launch_rx: mpsc::UnboundedReceiver<(String, Result<(String, Vec<String>), String>)>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Input events arrive instantly from a background crossterm reader thread.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AppEvent>();
    event::spawn_event_reader(event_tx);

    // Fingerprint of the last drawn frame's shape; a change forces a full
    // repaint instead of a diff. `None` until the first frame is drawn.
    let mut last_shape: Option<crate::app::ScreenShape> = None;

    // External SIGTERM/SIGINT (e.g. `kill`): quit through the normal path so
    // main() runs the full supervisor + terminal teardown (graceful group
    // TERM→KILL of the engine/user-db → WAL flush, disable_raw_mode,
    // LeaveAlternateScreen). Ctrl+C never reaches this handler while raw mode
    // is on (ISIG is off — crossterm delivers it as an ignored key event), so
    // copy/paste behavior is unchanged.
    let (signal_quit_tx, mut signal_quit_rx) = mpsc::unbounded_channel::<()>();
    spawn_signal_quit_task(signal_quit_tx);

    // Periodic redraw so time-driven UI (prompt countdown, module-error
    // expiry, streaming logs, the PAUSED indicator's flash) updates even
    // without input.
    let mut redraw = tokio::time::interval(Duration::from_millis(100));
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Blink phase counter for the PAUSED indicator. Advanced here — on the
    // DRAW ticker, not the 2s engine poll — because a phase that only moved
    // when db_status arrived would be a 2s stutter, not a flash. It rides the
    // repaint tick: every tick ends in a `terminal.draw`, and ratatui's diff
    // emits exactly the cells whose glyphs changed, so the phase flip needs
    // neither a full repaint nor a ScreenShape change (see `AppState::
    // pause_flash_on`). Held locally so an inbound StatsUpdate (which replaces
    // the whole stats struct) cannot rewind the phase mid-blink.
    let mut pause_flash_tick: u64 = 0;

    // Autostart: fires ONCE on the first engine connect, launching autostart-tagged
    // modules (the `A` marker) and resuming the paused pipeline. A reconnect
    // does not re-fire it.
    let mut auto_start_done = false;

    // Unresponsive watchdog cadence (only runs when no input is pending).
    let mut watchdog_last = Instant::now();
    // Modules observed looking-dead once; they must look dead on TWO consecutive
    // cycles (1s apart) before the watchdog restarts them, so a transient
    // liveness blip can't kill a healthy module.
    let mut watchdog_suspects: std::collections::HashMap<String, ()> =
        std::collections::HashMap::new();

    loop {
        tokio::select! {
            _ = signal_quit_rx.recv() => {
                // SIGTERM/SIGINT received — return so main() runs the normal
                // teardown (supervisor group-kill + terminal restore).
                return Ok(());
            }
            maybe = event_rx.recv() => {
                if let Some(ev) = maybe {
                    if handle_input_event(
                        ev,
                        terminal,
                        state,
                        supervisor,
                        &mut plugins,
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
                    // Autostart modules (the `A` marker) always launch on the
                    // first engine connect — no toggle. The guard is `done`,
                    // not a setting: fire once per TUI session.
                    if matches!(ev, WsEvent::Connected) && !auto_start_done {
                        auto_start_done = true;
                        auto_start_once(
                            state,
                            supervisor,
                            &plugins,
                            port,
                            pin,
                            &ws_command_tx,
                        ).await;
                    }
                    handle_ws_event(ev, state, &ws_command_tx, &ws_broadcast_tx);
                    while let Ok(ev) = ws_event_rx.try_recv() {
                        handle_ws_event(ev, state, &ws_command_tx, &ws_broadcast_tx);
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
                        // Kill the crashed process before removing it from the
                        // table — `remove` alone would orphan it (untracked,
                        // never killed on teardown).
                        if let Some(proc) = supervisor.remove(&name) {
                            let mut proc = proc.lock().unwrap();
                            proc.kill();
                        }
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
                    )
                    .await;
                }
            }
            maybe_retry = retry_rx.recv() => {
                // A crash-ladder backoff elapsed: relaunch the module. Skip if
                // the operator changed its state meanwhile (stopped it, or
                // manually started it — the manual start transitioned the
                // status to "building"/"starting", so this fires and no-ops).
                if let Some((name, mode)) = maybe_retry {
                    let still_pending = {
                        let runs = state.module_runs.lock().unwrap();
                        runs.get(&name).map(|s| s.as_str()) == Some("restarting")
                    };
                    if still_pending {
                        supervisor_log(state, format!("[supervisor] relaunching {} after crash backoff", name));
                        request_launch(state, &name, supervisor, &plugins, port, pin, mode);
                    }
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
                // Blink phase: advance before anything below can `continue`
                // past the draw, so the flash keeps its cadence even while the
                // watchdog is skipping disconnected frames.
                state.stats.pause_flash_tick = pause_flash_tick;
                pause_flash_tick = pause_flash_tick.wrapping_add(1);
                // A shutdown request the engine never answered must not leave
                // the removal half-done (asked, never resolved, and the engine
                // still there). A broken socket usually shows up as a
                // `Disconnected` instead, which finishes the removal itself —
                // this is the belt to that braces, and it is checked BEFORE the
                // watchdog's `connected` gate because that gate `continue`s
                // exactly when the engine is gone.
                if removal_deadline_passed(state.pending_engine_removal, Instant::now()) {
                    finish_engine_removal(
                        state,
                        &ws_command_tx,
                        EngineRemovalOutcome::Denied(format!(
                            "no answer within {}s",
                            ENGINE_SHUTDOWN_TIMEOUT.as_secs()
                        )),
                    );
                }
                // Watchdog: recover locally-launched modules that the engine
                // flagged unresponsive (hung, but the process may still be
                // alive) or that dropped off the live-session list. Two-strike rule + engine
                // connectivity gate: a transient liveness blip or a stale
                // module_list snapshot (engine disconnected / poll hiccup) must
                // never kill a healthy module.
                if watchdog_last.elapsed() >= Duration::from_secs(1) {
                    watchdog_last = Instant::now();
                    if !state.connected {
                        watchdog_suspects.clear();
                        continue;
                    }
                    let mut looks_dead: Vec<String> = Vec::new();
                    {
                        let runs = state.module_runs.lock().unwrap();
                        for (name, status) in runs.iter() {
                            if status.as_str() != "connected" || !supervisor.contains_key(name) {
                                continue;
                            }
                            let entry = state
                                .stats
                                .module_entries
                                .iter()
                                .find(|e| &e.name == name);
                            match entry {
                                Some(e) if !e.alive || e.status != "connected" => {
                                    looks_dead.push(name.clone());
                                }
                                _ => {
                                    watchdog_suspects.remove(name);
                                }
                            }
                        }
                    }
                    let dead: Vec<String> = looks_dead
                        .into_iter()
                        .filter(|name| {
                            if watchdog_suspects.insert(name.clone(), ()).is_some() {
                                supervisor_log(state, format!("[supervisor] {} looks dead — confirming next cycle", name));
                                false
                            } else {
                                true
                            }
                        })
                        .collect();
                    for name in dead {
                        watchdog_suspects.remove(&name);
                        supervisor_log(state, format!("[supervisor] {} unresponsive — restarting", name));
                        handle_crash(state, &name, supervisor).await;
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

        // A window that moved or resized can leave stale cells behind, because
        // ratatui only repaints what differs from the previous frame. When the
        // screen's SHAPE changes -- a layout drag, entering/leaving the config
        // editor, a focus change, a resize -- clear the terminal first so the
        // frame is a full repaint. `force_full_redraw` is the manual Ctrl+L
        // escape hatch for any glitch this does not anticipate.
        let terminal_size = terminal.size()?;
        let (term_w, term_h) = (terminal_size.width, terminal_size.height);
        let shape = state.screen_shape(term_w, term_h);
        if state.needs_full_repaint(shape, last_shape) {
            let _ = terminal.clear();
            state.force_full_redraw = false;
        }
        last_shape = Some(shape);

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

/// On SIGTERM/SIGINT (an external `kill`), nudge the event loop to quit so
/// main() runs the full supervisor + terminal teardown — the same restore the
/// double-Esc quit path uses. The terminal is NOT restored here: restoring
/// first and then letting the loop draw once more would scribble ratatui
/// escape codes onto the restored terminal; the teardown in main() does the
/// restore immediately after run_app returns.
fn spawn_signal_quit_task(signal_quit_tx: mpsc::UnboundedSender<()>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
            let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
            tokio::select! {
                _ = sigterm.recv() => {}
                _ = sigint.recv() => {}
            }
            let _ = signal_quit_tx.send(());
        }
        #[cfg(not(unix))]
        {
            // No POSIX signals here — hold the sender so the loop's recv()
            // never sees a closed channel.
            let _ = signal_quit_tx;
            std::future::pending::<()>().await;
        }
    });
}

/// Handle an engine (WebSocket) event. Broadcasts to sub-windows, updates the
/// log/stats/prompt state.
fn handle_ws_event(
    event: WsEvent,
    state: &mut AppState,
    ws_command_tx: &mpsc::UnboundedSender<WsCommand>,
    ws_broadcast_tx: &broadcast::Sender<WsEvent>,
) {
    // Once the TUI has forgotten its engine, NOTHING from the old connection
    // may change anything — not the log (it would keep growing from a dead
    // engine), not a stats update (a whole `GlobalStats` arrives and would
    // replace the cleared one wholesale), and not a late `Disconnected` (which
    // would put a connection back on the engine row). The switch is raised
    // AFTER the shutdown answer is read, so the answer itself still gets here.
    if state.engine_forgotten() {
        return;
    }
    let _ = ws_broadcast_tx.send(event.clone());
    match event {
        WsEvent::Connected => {
            state.connected = true;
            state.stats.engine_status = "connected".to_string();
        }
        WsEvent::Disconnected => {
            // A shutdown request the engine never answered: its socket went
            // first. Finish the removal rather than leaving it half-done with
            // nothing left to finish it.
            if state.pending_engine_removal.take().is_some() {
                finish_engine_removal(
                    state,
                    ws_command_tx,
                    EngineRemovalOutcome::Denied(
                        "the connection closed before the engine answered".to_string(),
                    ),
                );
                return;
            }
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
            if query_id == "pipeline_set_paused" {
                // The gate's own answer, in the log window: how many messages
                // are being HELD / RELEASED, and whether this press actually
                // moved the gate or was a no-op repeat.
                if let Some(note) = db::pipeline_pause_note(&result) {
                    supervisor_log(state, note);
                }
            }
            // The engine's answer to "shut down" — the second step of removing
            // it, and the only step that can finish the removal. The request is
            // cleared first so a duplicate/late answer cannot run it twice.
            if query_id == ENGINE_SHUTDOWN_QUERY && state.pending_engine_removal.take().is_some() {
                let outcome = classify_engine_shutdown(&result);
                finish_engine_removal(state, ws_command_tx, outcome);
            }
        }
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
    plugins: &mut Vec<crate::plugins::Plugin>,
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
                        // Both post-save answers are read INSIDE the window
                        // borrow and logged outside it: the log needs
                        // `&AppState` while the window holds `&mut`, and the
                        // module warning's "is it running" check needs
                        // `&AppState.module_runs`.
                        let saved_module = window.take_saved_module();
                        let saved_engine = window.take_saved_engine();
                        if let Some(saved) = saved_module {
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
                        // The engine mirror of the module warning. It is a
                        // LIST rather than a flag because most of what the
                        // editor can change is re-read by the running engine
                        // anyway: only the boot-bound settings need a restart,
                        // and naming them (with the reason) is the point.
                        // The key is named too: pointing at a restart that does
                        // not exist is how a warning becomes wallpaper, and
                        // there is now an action to point at.
                        if let Some(notes) = saved_engine {
                            if notes.is_empty() {
                                supervisor_log(state, "[supervisor] engine config saved — the running engine re-reads every change");
                            } else {
                                let list = notes
                                    .iter()
                                    .map(|n| format!("{} ({})", n.key, n.why))
                                    .collect::<Vec<_>>()
                                    .join("; ");
                                supervisor_log(state, format!(
                                    "[supervisor] engine config saved — restart the engine (R on the engine row) to apply: {}",
                                    list
                                ));
                            }
                        }
                        return Ok(false);
                    }
                }
            }

            // Ctrl+L: force a full repaint on the next frame. The automatic
            // shape-change repaint covers layout/editor/focus transitions, so
            // this is the manual escape hatch for anything it misses.
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l') {
                state.force_full_redraw = true;
                return Ok(false);
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

            // A global (nav) binding: `q` quits, focus moves are handled in
            // place, and anything else dispatchable (the pipeline pause
            // toggle) goes through the same path a window action takes — a
            // global binding has to work from every focused window.
            if let Some(action) = state.handle_global_key(key) {
                if let Action::Quit = action {
                    return Ok(true);
                }
                if is_dispatchable(&action) {
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
                // Same rule as the plain start key: with the engine row
                // selected there is no module to rebuild and launch.
                if is_start && focused_selection_is_module(state) {
                    let name = selected_module_name(state);
                    if !name.is_empty() {
                        request_launch(
                            state,
                            &name,
                            supervisor,
                            &*plugins,
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
                    // A per-module action on a non-module row is refused
                    // loudly. `start`/`stop`/`del`/`auto`/`copy`/`clear`/
                    // `creds`/`test` all resolve to a module name, and the
                    // fallback for "no name here" is the FIRST module, not
                    // nothing — which is how an operator ends up killing a
                    // module they were not looking at.
                    if is_module_scoped(&action) && !focused_selection_is_module(state) {
                        supervisor_log(
                            state,
                            format!(
                                "[supervisor] {} does not apply to the engine — select a module row first",
                                action_label(&action)
                            ),
                        );
                        return Ok(false);
                    }
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
            | Action::DuplicateModule(_)
            | Action::EditCredentials(_)
            | Action::EditConfig(_)
            | Action::ClearModuleConfig(_)
            | Action::RunTests
            | Action::UserQuery(_, _)
            | Action::TogglePipelinePause
            | Action::RemoveEngine
            | Action::RestartEngine
            | Action::MoveModuleStage(_, _)
    )
}

/// Actions that only make sense against a MODULE row, and so are refused on the
/// engine row.
///
/// `StartModule` and `StopModule` are deliberately NOT in this list: the engine
/// row reuses the standard `s`/`x` start/stop keys to launch/kill the ENGINE
/// process (the dispatcher redirects them when the engine row is selected). The
/// rest resolve to a module name with no engine meaning, so they are refused
/// there rather than falling through — because the app's name resolution ends
/// at "the first known module" when no window claims a selection, and an
/// unguarded `x` with the engine row selected would otherwise kill a module the
/// operator never selected.
///
/// `EditConfig` is deliberately NOT in this list either: it is the one
/// per-module key that also means something on the engine row, where it opens
/// the ENGINE's `.env` + `config.json` instead. `TogglePipelinePause` is
/// engine-scoped rather than module-scoped and is global anyway, so it is never
/// gated.
fn is_module_scoped(action: &Action) -> bool {
    matches!(
        action,
        Action::DeleteModule(_)
            | Action::ToggleAutostart(_)
            | Action::DuplicateModule(_)
            | Action::ClearModuleConfig(_)
            | Action::EditCredentials(_)
            | Action::RunTests
            | Action::MoveModuleStage(_, _)
    )
}

/// Whether the focused window's selection is a module, i.e. whether a
/// per-module action is meaningful right now.
///
/// Windows that do not track a module selection (the log, the chart, ...) keep
/// the historical behaviour of acting on the first known module: the default
/// is `true`, and only a window whose selection is deliberately something else
/// opts out.
fn focused_selection_is_module(state: &AppState) -> bool {
    match state.windows.iter().find(|w| w.id() == state.active_window) {
        Some(w) => w.selection_is_module(&state.stats),
        None => true,
    }
}

/// Actions that only make sense on the ENGINE row of the modules window — the
/// mirror of [`is_module_scoped`], and the reason the engine row is a row at
/// all: it is the one place the engine itself can be acted on.
///
/// The default is `false` (not a module → not the engine), which is what stops
/// either of these firing from a window that has no row concept at all, where
/// the app's fallback would be "the first known module".
fn is_engine_scoped(action: &Action) -> bool {
    matches!(action, Action::RemoveEngine | Action::RestartEngine)
}

/// Whether the focused window's selection is the engine row.
fn focused_selection_is_engine(state: &AppState) -> bool {
    match state.windows.iter().find(|w| w.id() == state.active_window) {
        Some(w) => w.selection_is_engine(&state.stats),
        None => false,
    }
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
        Action::DuplicateModule(_) => Action::DuplicateModule(name),
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
    // Don't stack a launch while one is already resolving. "restarting" is NOT
    // excluded: it means a crash-ladder backoff is pending, and an explicit
    // operator start (or the backoff firing) should be allowed to launch now —
    // the pending retry handler skips if the status has moved past "restarting".
    {
        let runs = state.module_runs.lock().unwrap();
        match runs.get(name).map(|s| s.as_str()) {
            Some("building") | Some("starting") => return false,
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

/// The modules whose manifest has `autostart: true`. `autostart` is not a
/// parsed `ModuleManifest` field — it is read from the raw manifest JSON (the
/// same way `ToggleAutostart` flips it), so a module missing the flag or whose
/// manifest cannot be read is treated as not-autostart.
fn autostart_module_names(plugins: &[crate::plugins::Plugin]) -> Vec<String> {
    plugins
        .iter()
        .filter(|p| {
            let path = p.directory.join(crate::plugins::MANIFEST_FILENAME);
            std::fs::read_to_string(path)
                .ok()
                .and_then(|data| serde_json::from_str::<serde_json::Value>(&data).ok())
                .and_then(|m| m.get("autostart").and_then(|v| v.as_bool()))
                .unwrap_or(false)
        })
        .map(|p| p.manifest.name.clone())
        .collect()
}

/// One-click start: on the first engine connect, launch every module whose
/// manifest has `autostart: true`, then resume the pipeline (the engine boots
/// paused, so without this nothing would dispatch until the operator pressed
/// `p`). Fires exactly once per TUI session.
async fn auto_start_once(
    state: &mut AppState,
    supervisor: &mut supervisor::ProcessTable,
    plugins: &[crate::plugins::Plugin],
    port: u16,
    pin: u32,
    ws_command_tx: &mpsc::UnboundedSender<WsCommand>,
) {
    let autostart = autostart_module_names(plugins);
    if !autostart.is_empty() {
        supervisor_log(
            state,
            format!("[supervisor] autostart: launching {} module(s)", autostart.len()),
        );
        for name in &autostart {
            // Skip already-running and already-starting modules; each launch is
            // async (build can take a while) and reports back via launch_rx.
            request_launch(state, name, supervisor, plugins, port, pin, supervisor::LaunchMode::Prebuilt);
        }
    }
    // Resume the engine pipeline so the freshly-started stack actually
    // dispatches chat. The engine boots paused to give modules time to connect;
    // at this point they are launching, so open the gate. Idempotent: if the
    // pipeline was never paused this is a `changed: false` no-op.
    send_engine_query(
        ws_command_tx,
        "pipeline_set_paused".to_string(),
        serde_json::json!({ "paused": false }).to_string(),
    );
    supervisor_log(state, "[supervisor] auto_start: pipeline resumed");
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
                    let _ = supervisor::write_atomic_0600(&supervisor::modules_registry_path(), &pretty);
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

    // Ladder mode (decided above; the rebuild→rollback fallback happens inside
    // resolve_launch). The launch itself runs in the background so a cold
    // rebuild doesn't freeze the UI.
    let mode = if rebuild_mode {
        supervisor_log(state, format!("[supervisor] {} crashed again — rebuilding from source", name));
        supervisor::LaunchMode::Rebuild
    } else {
        supervisor::LaunchMode::Prebuilt
    };

    // Crash-loop prompt (no retry cap — just ask about autostart).
    if consecutive >= crate::app::CONSECUTIVE_CRASH_PROMPT {
        push_crash_prompt(state, name);
    }

    // Exponential backoff (1s, 2s, 4s ... capped at 10 min) so a crash loop
    // doesn't thrash the machine. The sleep runs on a background task — never
    // the main loop — and the retry channel's handler relaunches when it fires.
    let backoff = crate::app::crash_backoff(consecutive);
    supervisor_log(
        state,
        format!("[supervisor] {} crashed — relaunching in {}s (backoff)", name, backoff.as_secs()),
    );
    let Some(retry_tx) = state.retry_tx.clone() else {
        supervisor_log(state, format!("[supervisor] {} crashed — no retry channel; left stopped", name));
        return;
    };
    let name = name.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(backoff).await;
        let _ = retry_tx.send((name, mode));
    });
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
    plugins: &mut Vec<crate::plugins::Plugin>,
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
            // A detached window is a full-screen TUI of its own, so it must get
            // its OWN terminal. Spawning it with inherited stdio gave it the
            // parent's tty, putting two ratatui renderers on one screen.
            let argv: Vec<String> = vec![
                exe.to_string_lossy().to_string(),
                "--detached".to_string(),
                window_name.clone(),
                "--ws-addr".to_string(),
                ws_addr.to_string(),
                "--ws-token".to_string(),
                ws_auth_token.to_string(),
            ];
            let title = format!("popout:{}", window_name);
            if let Err(e) = crate::supervisor::spawn_in_new_terminal(&argv, &title) {
                // Fall back to nothing rather than corrupting the parent: an
                // inherited-tty spawn would scribble over the live UI.
                crate::app::supervisor_log_global(format!(
                    "[supervisor] could not open a window for '{}': {}",
                    window_name, e
                ));
            }
        }
        Action::StartModule(name) => {
            // The standard start key (`s`) means something different on the
            // engine row: it launches the ENGINE process rather than a module.
            // The redirect is checked here, not in `is_module_scoped`, so the
            // "first known module" fallback never eats the engine case.
            if focused_selection_is_engine(state) {
                let outcome = start_engine(
                    state,
                    supervisor,
                    supervisor::launch_engine,
                    || {
                        std::net::TcpStream::connect_timeout(
                            &format!("127.0.0.1:{}", port).parse().unwrap(),
                            std::time::Duration::from_millis(300),
                        )
                        .is_ok()
                    },
                );
                // "Start" means "make it run". A running-but-paused engine is
                // exactly the trap of seeing "already listening" while nothing
                // moves, so a successful start re-opens the pipeline. The
                // resume is idempotent: if it was never paused this is a
                // `changed: false` no-op, so sending it unconditionally on a
                // resumed outcome is safe even when the local belief is stale.
                if outcome.resume_pipeline() {
                    send_engine_query(
                        ws_command_tx,
                        "pipeline_set_paused".to_string(),
                        serde_json::json!({ "paused": false }).to_string(),
                    );
                }
                supervisor_log(state, engine_start_note(&outcome));
                return Ok(false);
            }
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
            // The standard stop key (`x`), on the engine row, kills the ENGINE
            // process. Same redirect rationale as `StartModule` above.
            if focused_selection_is_engine(state) {
                let outcome = stop_engine(state, supervisor);
                supervisor_log(state, engine_stop_note(&outcome));
                return Ok(false);
            }
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
        Action::DuplicateModule(name) => {
            // Copy the selected module under a NEW name: the engine assigns a
            // fresh instance UUID on connect and it runs as its own process.
            // It shares the original's binary + config (same directory).
            let Some(plugin) = plugins.iter().find(|p| p.manifest.name == name).cloned() else {
                supervisor_log(state, format!("[supervisor] cannot copy '{}' — not a discovered module", name));
                return Ok(false);
            };
            let new_name = {
                let mut n = 1u32;
                loop {
                    let candidate = format!("{}-{}", name, n);
                    let taken = plugins.iter().any(|p| p.manifest.name == candidate)
                        || state.stats.module_entries.iter().any(|m| m.name == candidate);
                    if !taken {
                        break candidate;
                    }
                    n += 1;
                }
            };
            // Register it (auto-approved — it's a copy of a trusted module) and
            // add it to the pipeline ordering so the engine routes to it.
            supervisor::register_module_approved(&new_name, &plugin.manifest.capabilities, 100);
            supervisor::add_to_ordering(&new_name, &plugin.manifest.capabilities, 100);
            let mut manifest = plugin.manifest.clone();
            manifest.name = new_name.clone();
            plugins.push(crate::plugins::Plugin {
                manifest,
                directory: plugin.directory.clone(),
            });
            supervisor_log(
                state,
                format!("[supervisor] copied {} → {} (new instance UUID + own process)", name, new_name),
            );
            request_launch(state, &new_name, supervisor, &*plugins, port, pin, supervisor::LaunchMode::Prebuilt);
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
            // The EDIT key against the SELECTED row, and the two rows are
            // different things: a module row opens that module's `.env` +
            // `config.json`, the engine row opens the ENGINE's. Same editor
            // both times — it already masks `.env`, which is where the engine
            // keeps its PIN, and already writes both files back. The modules
            // window answers for the engine because the engine is not a plugin,
            // so nothing else can; a module row defers to the plugin manifest,
            // which is where a module's directory lives.
            let stats = std::mem::take(&mut state.stats);
            let target = state
                .get_window_mut(WindowId::Modules)
                .and_then(|w| w.config_editor_target(&stats));
            state.stats = stats;
            let target = target.or_else(|| {
                plugins
                    .iter()
                    .find(|p| p.manifest.name == name)
                    .map(|p| (crate::app::ConfigTarget::Module, name.clone(), p.directory.clone()))
            });
            match target {
                Some((kind, label, dir)) => {
                    if let Some(window) = state.get_window_mut(WindowId::Modules) {
                        window.start_config_editor(kind, &label, dir);
                        state.active_window = WindowId::Modules;
                        supervisor_log(state, format!("[supervisor] editing config for {} (j/k move, type to edit, Esc save+exit)", label));
                    }
                }
                None => supervisor_log(state, format!("[supervisor] cannot edit config for '{}' — not a discovered module", name)),
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
        Action::TogglePipelinePause => {
            // The gate lives in the engine, so an unreachable engine is the one
            // case where the toggle cannot be honoured — say so instead of
            // firing a query into a dead socket.
            if !state.connected {
                supervisor_log(state, "[supervisor] cannot toggle the pipeline pause — engine disconnected");
                return Ok(false);
            }
            send_engine_query(
                ws_command_tx,
                "pipeline_set_paused".to_string(),
                pipeline_pause_toggle_sql(state.stats.pipeline_paused),
            );
        }
        // The two engine-only actions, guarded HERE rather than at the key
        // site, so no path into dispatch can skip the check. `is_engine_scoped`
        // keeps the list and the guard in one place; the arm before them turns
        // a press on the wrong row into a sentence instead of an action on
        // something the operator never selected.
        a if is_engine_scoped(&a) && !focused_selection_is_engine(state) => {
            supervisor_log(
                state,
                format!(
                    "[supervisor] {} only applies to the engine row — select it first",
                    action_label(&a)
                ),
            );
        }
        Action::RemoveEngine => {
            begin_engine_removal(state, ws_command_tx);
        }
        Action::RestartEngine => {
            // The launcher is the same function the startup launch site uses;
            // `restart_engine` takes it as an argument only so the sequence is
            // testable without an engine binary.
            let outcome = restart_engine(state, supervisor, supervisor::launch_engine);
            supervisor_log(state, restart_note(&outcome));
        }
        Action::MoveModuleStage(name, direction) => {
            // Shift+arrow in the modules window: rewrite the module's position
            // in the engine's config.json ordering lists AND move it in the
            // TUI's view so the row jumps to its new group immediately. The
            // engine's config-poll task re-reads the ordering lists on change,
            // so a running engine picks the move up live. The window sends only
            // the DIRECTION plus the module's name; the resolver here reads the
            // module's current stage and the engine's chain order and decides
            // what the move actually is (a stage jump, or an in-process
            // reorder), because the window has no access to the config.
            let Some(from) = state
                .stats
                .module_entries
                .iter()
                .find(|m| m.name == name)
                .map(|m| m.position.clone())
            else {
                return Ok(false);
            };
            if from == "input" {
                return Ok(false);
            }
            match supervisor::move_module_by_direction(None, &name, &from, direction) {
                Ok(Some(new_pos)) => {
                    // Re-apply the config's authoritative ordering + positions
                    // locally so the moved row jumps to its new spot NOW — the
                    // engine's next module_list poll reports the same order, so
                    // the view and the poll agree and nothing flickers back.
                    supervisor::reorder_module_entries(&mut state.stats);
                    // The cursor follows the moved module to its new row, so the
                    // operator is not left looking at whatever row the old
                    // index now names.
                    let name_for_select = name.clone();
                    let stats_snapshot = state.stats.clone();
                    if let Some(win) = state.get_window_mut(WindowId::Modules) {
                        win.select_module_name(&name_for_select, &stats_snapshot);
                    }
                    supervisor_log(
                        state,
                        format!(
                            "[supervisor] moved {} {} → {} (config.json rewritten; a running engine re-reads the ordering live)",
                            name, from, new_pos
                        ),
                    );
                }
                Ok(None) => {
                    // A no-op (an input adapter, or a module already at the
                    // stage edge): nothing to rewrite. The press was still
                    // consumed.
                    return Ok(false);
                }
                Err(e) => {
                    supervisor_log(
                        state,
                        format!("[supervisor] could not move {}: {}", name, e),
                    );
                    return Ok(false);
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

/// The `sql` payload for a pause toggle. `paused` is the state we currently
/// believe the engine is in, so the toggle asks for its negation — the engine
/// treats a repeat as idempotent (`changed: false`) rather than an error, which
/// is what makes a stale view safe here instead of harmful.
fn pipeline_pause_toggle_sql(paused: bool) -> String {
    serde_json::json!({ "paused": !paused }).to_string()
}

/// Send a one-shot query to the engine (via the WebSocket command channel).
fn send_engine_query(ws_command_tx: &mpsc::UnboundedSender<WsCommand>, query_id: String, sql: String) {
    let _ = ws_command_tx.send(WsCommand::SendQuery { query_id, sql });
}

// ── removing the engine from the TUI ───────────────────────────────────

/// The key the TUI's own supervisor files the engine process under. The
/// original launch site uses this name, and so must a restart: `drain()` at
/// teardown reaps by that key, so a restart that registers under any other name
/// leaks an orphan.
const ENGINE_PROCESS: &str = "engine";

/// The engine's control-surface shutdown query.
///
/// The engine gates it twice: the caller must be the TUI (`engine_shutdown
/// denied: not the TUI` otherwise) AND its own `config.json` must carry
/// `"shutdown_on_request": true`. That flag DEFAULTS TO FALSE, so a stock
/// engine refuses and keeps running — the refusal is the answer, not a failure.
const ENGINE_SHUTDOWN_QUERY: &str = "engine_shutdown";

/// How long to wait for the shutdown answer before giving up on it.
///
/// The engine answers BEFORE it exits, over a socket that is already open, so a
/// real answer is milliseconds. The window only exists so a silent engine
/// cannot leave the removal half-done (asked, never resolved, engine still
/// there) — the same job the `Disconnected` handler does for the usual way a
/// socket dies, and this covers the ways it does not.
const ENGINE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The `sql` payload for the shutdown request.
///
/// Deliberately NOT a `paused`-style flag: the engine decides from its own
/// `shutdown_on_request` and ignores everything else, and a payload shaped like
/// the pause toggle's would read in a log as "and also toggle something". One
/// key saying who asked is the whole payload.
fn engine_shutdown_sql() -> String {
    serde_json::json!({ "source": "tui-remove-engine" }).to_string()
}

/// What the engine's answer to `engine_shutdown` means for the removal.
///
/// All three are NORMAL outcomes, not errors: the engine is a separate process
/// with a policy of its own, and a refusal is that policy being applied, not a
/// failure of the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineRemovalOutcome {
    /// Accepted: the engine answers and then exits, so its socket is about to
    /// close on its own.
    Accepted,
    /// Refused by the engine's OWN `shutdown_on_request` flag. The engine is
    /// STILL RUNNING and will not stop until the operator says it may.
    Disabled(String),
    /// Refused by the engine's caller gate (`not the TUI`), or the answer never
    /// arrived. Nothing was told to exit, so the engine may still be running.
    Denied(String),
}

/// Read one shutdown answer.
///
/// Pure, so all three outcomes are pinned by tests instead of only being
/// reachable by standing up an engine. The refusal is classified on the CONFIG
/// KEY it names rather than on the whole sentence, so a reworded message still
/// lands in the right bucket — and the engine's own wording is what the
/// operator is shown, because the engine is the authority on why it is still
/// running.
pub fn classify_engine_shutdown(result: &cockatiel_client::proto::DatabaseQueryResult) -> EngineRemovalOutcome {
    if result.success {
        return EngineRemovalOutcome::Accepted;
    }
    let reason = if result.error.is_empty() {
        "(no detail)".to_string()
    } else {
        result.error.clone()
    };
    if result.error.contains("shutdown_on_request") {
        EngineRemovalOutcome::Disabled(reason)
    } else {
        EngineRemovalOutcome::Denied(reason)
    }
}

/// The one line the operator gets for a shutdown answer.
///
/// Two rules, both of which come from what an operator can do next. The refusal
/// repeats the ENGINE's own reason and then names the key that would change it,
/// because "it refused" on its own leaves nothing to act on. And every variant
/// says the TUI is forgetting the engine ANYWAY, so nobody is left believing a
/// still-running engine is still supervised (it is not — the client was told to
/// stop dialling) or that a refusal undid the removal (it did not).
fn engine_removal_note(outcome: &EngineRemovalOutcome) -> String {
    match outcome {
        EngineRemovalOutcome::Accepted => "[supervisor] remove engine: the engine accepted — it is answering and then exiting. The TUI is forgetting it either way; start it again by hand, or run the TUI again, to bring it back.".to_string(),
        EngineRemovalOutcome::Disabled(reason) => format!(
            "[supervisor] remove engine: the engine REFUSED ({reason}) — it is STILL RUNNING, because its own config.json does not allow it to be stopped over the wire. Set \"shutdown_on_request\": true there (E on the engine row opens it) to allow this next time. The TUI is forgetting the engine; the process is not."
        ),
        EngineRemovalOutcome::Denied(reason) => format!(
            "[supervisor] remove engine: the engine did not accept the request ({reason}) — nothing was told to exit, so it may still be running. The TUI is forgetting it either way."
        ),
    }
}

/// Whether a shutdown request has been outstanding long enough to give up on.
pub fn removal_deadline_passed(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|d| now >= d)
}

/// Step one of removing the engine: ask, then WAIT for the answer.
///
/// The wait is the whole point. Firing the query and immediately dropping the
/// connection would be indistinguishable from an engine that ignored it, and
/// the three answers (accepted / refused by config / denied by the gate) are
/// three different things the operator has to be told.
fn begin_engine_removal(state: &mut AppState, ws_command_tx: &mpsc::UnboundedSender<WsCommand>) {
    if state.stats.engine_removed {
        supervisor_log(state, "[supervisor] remove engine: this TUI has already forgotten its engine");
        return;
    }
    if state.pending_engine_removal.is_some() {
        supervisor_log(state, "[supervisor] remove engine: still waiting for the engine's answer to the last request");
        return;
    }
    if !state.connected {
        // Nothing to ask, so nothing to wait for. The engine is unreachable
        // (it was never there, or it is already down) and the removal is just
        // the forgetting — reported as a refusal so the operator still learns
        // that nothing was asked of a running process.
        finish_engine_removal(
            state,
            ws_command_tx,
            EngineRemovalOutcome::Denied("not connected to an engine, so there was nothing to ask".to_string()),
        );
        return;
    }
    send_engine_query(
        ws_command_tx,
        ENGINE_SHUTDOWN_QUERY.to_string(),
        engine_shutdown_sql(),
    );
    state.pending_engine_removal = Some(Instant::now() + ENGINE_SHUTDOWN_TIMEOUT);
    supervisor_log(state, "[supervisor] remove engine: asked the engine to shut down — waiting for its answer");
}

/// Step two: report what the engine said, stop the client, forget the engine.
///
/// In exactly that order, because the operator's log line is the only record
/// of a refusal and it has to be written before the connection that carried it
/// goes away.
///
/// NOTHING here touches the engine's `config.json` or `.env`. "Remove the
/// engine" means THIS TUI stops pretending it has one — it is not the
/// destructive `DeleteModule` sitting one key away, whose whole job is to
/// destroy a module's registration. The operator's settings are still there, the
/// config editor still points at that directory (it is how they read
/// `shutdown_on_request` and change it), and the supervised child stays in the
/// process table so TUI teardown reaps it instead of leaking an orphan. The
/// `X` key is the one that invites the wrong reading, which is exactly why this
/// comment is here.
fn finish_engine_removal(
    state: &mut AppState,
    ws_command_tx: &mpsc::UnboundedSender<WsCommand>,
    outcome: EngineRemovalOutcome,
) {
    supervisor_log(state, engine_removal_note(&outcome));
    // The stop switch, for the client task's next check...
    state.detach_engine();
    // ...and the command that closes the socket now instead of leaving it open
    // for the rest of the reconnect backoff.
    let _ = ws_command_tx.send(WsCommand::Disconnect);
    state.connected = false;
    state.stats.forget_engine();
}

// ── restarting the engine ──────────────────────────────────────────────

/// Outcomes of an engine `start` (`s`) from the engine row. Like
/// [`RestartOutcome`], the refusals are distinct: `Removed` is "the TUI has no
/// engine", `AlreadySupervised` is "the TUI already owns one", and
/// `AlreadyRunning` is "somebody's engine is on the port".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStartOutcome {
    /// The engine was removed from this TUI. Relaunching under it would recreate
    /// the orphan the removal exists to prevent.
    Removed,
    /// A supervised child is already registered under [`ENGINE_PROCESS`].
    /// `resumed` is whether the pipeline was paused and start should re-open it.
    AlreadySupervised { resumed: bool },
    /// No child, but something already answers on the configured port — connect
    /// to it rather than starting a second engine. `resumed` as above.
    AlreadyRunning { resumed: bool },
    /// Launched and registered under [`ENGINE_PROCESS`]. A fresh engine boots
    /// paused (`start_paused` defaults true), and the operator pressed "start",
    /// so the pipeline is always resumed.
    Launched { pid: u32 },
    /// The launch failed; nothing is registered.
    Failed { error: String },
}

impl EngineStartOutcome {
    /// Whether the caller must send the resume query (`pipeline_set_paused`
    /// with `{"paused": false}`) after this outcome.
    ///
    /// "Start the engine" from the operator's seat means "make it run", and a
    /// running-but-paused engine is precisely the trap of pressing `s` and
    /// getting "already listening" while nothing moves. So every way this
    /// outcome ends with the engine available re-opens the pipeline; only a
    /// refusal or a failed launch does not.
    fn resume_pipeline(&self) -> bool {
        match self {
            EngineStartOutcome::Removed | EngineStartOutcome::Failed { .. } => false,
            EngineStartOutcome::AlreadySupervised { resumed }
            | EngineStartOutcome::AlreadyRunning { resumed } => *resumed,
            EngineStartOutcome::Launched { .. } => true,
        }
    }
}

/// Outcomes of an engine `stop` (`x`) from the engine row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStopOutcome {
    /// Already removed from this TUI; nothing to stop.
    Removed,
    /// No supervised child. The TUI does not own this engine, so it must not
    /// kill it — the same boundary `RestartOutcome::NotSupervised` draws.
    NotSupervised,
    /// The supervised child was killed.
    Stopped { pid: u32 },
}

/// Launch and supervise the engine, if it is not already running. This is the
/// `s` (start) key on the engine row, mirroring the startup launch site.
///
/// `spawn` and `probe` are injected (`FnOnce`/`Fn`, because a `Child` is not
/// `Copy` and the port probe is a real socket call) so the decision sequence is
/// testable without an engine binary or a real port, exactly like
/// [`restart_engine`]. Production passes `supervisor::launch_engine` and the
/// startup's own TCP probe.
fn start_engine(
    state: &AppState,
    supervisor: &mut supervisor::ProcessTable,
    spawn: impl FnOnce() -> Result<std::process::Child, String>,
    already_up: impl Fn() -> bool,
) -> EngineStartOutcome {
    if state.stats.engine_removed {
        return EngineStartOutcome::Removed;
    }
    if supervisor.contains_key(ENGINE_PROCESS) {
        return EngineStartOutcome::AlreadySupervised { resumed: state.stats.pipeline_paused };
    }
    // Same probe the startup launch site uses: is somebody's engine already
    // listening on the configured port? If so we do not start a second one —
    // we just connect to the one that is there (the client reconnects on its
    // own backoff).
    if already_up() {
        return EngineStartOutcome::AlreadyRunning { resumed: state.stats.pipeline_paused };
    }
    match spawn() {
        Ok(child) => {
            let pid = child.id();
            supervisor.insert(
                ENGINE_PROCESS.to_string(),
                Arc::new(Mutex::new(supervisor::ManagedProcess {
                    child,
                    terminal_window: None,
                    terminal_pidfile: None,
                })),
            );
            EngineStartOutcome::Launched { pid }
        }
        Err(error) => EngineStartOutcome::Failed { error },
    }
}

/// The operator-facing line for a start attempt.
fn engine_start_note(outcome: &EngineStartOutcome) -> String {
    match outcome {
        EngineStartOutcome::Removed => "[supervisor] start engine: the engine was removed from this TUI, so it will not be relaunched under it — E opens its config, and running the TUI again brings it back".to_string(),
        EngineStartOutcome::AlreadySupervised { resumed } => {
            if *resumed {
                "[supervisor] start engine: already running under this TUI — the pipeline was paused, resuming it".to_string()
            } else {
                "[supervisor] start engine: already running under this TUI — the pipeline is open".to_string()
            }
        }
        EngineStartOutcome::AlreadyRunning { resumed } => {
            if *resumed {
                "[supervisor] start engine: an engine is already listening on the configured port — the pipeline was paused, resuming it".to_string()
            } else {
                "[supervisor] start engine: an engine is already listening on the configured port — the pipeline is open, connecting".to_string()
            }
        }
        EngineStartOutcome::Launched { pid } => format!("[supervisor] start engine: launched (pid {pid}) — resuming the pipeline"),
        EngineStartOutcome::Failed { error } => format!("[supervisor] start engine: failed to launch ({error}) — its config is untouched, so fix the engine and try again"),
    }
}

/// Stop the supervised engine process. This is the `x` (stop) key on the engine
/// row — the same kill path modules use, on the engine's process.
fn stop_engine(
    state: &AppState,
    supervisor: &mut supervisor::ProcessTable,
) -> EngineStopOutcome {
    if state.stats.engine_removed {
        return EngineStopOutcome::Removed;
    }
    let Some(proc) = supervisor.remove(ENGINE_PROCESS) else {
        return EngineStopOutcome::NotSupervised;
    };
    let mut proc = proc.lock().unwrap();
    let pid = proc.pid();
    proc.kill();
    EngineStopOutcome::Stopped { pid }
}

/// The operator-facing line for a stop attempt. Says whether anything was
/// actually killed, so "stopped" is never a lie about a process the TUI does
/// not own.
fn engine_stop_note(outcome: &EngineStopOutcome) -> String {
    match outcome {
        EngineStopOutcome::Removed => "[supervisor] stop engine: the engine was already removed from this TUI — nothing to stop".to_string(),
        EngineStopOutcome::NotSupervised => "[supervisor] stop engine: the TUI did not launch this engine, so it will not kill it — stop it where you started it".to_string(),
        EngineStopOutcome::Stopped { pid } => format!("[supervisor] stop engine: killed the engine process (pid {pid})"),
    }
}

/// What a restart attempt actually did.
///
/// Data rather than a log line so every branch is reachable from a test, and so
/// the two refusals cannot be confused with each other: `Removed` is "the TUI
/// has no engine" and `NotSupervised` is "the TUI has an engine it does not own".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartOutcome {
    /// The engine was removed from this TUI. Nothing to restart, and launching
    /// one here would create precisely the orphan the removal exists to prevent
    /// — the client was told never to reconnect, so a fresh engine would have
    /// nobody to talk to and nothing would reap it until the TUI exited.
    Removed,
    /// No supervised child. The TUI did not launch this engine (it was already
    /// running, or the TUI was started `--no-engine`), so it has no business
    /// killing a process it does not own.
    NotSupervised,
    /// The old child was killed and a new one is registered under
    /// [`ENGINE_PROCESS`].
    Relaunched { old_pid: u32, pid: u32 },
    /// The old child is gone and the new one would not start.
    Failed { old_pid: u32, error: String },
}

/// Restart the supervised engine: kill the tracked child, launch a new one,
/// re-register it, and let the websocket client reconnect by itself.
///
/// `spawn` is the relaunch, injected (`FnOnce`, because a `Child` cannot be
/// cloned or handed out twice) so the sequence can be tested without starting a
/// real engine — the part that is NOT unit-testable is `Command::spawn` itself,
/// and the part that carries the risk is the order and the re-registration,
/// which is exactly what this tests.
///
/// The reconnect is deliberately NOT here: the client task is still running (a
/// restart never detaches it) and retries on its own backoff, which is the same
/// path an engine crash already takes. Reimplementing it would be a second
/// mechanism for one job.
fn restart_engine(
    state: &AppState,
    supervisor: &mut supervisor::ProcessTable,
    // Taken by value, not by reference: a `Child` is not `Copy`, so a launcher
    // can only be run once, and `FnOnce` is what says so.
    spawn: impl FnOnce() -> Result<std::process::Child, String>,
) -> RestartOutcome {
    if state.stats.engine_removed {
        return RestartOutcome::Removed;
    }
    let Some(old) = supervisor.remove(ENGINE_PROCESS) else {
        return RestartOutcome::NotSupervised;
    };
    // Kill BEFORE launching: the new engine has to bind the same port, and a
    // TERM→KILL group kill is exactly what the original launch's teardown uses.
    let old_pid = {
        let mut proc = old.lock().unwrap();
        let pid = proc.pid();
        proc.kill();
        pid
    };
    match spawn() {
        Ok(child) => {
            let pid = child.id();
            // Same key the original launch registers under, so `drain()` at
            // teardown still finds it. A restart that forgets this is how a
            // restart becomes an orphan.
            supervisor.insert(
                ENGINE_PROCESS.to_string(),
                Arc::new(Mutex::new(supervisor::ManagedProcess {
                    child,
                    terminal_window: None,
                    terminal_pidfile: None,
                })),
            );
            RestartOutcome::Relaunched { old_pid, pid }
        }
        Err(error) => {
            // The old engine is already dead and the new one never started, so
            // the table is now empty: the TUI supervises nothing, which is the
            // truth, and a later restart press will report `NotSupervised`
            // rather than pretend it is supervising a dead child.
            RestartOutcome::Failed { old_pid, error }
        }
    }
}

/// The operator-facing line for a restart attempt. Says what happened to the
/// OLD process too — a restart that silently leaves the old one running (or
/// silently fails to stop it) is the failure mode the operator cannot see.
fn restart_note(outcome: &RestartOutcome) -> String {
    match outcome {
        RestartOutcome::Removed => "[supervisor] restart engine: the engine was removed from this TUI, so there is nothing to restart — E opens its config, and running the TUI again brings it back".to_string(),
        RestartOutcome::NotSupervised => "[supervisor] restart engine: the TUI did not launch this engine, so it will not kill it — stop it where you started it, or start the TUI without --no-engine to have it supervise one".to_string(),
        RestartOutcome::Relaunched { old_pid, pid } => format!(
            "[supervisor] restart engine: killed the old engine (pid {old_pid}) and launched a new one (pid {pid}) — reconnecting"
        ),
        RestartOutcome::Failed { old_pid, error } => format!(
            "[supervisor] restart engine: killed the old engine (pid {old_pid}) and the new one would not start ({error}) — its config is untouched, so fix the engine and start it by hand"
        ),
    }
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

    /// The engine is a real, selectable row in the modules window, and it is not
    /// a module. The per-module keys must therefore do NOTHING there rather
    /// than fall through to the app's "first known module" fallback — an
    /// unguarded `x` there would kill a module nobody selected. `EditConfig` is
    /// the deliberate exception: the engine has its own config to open.
    #[test]
    fn per_module_actions_are_refused_on_the_engine_row() {
        use crate::app::WindowId;
        use crate::windows::ModulesWindow;

        let state_with = |selected: usize| {
            let mut s = AppState::new(
                crate::colors::load_colors(&std::path::PathBuf::from("")),
                crate::hotkeys::default_hotkeys(),
            );
            let mut win = ModulesWindow::new();
            win.selected = selected;
            s.windows = vec![Box::new(win)];
            s.active_window = WindowId::Modules;
            s.stats.module_entries = (0..2)
                .map(|i| crate::db::ModuleStatus {
                    name: format!("m{}", i),
                    description: String::new(),
                    status: "connected".into(),
                    position: "preprocess".into(),
                    credentials: Vec::new(),
                    directory: String::new(),
                    credential_values: Default::default(),
                    config_complete: true,
                    alive: true,
                    last_seen: 0,
                    avg_ms: None,
                    autostart: false,
                })
                .collect();
            s
        };

        // Row ENGINE_ROW is the engine (row 0 is the [ENGINE] header above it).
        let engine = state_with(1);
        assert!(!focused_selection_is_module(&engine));
        // The module-only actions are refused on the engine row.
        for a in [
            Action::DeleteModule(String::new()),
            Action::ToggleAutostart(String::new()),
            Action::DuplicateModule(String::new()),
            Action::ClearModuleConfig(String::new()),
            Action::EditCredentials(String::new()),
            Action::RunTests,
        ] {
            assert!(is_module_scoped(&a), "{:?} should be module-scoped", a);
            assert!(
                is_module_scoped(&a) && !focused_selection_is_module(&engine),
                "{:?} must be refused with the engine row selected",
                a
            );
        }
        // Start and stop are NOT module-scoped: the engine row reuses the
        // standard `s`/`x` keys to launch/kill the ENGINE process, so the
        // module-scope refusal must not block them. The dispatcher redirects
        // them when the engine row is selected (covered elsewhere).
        assert!(!is_module_scoped(&Action::StartModule(String::new())));
        assert!(!is_module_scoped(&Action::StopModule(String::new())));
        // The two that are NOT refused: the edit key (the engine has a config)
        // and the global pause toggle (it is engine-scoped, not module-scoped).
        assert!(!is_module_scoped(&Action::EditConfig(String::new())));
        assert!(!is_module_scoped(&Action::TogglePipelinePause));
        assert!(is_dispatchable(&Action::EditConfig(String::new())));
        assert!(is_dispatchable(&Action::TogglePipelinePause));

        // A module row is a module again, so every one of them is allowed. With two
        // pre-process modules and every group header always present, the first one
        // sits at row 4 ([ENGINE] header, engine, [ADAPTERS], [PRE-PROCESS], m0).
        let module = state_with(4);
        assert!(focused_selection_is_module(&module));
        assert!(!is_module_scoped(&Action::StopModule(String::new()))
            || focused_selection_is_module(&module));
        assert_eq!(selected_module_name(&module), "m0");

        // A window with no module selection of its own (the log, the chart)
        // keeps the historical behaviour of acting on the first known module.
        let mut other = state_with(0);
        other.windows = vec![Box::new(crate::windows::LogWindow::new())];
        other.active_window = WindowId::Log;
        assert!(focused_selection_is_module(&other));
    }

    /// `autostart_module_names` reads `autostart` from each plugin's raw
    /// manifest (not the parsed struct), so a missing flag or unreadable
    /// manifest is treated as not-autostart. This is the one-click-start
    /// pick list.
    #[test]
    fn autostart_picks_only_manifests_that_say_true() {
        use crate::plugins::MANIFEST_FILENAME;
        use crate::plugins::ModuleManifest;

        fn plugin(name: &str, dir: std::path::PathBuf) -> crate::plugins::Plugin {
            crate::plugins::Plugin {
                manifest: ModuleManifest {
                    name: name.to_string(),
                    description: String::new(),
                    version: String::new(),
                    capabilities: String::new(),
                    root_file: String::new(),
                    launch_command: String::new(),
                    command_flags: Vec::new(),
                    terminal: false,
                    credentials: Vec::new(),
                    binary: Default::default(),
                    build_command: None,
                    build_flags: Vec::new(),
                },
                directory: dir,
            }
        }

        let tmp = std::env::temp_dir().join(format!("ck-autostart-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();

        // Three module dirs: one autostart:true, one autostart:false, one with
        // no autostart key at all.
        let on_dir = tmp.join("on");
        std::fs::create_dir_all(&on_dir).unwrap();
        std::fs::write(on_dir.join(MANIFEST_FILENAME), r#"{"name":"on","autostart":true}"#).unwrap();

        let off_dir = tmp.join("off");
        std::fs::create_dir_all(&off_dir).unwrap();
        std::fs::write(off_dir.join(MANIFEST_FILENAME), r#"{"name":"off","autostart":false}"#).unwrap();

        let unset_dir = tmp.join("unset");
        std::fs::create_dir_all(&unset_dir).unwrap();
        std::fs::write(unset_dir.join(MANIFEST_FILENAME), r#"{"name":"unset"}"#).unwrap();

        let plugins = vec![
            plugin("on", on_dir),
            plugin("off", off_dir),
            plugin("unset", unset_dir),
        ];
        assert_eq!(autostart_module_names(&plugins), vec!["on".to_string()]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// End-to-end through the real dispatch: `E` with the engine row selected
    /// opens the ENGINE's config, not a module's. The engine is not a plugin,
    /// so `plugins` is empty here — which is exactly the case that used to
    /// open nothing at all.
    #[test]
    fn the_edit_key_opens_the_engine_config_end_to_end() {
        use crate::app::WindowId;
        use crate::windows::ModulesWindow;

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut s = AppState::new(
                crate::colors::load_colors(&std::path::PathBuf::from("")),
                crate::hotkeys::default_hotkeys(),
            );
            s.windows = vec![Box::new(ModulesWindow::new())];
            s.active_window = WindowId::Modules;
            s.stats.module_entries = vec![crate::db::ModuleStatus {
                name: "m0".into(),
                description: String::new(),
                status: "connected".into(),
                position: "preprocess".into(),
                credentials: Vec::new(),
                directory: String::new(),
                credential_values: Default::default(),
                config_complete: true,
                alive: true,
                last_seen: 0,
                avg_ms: None,
                autostart: false,
            }];

            // Filled in by `fill_window_action` for a real keypress. With the
            // engine row selected it resolves to the fallback name, which this
            // path must ignore in favour of the window's own answer.
            let action = fill_window_action(&s, "modules", Action::EditConfig(String::new()));
            let mut supervisor: supervisor::ProcessTable = Default::default();
            let mut plugins: Vec<crate::plugins::Plugin> = Vec::new();
            let (tx, _rx) = mpsc::unbounded_channel::<WsCommand>();

            dispatch_action(
                &mut s,
                action,
                &mut supervisor,
                &mut plugins,
                0,
                0,
                &tx,
                "127.0.0.1:1".parse().unwrap(),
                "",
            )
            .await
            .unwrap();

            assert_eq!(s.active_window, WindowId::Modules);
            let window = s.get_window_mut(WindowId::Modules).expect("modules window");
            assert!(window.in_editor(), "the edit key must open the config editor");
        });
    }

    #[test]
    fn the_pause_toggle_sends_the_negation_of_the_current_state() {        // Believed running → ask for a pause. Believed paused → ask for a
        // resume. The toggle is the operator's intent, so the payload must be
        // the inverse of what we last knew, never a hardcoded direction.
        for (known_paused, expected) in [(false, "true"), (true, "false")] {
            let sql = pipeline_pause_toggle_sql(known_paused);
            let parsed: serde_json::Value = serde_json::from_str(&sql).expect("valid json payload");
            assert_eq!(parsed["paused"], serde_json::json!(expected == "true"), "sql: {}", sql);
            assert_eq!(
                parsed.as_object().unwrap().len(),
                1,
                "the payload carries only the flag: {}",
                sql
            );
        }
        // Exactly the two wire forms the engine parses.
        assert_eq!(pipeline_pause_toggle_sql(false), r#"{"paused":true}"#);
        assert_eq!(pipeline_pause_toggle_sql(true), r#"{"paused":false}"#);
    }

    #[test]
    fn the_pause_toggle_travels_as_a_pipeline_set_paused_query() {
        let (tx, mut rx) = mpsc::unbounded_channel::<WsCommand>();
        send_engine_query(&tx, "pipeline_set_paused".to_string(), pipeline_pause_toggle_sql(true));
        match rx.try_recv() {
            Ok(WsCommand::SendQuery { query_id, sql }) => {
                assert_eq!(query_id, "pipeline_set_paused");
                let parsed: serde_json::Value = serde_json::from_str(&sql).unwrap();
                assert_eq!(parsed["paused"], false);
            }
            other => panic!("unexpected: {:?}", other),
        }
        assert!(is_dispatchable(&Action::TogglePipelinePause));
    }
}

#[cfg(test)]
mod full_repaint_wiring_tests {
    /// The clear decision is unit-tested on AppState, but the DRAW LOOP living
    /// inside `run_app` is not reachable from a test. Deleting the call site
    /// would silently disable the whole feature while every behavioural test
    /// still passed, so assert the wiring structurally.
    #[test]
    fn the_draw_loop_actually_clears_on_a_shape_change() {
        let src = include_str!("main.rs");
        let start = src.find("async fn run_app(").expect("run_app not found");
        let end = start + src[start..].find("\nasync fn ").expect("end of run_app");
        let body = &src[start..end];

        assert!(
            body.contains("needs_full_repaint("),
            "run_app must ask AppState whether this frame needs a full repaint"
        );
        assert!(
            body.contains("terminal.clear()"),
            "run_app must clear the terminal before a full repaint, otherwise ratatui still diffs"
        );
        // The clear has to happen BEFORE the draw, or it is pointless.
        let clear_at = body.find("terminal.clear()").expect("clear missing");
        let draw_at = body.find("terminal.draw(").expect("draw missing");
        assert!(clear_at < draw_at, "the clear must precede the draw");
    }

    #[test]
    fn ctrl_l_is_bound_as_the_manual_repaint_escape_hatch() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("force_full_redraw = true"),
            "Ctrl+L must raise the manual full-repaint flag"
        );
        assert!(
            src.contains("KeyModifiers::CONTROL") && src.contains("KeyCode::Char('l')"),
            "the escape hatch must be bound to Ctrl+L"
        );
    }
}

#[cfg(test)]
mod pause_blink_wiring_tests {
    /// `AppState::pause_flash_on` is unit-tested, but its clock lives in the
    /// DRAW LOOP inside `run_app`, which no behavioural test can reach: dropping
    /// the increment would freeze the indicator on one phase (or pin it hidden)
    /// while every other test still passed. Assert the wiring structurally.
    #[test]
    fn the_blink_phase_is_advanced_by_the_draw_ticker() {
        let src = include_str!("main.rs");
        let start = src.find("async fn run_app(").expect("run_app not found");
        let end = start + src[start..].find("\nasync fn ").expect("end of run_app");
        let body = &src[start..end];

        let tick_at = body.find("_ = redraw.tick()").expect("redraw tick arm missing");
        let bump_at = body
            .find("state.stats.pause_flash_tick = pause_flash_tick")
            .expect("the blink phase must be published to stats on every redraw tick");
        let advance_at = body
            .find("pause_flash_tick = pause_flash_tick.wrapping_add(1)")
            .expect("the blink phase must advance");
        assert!(tick_at < bump_at, "the phase must be published from the redraw tick");
        assert!(bump_at < advance_at, "publish this tick's phase, then advance");

        // The watchdog `continue`s out of the tick arm while disconnected, so
        // the advance has to come first or the cadence would drift.
        assert!(
            advance_at < body[tick_at..].find("watchdog_last.elapsed()").expect("watchdog") + tick_at,
            "advance the phase before anything can skip the rest of the arm"
        );
        // Drawn on every pass of the loop, so a phase flip is painted without a
        // full repaint (see AppState::pause_flash_on).
        assert!(body.contains("terminal.draw("), "the loop must draw each frame");
    }

    #[test]
    fn the_blink_is_not_part_of_the_shape_fingerprint() {
        // If the phase ever moved into ScreenShape, every flip would clear and
        // repaint the whole terminal to change a few glyphs. Pin the decision.
        let app = include_str!("app.rs");
        let shape_at = app.find("pub struct ScreenShape").expect("ScreenShape");
        let shape_end = app[shape_at..].find('}').expect("end of ScreenShape") + shape_at;
        assert!(
            !app[shape_at..shape_end].contains("flash") && !app[shape_at..shape_end].contains("blink"),
            "a blink changes no geometry — it must stay out of the shape fingerprint"
        );
        // ...while the phase still reaches the renderer through stats.
        let db = include_str!("db.rs");
        assert!(db.contains("pub pause_flash_tick: u64"), "the tick must be part of stats");
    }

    #[test]
    fn a_global_binding_is_dispatched_not_swallowed() {
        // handle_global_key now returns dispatchable global actions. The caller
        // has to run them, not just match `Quit` — otherwise the pause key is
        // consumed and silently dropped. (Asserted positively: this file is
        // its own source here, so naming the old form would match the test.)
        let src = include_str!("main.rs");
        let start = src.find("async fn handle_input_event(").expect("handle_input_event");
        let end = start + src[start..].find("\n/// Handle a keypress").expect("end of the key arm");
        let body = &src[start..end];
        let global_at = body
            .find("if let Some(action) = state.handle_global_key(key)")
            .expect("the global binding result must be bound, not pattern-matched away");
        let dispatch_at = body.find("is_dispatchable(&action)").expect("global actions must be dispatched");
        assert!(global_at < dispatch_at, "the dispatch must follow the global lookup");
        assert!(
            body[global_at..].contains("dispatch_action("),
            "a dispatchable global action must reach dispatch_action"
        );
    }
}

#[cfg(test)]
mod terminal_hygiene_tests {
    /// Anything printed to stdout/stderr while the alternate screen + raw mode
    /// are active lands in the middle of the ratatui UI: in raw mode ONLCR is
    /// off, so a bare `\n` never returns the cursor to column 0 and the text
    /// staircases down the screen, scrolling it and pushing the layout up. It
    /// shows up at the cursor, inside a window, or at the bottom depending on
    /// where the cursor happened to be.
    ///
    /// These call sites are allowed because they run BEFORE the UI exists
    /// (engine/user-db launch) or AFTER it is torn down (panic hook, teardown),
    /// or are test-only. Everything else must go through
    /// `app::supervisor_log_global` so it lands in the log window.
    #[test]
    fn no_live_code_prints_directly_to_the_terminal() {
        let src = include_str!("main.rs");
        let allowed = [
            // Panic hook + teardown: run after LeaveAlternateScreen, and a
            // panic message must be visible on the real terminal.
            "TUI panicked",
            "at {}:{}:{}",
            "run with RUST_BACKTRACE",
            "Error: {}",
            // Engine / user-db launch. These run BEFORE enable_raw_mode() and
            // EnterAlternateScreen, so there is no UI to corrupt yet and a
            // failed launch genuinely needs to be visible on the terminal.
            "Launched user database",
            "User DB launch failed",
            "Launched engine",
            "Engine launch failed",
            // Teardown of a module process we are killing as we exit.
            "Killing {} (pid {})",
        ];
        for (i, line) in src.lines().enumerate() {
            let t = line.trim();
            if !t.starts_with("eprintln!") && !t.starts_with("println!") && !t.starts_with("print!") {
                continue;
            }
            assert!(
                allowed.iter().any(|a| line.contains(a)),
                "main.rs:{} prints to the terminal while the UI may be live: {}",
                i + 1,
                t
            );
        }
    }

    #[test]
    fn background_tasks_route_diagnostics_to_the_log_window() {
        // The websocket client reconnects every 3s and the pop-out server logs
        // every handshake, so both used to print constantly, from tokio tasks,
        // with the UI live.
        for (path, body) in [
            ("ws_client.rs", include_str!("ws_client.rs")),
            ("ws_server.rs", include_str!("ws_server.rs")),
            ("supervisor.rs", include_str!("supervisor.rs")),
            ("windows/modules.rs", include_str!("windows/modules.rs")),
        ] {
            assert!(
                !body.contains("eprintln!(\"[ws_client]"),
                "{} still prints websocket-client errors to the terminal",
                path
            );
            assert!(
                !body.contains("eprintln!(\"WS server"),
                "{} still prints websocket-server errors to the terminal",
                path
            );
        }
    }

    #[test]
    fn the_popout_gets_its_own_terminal_instead_of_the_parents_tty() {
        let src = include_str!("main.rs");
        let start = src.find("Action::PopOut(window_name) =>").expect("PopOut arm missing");
        let arm = &src[start..start + 1400];
        assert!(
            arm.contains("spawn_in_new_terminal("),
            "the pop-out must be launched into a terminal of its own"
        );
        assert!(
            !arm.contains("std::process::Command::new(&exe)"),
            "the pop-out must not be spawned with inherited stdio onto the parent's tty"
        );
    }
}

#[cfg(test)]
mod engine_lifecycle_tests {
    //! Removing and restarting the ENGINE, and the `--with-engine` /
    //! `--no-engine` launch switch.
    //!
    //! The removal is the load-bearing part. It is a SEQUENCE — ask, wait for
    //! the answer, report it, stop the client, forget the engine — and every
    //! step of it is asserted here at the moment it happens, because the failure
    //! mode is silent in both directions: fire-and-forget is indistinguishable
    //! from an engine that ignored the request, and a client left reconnecting
    //! is an orphan nobody notices until the next boot.
    use super::*;

    /// A modules window with the engine row selected and a live engine: the
    /// state every test here starts from.
    fn engine_state() -> AppState {
        use crate::app::WindowId;
        use crate::windows::ModulesWindow;
        let mut s = AppState::new(
            crate::colors::load_colors(&std::path::PathBuf::from("")),
            crate::hotkeys::default_hotkeys(),
        );
        s.windows = vec![Box::new(ModulesWindow::new())];
        s.active_window = WindowId::Modules;
        s.connected = true;
        s.stats.engine_status = "connected".to_string();
        s.stats.module_entries = vec![crate::db::ModuleStatus {
            name: "m0".into(),
            description: String::new(),
            status: "connected".into(),
            position: "preprocess".into(),
            credentials: Vec::new(),
            directory: String::new(),
            credential_values: Default::default(),
            config_complete: true,
            alive: true,
            last_seen: 0,
            avg_ms: None,
            autostart: false,
        }];
        s.stats.connection = db::ConnectionInfo { ip: "127.0.0.1".into(), port: 9734, pin: 4242 };
        s
    }

    /// The engine's own refusal strings, quoted here so a rename on the engine
    /// side that the TUI's wording no longer matches is a test failure rather
    /// than a silently different message.
    const DISABLED_REFUSAL: &str = "engine_shutdown denied: engine shutdown is disabled (shutdown_on_request is false in config.json)";
    const GATE_REFUSAL: &str = "engine_shutdown denied: not the TUI";

    fn shutdown_result(success: bool, error: &str) -> cockatiel_client::proto::DatabaseQueryResult {
        cockatiel_client::proto::DatabaseQueryResult {
            query_id: ENGINE_SHUTDOWN_QUERY.to_string(),
            success,
            error: error.to_string(),
            result_blob: Vec::new(),
        }
    }

    /// A broadcast sender with no subscribers. The removal never reaches it
    /// (events after the removal are dropped), and a panicking `send` on an
    /// empty channel would be a false failure.
    fn no_broadcast() -> broadcast::Sender<WsEvent> {
        broadcast::channel::<WsEvent>(4).0
    }

    // ── the remove sequence ───────────────────────────────────────────────

    /// The whole sequence, walked in order through the real functions: ask →
    /// wait → report → stop the client → forget the engine.
    #[test]
    fn the_remove_sequence_asks_waits_then_stops_and_forgets() {
        let mut s = engine_state();
        let (tx, mut rx) = mpsc::unbounded_channel::<WsCommand>();

        // ── 1. ask ──
        begin_engine_removal(&mut s, &tx);
        match rx.try_recv().expect("the request must be sent") {
            WsCommand::SendQuery { query_id, sql } => {
                assert_eq!(query_id, "engine_shutdown", "the engine's own query id");
                // The payload says who asked and nothing else. A `paused`-style
                // field would read as "and toggle something", which is the
                // wrong impression to leave in a log next to a shutdown.
                let parsed: serde_json::Value = serde_json::from_str(&sql).expect("valid json");
                assert!(parsed.get("paused").is_none(), "no pause flag: {}", sql);
                assert!(
                    parsed.get("shutdown_on_request").is_none(),
                    "that flag belongs to the engine's own config, not to a request: {}",
                    sql
                );
                assert_eq!(parsed.as_object().unwrap().len(), 1, "payload: {}", sql);
            }
            other => panic!("expected the shutdown query, got {:?}", other),
        }

        // ── 2. WAIT. Nothing is stopped and nothing is forgotten yet: the
        //    operator is owed an answer, and a refusal is a real answer. ──
        assert!(s.pending_engine_removal.is_some(), "the request must be pending");
        assert!(rx.try_recv().is_err(), "nothing else may be sent while waiting");
        assert!(!s.engine_forgotten(), "the client must still be connected while waiting");
        assert!(!s.stats.engine_removed);
        assert_eq!(s.stats.engine_status, "connected", "the row still shows a live engine");
        assert_eq!(s.stats.connection.port, 9734);

        // The answer arrives through the real event handler (the wire path, not
        // a direct call), as the engine's own "flag is off" refusal.
        handle_ws_event(
            WsEvent::QueryResult {
                query_id: ENGINE_SHUTDOWN_QUERY.to_string(),
                result: shutdown_result(false, DISABLED_REFUSAL),
            },
            &mut s,
            &tx,
            &no_broadcast(),
        );
        assert!(s.pending_engine_removal.is_none(), "the request is answered");

        // ── 3. disconnected, and stopped reconnecting ──
        assert!(s.engine_forgotten(), "the client must be stopped for good");
        assert!(!s.connected);
        assert!(
            matches!(rx.try_recv(), Ok(WsCommand::Disconnect)),
            "the socket must be closed now, not left to the reconnect backoff"
        );

        // ── 4. the TUI's copy of the engine is gone ──
        assert!(s.stats.engine_removed);
        assert!(s.stats.connection.ip.is_empty() && s.stats.connection.port == 0);
        assert!(s.stats.connection.pin == 0);
        assert!(s.stats.module_entries.is_empty());
        assert!(s.stats.pipeline_paused, "no belief about a gate that is gone");
        assert_ne!(s.stats.engine_status, "connected");
        // The supervisor's own module bookkeeping is NOT cleared: those
        // processes are still running under the TUI.
        assert_eq!(s.module_runs.lock().unwrap().len(), 0);
    }

    /// All three answers are NORMAL outcomes and each has to produce its own
    /// line. A refusal that says only "refused" leaves the operator with nothing
    /// to do; an acceptance indistinguishable from a refusal leaves them unsure
    /// whether their engine is gone.
    #[test]
    fn each_shutdown_answer_gets_its_own_operator_facing_line() {
        // 1. Accepted: it is answering and then exiting.
        assert_eq!(
            classify_engine_shutdown(&shutdown_result(true, "")),
            EngineRemovalOutcome::Accepted
        );
        let accepted = engine_removal_note(&EngineRemovalOutcome::Accepted);
        assert!(accepted.contains("accepted"), "{}", accepted);
        assert!(accepted.contains("exiting"), "the engine is on its way out: {}", accepted);

        // 2. Refused by the engine's own flag: STILL RUNNING, and here is both
        //    the reason (the engine's own words) and the key that would change
        //    it.
        assert_eq!(
            classify_engine_shutdown(&shutdown_result(false, DISABLED_REFUSAL)),
            EngineRemovalOutcome::Disabled(DISABLED_REFUSAL.to_string())
        );
        let disabled = engine_removal_note(&EngineRemovalOutcome::Disabled(DISABLED_REFUSAL.to_string()));
        assert!(disabled.contains("STILL RUNNING"), "the operator must learn the process survived: {}", disabled);
        assert!(disabled.contains(DISABLED_REFUSAL), "the engine's own reason is quoted: {}", disabled);
        assert!(disabled.contains("shutdown_on_request"), "and the key that would change it: {}", disabled);
        assert!(disabled.contains("config.json"), "and where that key lives: {}", disabled);

        // 3. Denied by the caller gate, or no answer at all: nothing was told
        //    to exit, so the engine may still be running.
        assert_eq!(
            classify_engine_shutdown(&shutdown_result(false, GATE_REFUSAL)),
            EngineRemovalOutcome::Denied(GATE_REFUSAL.to_string())
        );
        let denied = engine_removal_note(&EngineRemovalOutcome::Denied(GATE_REFUSAL.to_string()));
        assert!(denied.contains("did not accept"), "{}", denied);
        assert!(denied.contains(GATE_REFUSAL), "the engine's reason is quoted: {}", denied);
        assert!(denied.contains("may still be running"), "an unaccepted request is not a stopped engine: {}", denied);

        // Missing detail is reported as missing, never invented into a reason.
        assert_eq!(
            classify_engine_shutdown(&shutdown_result(false, "")),
            EngineRemovalOutcome::Denied("(no detail)".to_string())
        );
        // The classification keys on the config key, so a reworded refusal
        // still lands in the right bucket.
        assert!(matches!(
            classify_engine_shutdown(&shutdown_result(false, "refused: shutdown_on_request is off")),
            EngineRemovalOutcome::Disabled(_)
        ));

        // All three end the same way — the TUI forgets the engine regardless,
        // because a refusal is the engine's policy, not a failed removal — and
        // they are three genuinely different lines.
        for note in [&accepted, &disabled, &denied] {
            assert!(note.contains("forgetting"), "every variant must say the TUI forgot it: {}", note);
        }
        assert_ne!(accepted, disabled);
        assert_ne!(disabled, denied);
        assert_ne!(accepted, denied);
    }

    /// Each outcome really does finish the removal, not just produce a line —
    /// the three paths through the real event handler, since that is the only
    /// place the answer is read.
    #[test]
    fn every_outcome_finishes_the_removal_the_same_way() {
        for result in [
            shutdown_result(true, ""),
            shutdown_result(false, DISABLED_REFUSAL),
            shutdown_result(false, GATE_REFUSAL),
        ] {
            let why = if result.success { "accepted" } else { &result.error }.to_string();
            let mut s = engine_state();
            let (tx, mut rx) = mpsc::unbounded_channel::<WsCommand>();
            begin_engine_removal(&mut s, &tx);
            let _ = rx.try_recv();
            handle_ws_event(
                WsEvent::QueryResult {
                    query_id: ENGINE_SHUTDOWN_QUERY.to_string(),
                    result,
                },
                &mut s,
                &tx,
                &no_broadcast(),
            );
            assert!(s.engine_forgotten(), "the removal must finish: {}", why);
            assert!(s.stats.engine_removed, "the removal must finish: {}", why);
            assert!(matches!(rx.try_recv(), Ok(WsCommand::Disconnect)), "{}", why);
        }
    }

    /// A socket that dies under an unanswered request is the same third answer
    /// arriving a different way, and a request that is never answered at all
    /// must not leave the removal half-done. Without these two the TUI can sit
    /// on "asked, never resolved" for good — with the client still dialling an
    /// engine it was told to forget.
    #[test]
    fn an_unanswered_request_still_finishes_the_removal() {
        // The socket closed first.
        let mut s = engine_state();
        let (tx, mut rx) = mpsc::unbounded_channel::<WsCommand>();
        begin_engine_removal(&mut s, &tx);
        assert!(s.pending_engine_removal.is_some());
        let _ = rx.try_recv();
        handle_ws_event(WsEvent::Disconnected, &mut s, &tx, &no_broadcast());
        assert!(s.pending_engine_removal.is_none());
        assert!(s.engine_forgotten(), "the removal still completes");
        assert!(s.stats.engine_removed);
        assert!(matches!(rx.try_recv(), Ok(WsCommand::Disconnect)));

        // The engine went quiet without dropping the socket: the deadline the
        // main loop's ticker acts on.
        let mut s = engine_state();
        let (tx2, _rx2) = mpsc::unbounded_channel::<WsCommand>();
        begin_engine_removal(&mut s, &tx2);
        let deadline = s.pending_engine_removal.expect("pending");
        assert!(
            !removal_deadline_passed(Some(deadline), deadline - Duration::from_millis(1)),
            "a fresh request has time"
        );
        assert!(removal_deadline_passed(Some(deadline), deadline));
        assert!(
            !removal_deadline_passed(None, Instant::now() + Duration::from_secs(600)),
            "no request in flight, no deadline"
        );
        // Nothing is removed by the deadline ALONE — it only decides that the
        // wait is over; the finish is the same call the answer path uses.
        assert!(!s.engine_forgotten());
        if removal_deadline_passed(s.pending_engine_removal, deadline) {
            s.pending_engine_removal = None;
            finish_engine_removal(
                &mut s,
                &tx2,
                EngineRemovalOutcome::Denied(format!("no answer within {}s", ENGINE_SHUTDOWN_TIMEOUT.as_secs())),
            );
        }
        assert!(s.engine_forgotten());
    }

    /// The two ends of the removal switch must be the SAME flag. A structural
    /// check, because the failure is invisible: both `AppState` and `WsClient`
    /// construct their own `Arc<AtomicBool>`, so forgetting the hand-off in
    /// `main()` leaves a client that reconnects forever to an engine the TUI
    /// believes it has forgotten — every behavioural test still passing, because
    /// each one drives one end.
    #[test]
    fn the_app_and_the_client_share_one_removal_switch() {
        let state = AppState::new(
            crate::colors::load_colors(&std::path::PathBuf::from("")),
            crate::hotkeys::default_hotkeys(),
        );
        let (tx, _rx) = mpsc::unbounded_channel::<WsEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<WsCommand>();
        let mut client = ws_client::WsClient::new("127.0.0.1".into(), 1, 0, tx, cmd_rx);

        // A fresh client is NOT already stopped: the switch is shared, not
        // pre-set by the constructor.
        assert!(!client.is_stopped());
        // The hand-off main() performs.
        client.stopped = state.engine_detached.clone();

        let mut state = state;
        state.detach_engine();
        assert!(state.engine_forgotten());
        assert!(
            client.is_stopped(),
            "raising the switch on the app must stop the client: they are one flag"
        );
    }

    /// A press while a request is already in flight must not stack a second
    /// one, and a press on an engine already forgotten must say so.
    #[test]
    fn a_second_remove_press_does_not_stack_a_second_request() {
        let mut s = engine_state();
        let (tx, mut rx) = mpsc::unbounded_channel::<WsCommand>();
        begin_engine_removal(&mut s, &tx);
        let _ = rx.try_recv();
        begin_engine_removal(&mut s, &tx);
        assert!(rx.try_recv().is_err(), "only one request may be outstanding");
        assert!(s.pending_engine_removal.is_some());

        // Already gone.
        finish_engine_removal(&mut s, &tx, EngineRemovalOutcome::Accepted);
        let _ = rx.try_recv();
        begin_engine_removal(&mut s, &tx);
        assert!(rx.try_recv().is_err(), "a forgotten engine cannot be removed again");
        assert!(s.stats.engine_removed);
    }

    /// Nothing to ask means nothing to wait for: a disconnected engine is
    /// removed without a round trip, and the operator is still told that
    /// nothing was asked of a running process.
    #[test]
    fn a_disconnected_engine_is_removed_without_asking() {
        let mut s = engine_state();
        s.connected = false;
        let (tx, mut rx) = mpsc::unbounded_channel::<WsCommand>();
        begin_engine_removal(&mut s, &tx);
        assert!(s.engine_forgotten());
        assert!(s.stats.engine_removed);
        // No query: there was no engine to ask.
        assert!(matches!(rx.try_recv(), Ok(WsCommand::Disconnect)));
        let note = engine_removal_note(&EngineRemovalOutcome::Denied(
            "not connected to an engine, so there was nothing to ask".to_string(),
        ));
        assert!(note.contains("did not accept"), "{}", note);
    }

    /// After the removal, nothing from the dead connection may put an engine
    /// back. The late `Disconnected` is the real one: the engine answers FIRST
    /// and exits second, so its close always arrives after the removal. A stats
    /// update is the dangerous one — it replaces the whole `GlobalStats` and
    /// would resurrect every cleared number.
    #[test]
    fn a_late_engine_event_cannot_revive_a_removed_engine() {
        let mut s = engine_state();
        let (tx, _rx) = mpsc::unbounded_channel::<WsCommand>();
        finish_engine_removal(&mut s, &tx, EngineRemovalOutcome::Accepted);

        handle_ws_event(WsEvent::Connected, &mut s, &tx, &no_broadcast());
        assert!(!s.connected, "a forgotten engine cannot reconnect");
        assert!(s.stats.engine_removed);
        assert_ne!(s.stats.engine_status, "connected");

        handle_ws_event(WsEvent::Disconnected, &mut s, &tx, &no_broadcast());
        assert!(s.stats.engine_removed);
        assert_ne!(s.stats.engine_status, "connected");

        // A whole `GlobalStats` from the dead engine: exactly the shape that
        // would resurrect every cleared number if it were not dropped.
        let stale = db::GlobalStats {
            engine_status: "connected".to_string(),
            module_entries: engine_state().stats.module_entries,
            total_messages: 99,
            ..Default::default()
        };
        handle_ws_event(WsEvent::StatsUpdate(stale), &mut s, &tx, &no_broadcast());
        assert!(s.stats.module_entries.is_empty(), "a stale stats update must not come back");
        assert_eq!(s.stats.total_messages, 0);

        // Even the connection details, which arrive on every (re)connect.
        handle_ws_event(
            WsEvent::ConnectionInfo { ip: "10.0.0.1".into(), port: 9734, pin: 42 },
            &mut s,
            &tx,
            &no_broadcast(),
        );
        assert!(s.stats.connection.ip.is_empty() && s.stats.connection.port == 0);

        // And a prompt from the engine it no longer talks to is not queued.
        handle_ws_event(
            WsEvent::Prompt(cockatiel_client::proto::Prompt::default()),
            &mut s,
            &tx,
            &no_broadcast(),
        );
        assert!(s.pending_prompt.is_empty());
    }

    /// "Remove the engine" is a TUI-side forgetting and this is the assertion
    /// that keeps it one. The verb sits one key away from `DeleteModule`, which
    /// really does destroy a module's registration, so the removal path must
    /// contain no file writing, no registry editing and no process killing at
    /// all. (Structural, because there is no honest way to observe "no file was
    /// written" other than reading the code that does the writing.)
    #[test]
    fn removing_the_engine_never_touches_the_engines_files_or_process() {
        let src = include_str!("main.rs");
        let start = src.find("fn finish_engine_removal(").expect("finish_engine_removal");
        let end = start + src[start..].find("\n}\n").expect("end of the fn");
        let body = &src[start..end];
        for forbidden in [
            "write_atomic_0600",
            "remove_file",
            "clear_module_config",
            "remove_from_ordering",
            "engine_config_path",
            "engine_env_path",
            "engine_dir",
            ".kill()",
            "launch_engine",
        ] {
            assert!(
                !body.contains(forbidden),
                "the removal must not touch {:?} — it forgets the engine, it does not destroy it:\n{}",
                forbidden,
                body
            );
        }
        // What it DOES do, in this order: report, stop the client, close the
        // socket, clear the TUI's facts.
        let report = body.find("engine_removal_note").expect("the answer must be reported");
        let stop = body.find("detach_engine()").expect("the client must be stopped");
        let close = body.find("WsCommand::Disconnect").expect("the socket must be closed");
        let forget = body.find("forget_engine()").expect("the TUI's facts must be cleared");
        assert!(report < stop && stop < close && close < forget, "the order is the sequence:\n{}", body);
    }

    /// The two engine-only keys are refused from a MODULE row, and from a
    /// window with no row concept at all — the app's fallback there is "the
    /// first known module", so an unguarded press would act on something the
    /// operator never selected.
    #[test]
    fn the_engine_only_keys_are_refused_off_the_engine_row() {
        use crate::app::WindowId;
        let mut on_module = engine_state();
        select_module_row(&mut on_module);
        assert!(focused_selection_is_module(&on_module));
        for a in [Action::RemoveEngine, Action::RestartEngine] {
            assert!(is_engine_scoped(&a), "{:?} is engine-scoped", a);
            assert!(is_dispatchable(&a), "{:?} must be dispatched", a);
            assert!(!focused_selection_is_engine(&on_module), "a module row is not the engine row");
        }
        // A module action is not engine-scoped: the two lists do not overlap.
        assert!(!is_engine_scoped(&Action::StartModule(String::new())));
        assert!(!is_engine_scoped(&Action::EditConfig(String::new())));

        // The engine row, and a window with no row at all.
        assert!(focused_selection_is_engine(&engine_state()));
        let mut other = engine_state();
        other.windows = vec![Box::new(crate::windows::LogWindow::new())];
        other.active_window = WindowId::Log;
        assert!(!focused_selection_is_engine(&other));

        // ...and through the real dispatch, a refused press sends nothing.
        let mut s = engine_state();
        select_module_row(&mut s);
        let (tx, mut rx) = mpsc::unbounded_channel::<WsCommand>();
        let mut supervisor: supervisor::ProcessTable = Default::default();
        let mut plugins: Vec<crate::plugins::Plugin> = Vec::new();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for a in [Action::RemoveEngine, Action::RestartEngine] {
                dispatch_action(
                    &mut s,
                    a,
                    &mut supervisor,
                    &mut plugins,
                    0,
                    0,
                    &tx,
                    "127.0.0.1:1".parse().unwrap(),
                    "",
                )
                .await
                .unwrap();
            }
        });
        assert!(rx.try_recv().is_err(), "a refused press must send nothing");
        assert!(!s.engine_forgotten() && !s.stats.engine_removed);
        assert!(s.pending_engine_removal.is_none());
    }

    /// Move the selection off the engine row and onto the first module. In the
    /// grouped list the first Down lands on the [PRE-PROCESS] header, so keep
    /// pressing until a row that IS a module is selected.
    fn select_module_row(state: &mut AppState) {
        use crate::app::WindowId;
        let mut stats = std::mem::take(&mut state.stats);
        if let Some(w) = state.get_window_mut(WindowId::Modules) {
            for _ in 0..8 {
                w.handle_key(
                    crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Down,
                        crossterm::event::KeyModifiers::empty(),
                    ),
                    &mut stats,
                );
                if w.selection_is_module(&stats) {
                    break;
                }
            }
        }
        state.stats = stats;
    }

    // ── restarting the engine ─────────────────────────────────────────────

    /// Restart: kill the tracked child, launch a new one, and register it under
    /// the SAME key the original launch uses. That key is the load-bearing part
    /// — it is what `drain()` reaps at teardown, so a restart that registered
    /// under any other name is exactly how a restart becomes an orphan.
    ///
    /// The relaunch is injected (a unit test may not start a real engine), so
    /// what is covered is the sequence and the registration. The `Command::spawn`
    /// itself is the part that is not unit-testable, and the same is true of the
    /// reconnect: it is the client's own backoff, the path an engine crash
    /// already takes.
    #[test]
    fn a_restart_kills_the_old_child_and_re_registers_the_new_one() {
        let s = engine_state();
        let mut table: supervisor::ProcessTable = Default::default();
        // A real, harmless child: `ManagedProcess` holds a `std::process::Child`,
        // which cannot be faked, and the KILL is part of what is being tested.
        let old_pid = track_stand_in_engine(&mut table);

        let new = stand_in_child();
        let new_pid = new.id();
        let outcome = restart_engine(&s, &mut table, || Ok(new));

        assert_eq!(outcome, RestartOutcome::Relaunched { old_pid, pid: new_pid });
        // Tracked under the launch key, so teardown reaps it.
        let registered = table.get(ENGINE_PROCESS).expect("the new engine must be registered");
        assert_eq!(registered.lock().unwrap().pid(), new_pid);
        assert_eq!(table.len(), 1, "exactly one supervised engine");
        // ...and the old process is really gone, not merely untracked.
        assert!(
            !supervisor::pid_alive(old_pid as i32),
            "the old engine must be killed, not orphaned"
        );
        // The operator is told what happened to the OLD one as well.
        let note = restart_note(&outcome);
        assert!(note.contains(&old_pid.to_string()), "{}", note);
        assert!(note.contains(&new_pid.to_string()), "{}", note);
        // A restart never detaches the client, so the engine can come back.
        assert!(!s.engine_forgotten());

        registered.lock().unwrap().kill();
    }

    /// The two refusals are different situations and must not be reported as
    /// the same thing: one is "there is no engine", the other is "there is an
    /// engine and it is not ours to kill".
    #[test]
    fn a_restart_is_refused_when_there_is_nothing_to_restart() {
        // Removed from the TUI. Launching one here would create precisely the
        // orphan the removal exists to prevent: the client was told never to
        // reconnect, so a fresh engine would have nobody to talk to.
        let mut s = engine_state();
        s.stats.forget_engine();
        let mut table: supervisor::ProcessTable = Default::default();
        assert_eq!(
            restart_engine(&s, &mut table, || Err("must not launch".to_string())),
            RestartOutcome::Removed
        );
        let removed = restart_note(&RestartOutcome::Removed);
        assert!(removed.contains("removed"), "{}", removed);
        assert!(removed.contains("E"), "the config editor is the way back: {}", removed);

        // Present but not ours — `--no-engine`, or it was already running. The
        // TUI does not launch, so it does not kill.
        let s = engine_state();
        let mut table: supervisor::ProcessTable = Default::default();
        assert_eq!(
            restart_engine(&s, &mut table, || Err("must not launch".to_string())),
            RestartOutcome::NotSupervised
        );
        let not_ours = restart_note(&RestartOutcome::NotSupervised);
        assert!(not_ours.contains("did not launch"), "{}", not_ours);
        assert!(not_ours.contains("--no-engine"), "and how to change that: {}", not_ours);

        // A relaunch that fails says so, and says the config is untouched. The
        // old engine is already dead at that point, which the operator has to be
        // told rather than left to infer from a silence.
        let mut table: supervisor::ProcessTable = Default::default();
        let old_pid = track_stand_in_engine(&mut table);
        let outcome = restart_engine(&s, &mut table, || Err("binary not found".to_string()));
        assert_eq!(outcome, RestartOutcome::Failed { old_pid, error: "binary not found".into() });
        let note = restart_note(&outcome);
        assert!(note.contains("binary not found"), "{}", note);
        assert!(note.contains("untouched"), "{}", note);
        assert!(
            !table.contains_key(ENGINE_PROCESS),
            "a dead engine must not be left looking supervised"
        );
    }

    /// A stand-in engine process, tracked exactly the way the real launch site
    /// tracks the real one (same key, same `ManagedProcess` shape).
    ///
    /// Its own process group, like every supervisor child, so the group TERM in
    /// `ManagedProcess::kill` actually reaches it. Without that the kill would
    /// fall through to the 3s wait-for-child timeout on every run, and the
    /// stand-in's own `sleep` would be left behind.
    fn track_stand_in_engine(table: &mut supervisor::ProcessTable) -> u32 {
        let child = stand_in_child();
        let pid = child.id();
        table.insert(
            ENGINE_PROCESS.to_string(),
            Arc::new(Mutex::new(supervisor::ManagedProcess {
                child,
                terminal_window: None,
                terminal_pidfile: None,
            })),
        );
        pid
    }

    fn stand_in_child() -> std::process::Child {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.spawn().expect("spawn a stand-in engine")
    }

    // ── engine start / stop (`s` / `x` on the engine row) ───────────────

    #[test]
    fn starting_an_engine_launches_and_supervises_it() {
        let s = engine_state();
        let mut table: supervisor::ProcessTable = Default::default();
        let outcome = start_engine(&s, &mut table, || Ok(stand_in_child()), || false);
        let EngineStartOutcome::Launched { pid } = outcome else {
            panic!("expected a launch, got {outcome:?}");
        };
        let note = engine_start_note(&outcome);
        assert!(note.contains("launched"), "{}", note);
        assert!(note.contains(&format!("{pid}")), "names the pid: {}", note);
        assert!(
            table.contains_key(ENGINE_PROCESS),
            "a launched engine must be supervised under {}",
            ENGINE_PROCESS
        );
        // A fresh engine boots paused, so "start" must also re-open the
        // pipeline — that is the whole point of pressing start.
        assert!(
            outcome.resume_pipeline(),
            "launching the engine must resume the pipeline"
        );
        assert!(
            engine_start_note(&outcome).contains("resuming"),
            "the launch note must say the pipeline is being resumed"
        );
        // The next start press sees it and declines to double-launch, and —
        // the engine now believed running (not paused) — must NOT pause
        // anything it wasn't already pausing.
        let mut s_running = engine_state();
        s_running.stats.pipeline_paused = false;
        assert_eq!(
            start_engine(&s_running, &mut table, || panic!("must not launch twice"), || false),
            EngineStartOutcome::AlreadySupervised { resumed: false }
        );
        let _ = table.remove(ENGINE_PROCESS);
    }

    #[test]
    fn start_is_refused_after_removal_and_defers_to_a_running_engine() {
        // Removed: the client is stopped forever, so launching a fresh engine
        // would orphan it. Refuse rather than relaunch.
        let mut s = engine_state();
        s.stats.forget_engine();
        let mut table: supervisor::ProcessTable = Default::default();
        assert_eq!(
            start_engine(&s, &mut table, || panic!("must not launch after removal"), || false),
            EngineStartOutcome::Removed
        );
        let note = engine_start_note(&EngineStartOutcome::Removed);
        assert!(note.contains("removed"), "{}", note);

        // Something already listening on the port: connect, don't double-start.
        // The engine state BELIEVES it is paused (boot default is paused), so
        // the outcome must demand a resume — this is the exact bug report:
        // "already listening on port" while the UI still says paused.
        let s = engine_state();
        let mut table: supervisor::ProcessTable = Default::default();
        let outcome =
            start_engine(&s, &mut table, || panic!("must not launch over a live port"), || true);
        assert_eq!(outcome, EngineStartOutcome::AlreadyRunning { resumed: true });
        assert!(
            outcome.resume_pipeline(),
            "an already-listening-but-paused engine must be resumed on start"
        );
        assert!(!table.contains_key(ENGINE_PROCESS), "no process registered");
        let note = engine_start_note(&outcome);
        assert!(note.contains("already listening"), "{}", note);
        assert!(note.contains("resuming"), "the note must say the pipeline resumes: {}", note);

        // The same engine, not paused: start is a no-op that must not resume
        // (nothing to resume) and must not pause either.
        let mut s_running = engine_state();
        s_running.stats.pipeline_paused = false;
        let outcome = start_engine(
            &s_running,
            &mut table,
            || panic!("must not launch over a live port"),
            || true,
        );
        assert_eq!(outcome, EngineStartOutcome::AlreadyRunning { resumed: false });
        assert!(!outcome.resume_pipeline());
        let note = engine_start_note(&outcome);
        assert!(note.contains("pipeline is open"), "{}", note);
    }

    #[test]
    fn start_failure_leaves_nothing_supervised() {
        let s = engine_state();
        let mut table: supervisor::ProcessTable = Default::default();
        let outcome =
            start_engine(&s, &mut table, || Err("port busy".to_string()), || false);
        assert_eq!(outcome, EngineStartOutcome::Failed { error: "port busy".into() });
        assert!(
            !table.contains_key(ENGINE_PROCESS),
            "a failed launch must not be left looking supervised"
        );
        let note = engine_start_note(&outcome);
        assert!(note.contains("port busy"), "{}", note);
        assert!(note.contains("untouched"), "{}", note);
    }

    #[test]
    fn stopping_an_engine_kills_the_supervised_child() {
        let s = engine_state();
        let mut table: supervisor::ProcessTable = Default::default();
        let pid = track_stand_in_engine(&mut table);
        let outcome = stop_engine(&s, &mut table);
        assert_eq!(outcome, EngineStopOutcome::Stopped { pid });
        assert!(!table.contains_key(ENGINE_PROCESS), "killed engine must be deregistered");
        let note = engine_stop_note(&outcome);
        assert!(note.contains("killed"), "{}", note);
    }

    #[test]
    fn stop_is_refused_when_there_is_no_engine_to_own() {
        // Removed: nothing to stop.
        let mut s = engine_state();
        s.stats.forget_engine();
        let mut table: supervisor::ProcessTable = Default::default();
        assert_eq!(stop_engine(&s, &mut table), EngineStopOutcome::Removed);
        let note = engine_stop_note(&EngineStopOutcome::Removed);
        assert!(note.contains("already removed"), "{}", note);

        // Present but not ours (the TUI started `--no-engine`, or connected to
        // an engine it did not launch): do not kill a process we do not own.
        let s = engine_state();
        let mut table: supervisor::ProcessTable = Default::default();
        assert_eq!(stop_engine(&s, &mut table), EngineStopOutcome::NotSupervised);
        let note = engine_stop_note(&EngineStopOutcome::NotSupervised);
        assert!(note.contains("did not launch"), "{}", note);
    }

    // ── --with-engine / --no-engine ───────────────────────────────────────

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The switch parses as a switch — it takes no value, so it must not eat the
    /// next argument — and the last one given wins, so the pair reads left to
    /// right.
    #[test]
    fn the_engine_launch_switches_parse() {
        // Neither: nothing was said, which is what lets the config decide.
        assert_eq!(parse_cli(&args(&[])).engine_launch, None);
        assert_eq!(parse_cli(&args(&["--with-engine"])).engine_launch, Some(true));
        assert_eq!(parse_cli(&args(&["--no-engine"])).engine_launch, Some(false));
        assert_eq!(parse_cli(&args(&["--no-engine", "--with-engine"])).engine_launch, Some(true));
        assert_eq!(parse_cli(&args(&["--with-engine", "--no-engine"])).engine_launch, Some(false));
        // A value flag right after still finds its value.
        let mixed = parse_cli(&args(&["--no-engine", "--port", "1234", "--with-engine"]));
        assert_eq!(mixed.engine_launch, Some(true));
        assert_eq!(mixed.override_port, Some(1234));
    }

    /// The pre-existing flags must parse exactly as they did. A refactor of the
    /// hand-rolled loop is precisely where a working flag goes missing without
    /// anyone noticing, and `-p` is `--port` — NOT a free short flag for
    /// anything new.
    #[test]
    fn the_pre_existing_flags_still_parse() {
        let full = parse_cli(&args(&[
            "--detached",
            "log",
            "--ws-addr",
            "127.0.0.1:9",
            "--ws-token",
            "tok",
            "--ip",
            "1.2.3.4",
            "-i",
            "5.6.7.8",
            "--port",
            "9999",
            "-p",
            "1111",
            "--pin",
            "2222",
        ]));
        assert_eq!(
            full,
            CliArgs {
                detached_window: Some("log".into()),
                ws_parent_addr: Some("127.0.0.1:9".into()),
                ws_parent_token: Some("tok".into()),
                // `-i` is the last one given, so it wins over `--ip`.
                override_ip: Some("5.6.7.8".into()),
                // ...and `-p` is `--port`, the same as it always was.
                override_port: Some(1111),
                override_pin: Some(2222),
                engine_launch: None,
            }
        );
        assert_eq!(parse_cli(&args(&["-p", "1234"])).override_port, Some(1234));
        assert_eq!(parse_cli(&args(&["--pin", "4321"])).override_pin, Some(4321));
        // A value flag with nothing after it is stepped over, not a panic — and
        // an argument that LOOKS like a flag is still consumed as the value,
        // which is the leniency (and the sharp edge) the loop always had.
        let dangling = parse_cli(&args(&["--port", "--pin", "77"]));
        assert_eq!(dangling.override_port, None, "'--pin' is eaten as the port");
        assert_eq!(dangling.override_pin, None, "...so the pin is lost with it");
        let trailing = parse_cli(&args(&["--port"]));
        assert_eq!(trailing.override_port, None, "a trailing flag is simply ignored");
        // Unknown arguments are ignored rather than fatal.
        assert_eq!(parse_cli(&args(&["--wat", "1", "--no-engine"])).engine_launch, Some(false));
    }

    /// The precedence: the flag beats the config, the config beats the built-in
    /// default, and the built-in default is LAUNCH — starting without the engine
    /// is the special case, never the accident.
    #[test]
    fn the_flag_beats_the_config_and_the_default_is_to_launch() {
        // The whole matrix, spelled out.
        for (flag, config, expected) in [
            (Some(true), Some(true), true),
            (Some(true), Some(false), true),
            (Some(true), None, true),
            (Some(false), Some(true), false),
            (Some(false), Some(false), false),
            (Some(false), None, false),
            (None, Some(true), true),
            (None, Some(false), false),
            // No flag, no key: launch. This is the row that must never change.
            (None, None, true),
        ] {
            assert_eq!(
                should_launch_engine(flag, config),
                expected,
                "flag={:?} config={:?} must be {}",
                flag,
                config,
                expected
            );
        }
    }

    /// The probe and the flag compose: neither alone is enough. Launching over an
    /// engine that is already listening means two engines and one port, and
    /// launching when `--no-engine` was typed means starting the exact process
    /// the operator said not to start.
    #[test]
    fn the_probe_and_the_flag_have_to_agree_before_anything_is_launched() {
        // (already_up, should_launch) -> launch?
        for (up, want, expected) in [
            (false, true, true),  // the ordinary case: nothing there, we want one
            (true, true, false),  // somebody's engine is already listening
            (false, false, false), // --no-engine
            (true, false, false),  // --no-engine, and there is one to use anyway
        ] {
            assert_eq!(
                engine_start_decision(up, want),
                expected,
                "already_up={} should_launch={}",
                up,
                want
            );
        }
    }

    /// `--no-engine` is about the ENGINE and nothing else. The user database and
    /// the module registration must stay outside the gate, or pointing the TUI
    /// at somebody else's engine silently costs it its database and its modules
    /// — and the engine needs both. Structural, because the launches are
    /// processes: the assertion is about what the guard is wrapped around.
    #[test]
    fn no_engine_gates_the_engine_launch_and_nothing_else() {
        let src = include_str!("main.rs");
        let start = src.find("if engine_start_decision(").expect("the launch gate");
        let end = start + src[start..].find("\n        }\n").expect("end of the guarded block");
        let block = &src[start..end];

        // What is inside: the engine, and the wait for its config afterwards.
        assert!(block.contains("supervisor::launch_engine()"), "the gate is on the engine launch:\n{}", block);
        // What is NOT: the other two services, and the plugin discovery the
        // engine needs to approve them.
        for outside in [
            "launch_user_db",
            "register_module",
            "add_to_ordering",
            "discover_plugins",
        ] {
            assert!(
                !block.contains(outside),
                "{:?} must not be behind the engine-launch gate:\n{}",
                outside,
                block
            );
        }
        // And the user database is launched BEFORE the gate, unconditionally,
        // on the way to it.
        let db_at = src.find("supervisor::launch_user_db()").expect("the user database launch");
        assert!(db_at < start, "the user database is not behind the engine gate");
        // The gate is the decision function, not a bare boolean: a later edit
        // cannot quietly drop the "already running" half of the condition.
        assert!(src.contains("engine_start_decision(engine_up, should_launch)"));
    }
}
