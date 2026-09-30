use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Global queue of supervisor messages (start/stop/build/crash events) that the
/// main loop drains into the log window. Kept out of the terminal so starting
/// modules never scribbles clear text over the ratatui screen.
pub static SUPERVISOR_LOGS: OnceLock<Arc<Mutex<VecDeque<crate::windows::log::LogEntry>>>> =
    OnceLock::new();

/// Push a supervisor message into the global log queue (shown in the log
/// window, not the terminal).
pub fn supervisor_log_global(message: String) {
    let q = SUPERVISOR_LOGS.get_or_init(|| Arc::new(Mutex::new(VecDeque::new())));
    let mut q = q.lock().unwrap();
    q.push_back(crate::windows::log::LogEntry {
        timestamp: String::new(),
        source: "supervisor".to_string(),
        message,
        event_type: 1,
    });
    while q.len() > 500 {
        q.pop_front();
    }
}

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use cockatiel_client::proto::Prompt;

use crate::colors::ColorConfig;
use crate::hotkeys::{Action, HotkeyConfig};
use crate::layout::LayoutState;
use crate::db::GlobalStats;
use crate::windows::log::LogEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowId {
    Logo,
    Log,
    Modules,
    Chart,
    Prompts,
    /// Detached pop-out user database view. NOT part of the embedded window
    /// cycle (`all()`), so it never joins the focus rotation.
    Users,
}

impl WindowId {
    pub fn all() -> &'static [WindowId] {
        &[WindowId::Logo, WindowId::Log, WindowId::Modules, WindowId::Chart, WindowId::Prompts]
    }

    pub fn name(&self) -> &'static str {
        match self {
            WindowId::Logo => "logo",
            WindowId::Log => "log",
            WindowId::Modules => "modules",
            WindowId::Chart => "chart",
            WindowId::Prompts => "prompts",
            WindowId::Users => "users",
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            WindowId::Logo => "cockatiel",
            WindowId::Log => "log",
            WindowId::Modules => "modules",
            WindowId::Chart => "message chart",
            WindowId::Prompts => "prompts",
            WindowId::Users => "user database",
        }
    }
}

/// An unanswered prompt from the engine (or another module), shown as a dialog.
pub struct PendingPrompt {
    pub prompt: Prompt,
    pub deadline: Instant,
    /// Typed text for free-text prompts (`prompt.input_label` is non-empty).
    pub text_input: String,
}

/// A credential-entry session answered through the prompt subwindow: one
/// PendingPrompt per credential field. When every field has been answered, the
/// collected values are sent to the engine as a `set_credentials` query.
pub struct CredentialSession {
    /// Module being configured.
    pub module_name: String,
    /// (credential field key, prompt_id_uuid7) for each open prompt.
    pub fields: Vec<(String, String)>,
    /// Values collected so far, keyed by field key.
    pub collected: HashMap<String, String>,
}

/// Which config a config-editor session is pointed at.
///
/// The engine owns a `.env` + `config.json` exactly like a module does, but it
/// is not a plugin — no manifest, no discovered directory, nothing the module
/// list can name. So it is a target in its own right rather than a special
/// name: the SAME editor is opened against a different directory, and the
/// post-save restart warning differs (the engine has boot-bound settings, a
/// module only needs relaunching).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigTarget {
    Engine,
    Module,
    /// The user database's own config.json (its rank decay, score divisor,
    /// etc.). The user-db is self-contained and reads only this file.
    UserDb,
}

pub trait Window {
    fn id(&self) -> WindowId;
    fn render(&mut self, area: Rect, buf: &mut Buffer, is_active: bool, stats: &GlobalStats, colors: &ColorConfig, hotkeys: &HotkeyConfig, prompts: &[PendingPrompt]);
    fn handle_key(&mut self, _key: KeyEvent, _stats: &mut GlobalStats) -> Option<Action> { None }
    fn handle_mouse(&mut self, _mouse: MouseEvent, _area: Rect) -> Option<Action> { None }
    /// The module currently selected in this window (used to fill in the name
    /// for module actions resolved from the hotkey config).
    fn selected_module_name(&self, _stats: &GlobalStats) -> Option<String> { None }
    /// Whether the focused window's selection IS a module, i.e. whether the
    /// per-module actions (start/stop/delete/...) have anything to act on.
    ///
    /// `None` from [`Window::selected_module_name`] is not enough to decide
    /// this: the app falls back to the first known module when no window
    /// claims a selection, so a window whose selected row is deliberately not
    /// a module has to say so explicitly or "stop" ends up killing a module
    /// the operator is not even looking at.
    fn selection_is_module(&self, _stats: &GlobalStats) -> bool { true }
    /// Whether the focused window's selection IS the engine row — the mirror of
    /// [`Window::selection_is_module`] for the two actions that exist only to
    /// act on the engine. Default `false` because "this window has no notion of
    /// the engine row" must not read as "the engine row is selected" any more
    /// than it reads as "a module is selected".
    fn selection_is_engine(&self, _stats: &GlobalStats) -> bool { false }
    /// Re-anchor the selection onto a module by name, so the cursor follows a
    /// module that was moved to another group (Shift+up/down stage moves).
    /// Default no-op; only the modules window tracks a module selection it can
    /// move.
    fn select_module_name(&mut self, _name: &str, _stats: &GlobalStats) {}
    /// A clickable link rendered by this window (e.g. a prompt's link), if any.
    fn pending_link(&self) -> Option<(Rect, String)> { None }
    /// Append a log entry to this window (the log window displays them).
    fn push_log(&mut self, _entry: LogEntry) {}
    /// Which prompt in the queue this window should highlight (only the
    /// prompts window uses this).
    fn set_prompt_selected(&mut self, _idx: usize) {}
    /// Enter the window's inline config editor for `target` (loads `.env` +
    /// `config.json` into editable rows, labelled `label`).
    fn start_config_editor(&mut self, _target: ConfigTarget, _label: &str, _dir: std::path::PathBuf) {}
    /// The config the editor key should open, when THIS window can answer for
    /// its own selection. `None` means "the caller decides" — which is what a
    /// module row wants, because plugin directories live with the supervisor
    /// and not in the window. The engine row answers here, because no plugin
    /// list contains the engine.
    fn config_editor_target(&self, _stats: &GlobalStats) -> Option<(ConfigTarget, String, std::path::PathBuf)> { None }
    /// True when the window's config editor is active (it consumes all keys).
    fn in_editor(&self) -> bool { false }
    /// Handle a key while the config editor is active. Returns true when the
    /// key was consumed by the editor.
    fn editor_key(&mut self, _key: KeyEvent, _hotkeys: &HotkeyConfig) -> bool { false }
    /// Paste text into the active config editor (at the cursor). Consumed?
    fn editor_paste(&mut self, _text: &str) -> bool { false }
    /// The module whose config the editor most recently saved, if any (cleared
    /// when read). Lets the app warn that the module must be restarted.
    fn take_saved_module(&mut self) -> Option<String> { None }
    /// The engine config keys the editor most recently saved that will NOT
    /// take effect until the engine restarts (cleared on read). The engine
    /// mirror of [`Window::take_saved_module`]: both answer "what did the last
    /// save just invalidate", one per restart unit.
    fn take_saved_engine(&mut self) -> Option<Vec<crate::windows::modules::EngineRestartNote>> { None }
}

/// Cheap fingerprint of the screen's shape. See [`AppState::screen_shape`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenShape {
    pub width: u16,
    pub height: u16,
    pub left_width_pct: u16,
    pub top_height_pct: u16,
    pub log_height_pct: u16,
    pub prompts_width_pct: u16,
    pub window_count: usize,
    pub active: WindowId,
    pub editing: bool,
    pub prompting: bool,
    pub prompt_count: usize,
}

pub struct AppState {
    pub active_window: WindowId,
    pub windows: Vec<Box<dyn Window>>,
    /// The configured terminal emulator (from the TUI's config.json
    /// `terminal_emulator` key). Empty/None = the system default. Used to open
    /// terminal modules and pop-out windows.
    pub terminal_emulator: Option<String>,
    /// Set by the Ctrl+L escape hatch: repaint every cell on the next frame
    /// instead of diffing, so any visual glitch can be cleared by hand.
    pub force_full_redraw: bool,
    pub stats: GlobalStats,
    pub colors: ColorConfig,
    pub hotkeys: HotkeyConfig,
    pub connected: bool,
    /// The websocket client's stop switch, shared with the client task.
    ///
    /// Raised by `Action::RemoveEngine` AFTER the engine has answered, and read
    /// by `WsClient::run` between connection attempts. It is an
    /// `Arc<AtomicBool>` and not a plain `bool` because the client is moved
    /// into a task while the switch is thrown from the main loop, and because a
    /// client that keeps retrying an engine the operator deliberately removed is
    /// exactly the orphaned pop-out this app has already shipped once (see
    /// `ws_client::DETACHED_GIVE_UP_AFTER`).
    pub engine_detached: Arc<AtomicBool>,
    /// When the outstanding `engine_shutdown` request must be given up on, or
    /// `None` when there is no request in flight. Removal is a two-step
    /// handshake — ask, then read the answer — so the request has to be
    /// remembered until the matching `QueryResult` comes back, and a request
    /// that is never answered must not leave the removal half-done.
    pub pending_engine_removal: Option<Instant>,
    pub layout: LayoutState,
    pub popped_out: HashSet<String>,
    /// Active credential-entry session (driven through the prompt subwindow
    /// instead of a pop-up modal). None when no credential entry is in flight.
    pub credential_session: Option<CredentialSession>,
    /// Active config-editing session (driven through the prompt subwindow).
    /// Module to launch once its credential entry completes (the engine saves
    /// the config, then the TUI supervisor starts the process).
    pub pending_launch: Option<String>,
    /// True once the engine's `set_credentials` QueryResult confirms the save.
    /// The main loop only launches `pending_launch` when this is set, so a
    /// failed credential save never launches the module.
    pub pending_launch_confirmed: bool,
    /// Supervisor-side module lifecycle status: "starting" / "connected" /
    /// "crashed" / "stopped". Merged over the engine-reported status at render.
    pub module_runs: Arc<Mutex<HashMap<String, String>>>,
    /// Sender for auto-rebuild requests: a module that crashed unexpectedly is
    /// rebuilt from source (see `rebuild_attempts`). Created by the supervisor.
    pub rebuild_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Consecutive auto-rebuild attempts per module (capped to avoid loops).
    pub rebuild_attempts: Arc<Mutex<HashMap<String, u32>>>,
    /// Sender for runtime-crash events: a module that died AFTER connecting is
    /// recovered by the crash ladder (restart, then rebuild, then rollback).
    pub restart_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Sender for crash-ladder relaunches after the exponential backoff delay
    /// has elapsed. The backoff sleep runs on a background task (never the main
    /// loop), which reports back here with (module, launch mode) when it fires.
    pub retry_tx: Option<tokio::sync::mpsc::UnboundedSender<(String, crate::supervisor::LaunchMode)>>,
    /// Sender for launch results: a background task resolves a module's launch
    /// (possibly building it) and reports back (name, Result<(cmd, args)>).
    /// The main loop spawns the child once it receives the result, so a cold
    /// build never blocks the UI.
    pub launch_tx: Option<tokio::sync::mpsc::UnboundedSender<(String, Result<(String, Vec<String>), String>)>>,
    /// Per-module crash bookkeeping driving the recovery ladder + the
    /// "disable autostart?" prompt.
    pub crash_state: Arc<Mutex<HashMap<String, ModuleCrashState>>>,
    /// Prompts awaiting an answer, rendered as a queue (first = active).
    pub pending_prompt: VecDeque<PendingPrompt>,
    /// Index into `pending_prompt` of the prompt currently focused in the
    /// prompts window (cycled with the left/right arrow keys).
    pub selected_prompt: usize,
    /// When the user last pressed Esc outside a prompt/form (used to detect a
    /// double-Esc quit). Ctrl+C is deliberately NOT bound to quit so that
    /// copy/paste stays safe.
    pub last_esc_press: Option<Instant>,
    /// Lines captured from module stdout/stderr, drained into the log window
    /// each frame (and forwarded to the engine for the timeline database).
    pub module_logs: Arc<Mutex<VecDeque<LogEntry>>>,
    /// When each module last produced an error line (module -> Instant). Used
    /// to surface an "error" status for a still-connected module.
    pub module_errors: Arc<Mutex<HashMap<String, Instant>>>,
    /// When each module was last told to start ("starting"). A module stuck in
    /// "starting" past [`crate::main::STARTUP_FAILED_AFTER`] — without the
    /// engine ever reporting it connected — is surfaced as a terminal
    /// "startup failed" instead of an eternal spinner.
    pub module_started_at: Arc<Mutex<HashMap<String, Instant>>>,
    /// The last error line each module emitted, kept so a "startup failed"
    /// status can explain WHY (the reason a user can act on, instead of
    /// "starting" forever).
    pub module_last_error: Arc<Mutex<HashMap<String, String>>>,
}

/// Per-module recovery state for the crash ladder. A module that survives long
/// enough (>= CRASH_WINDOW_MS connected) resets its consecutive count so a
/// one-off crash goes back to the prebuilt-binary restart path.
#[derive(Debug, Clone, Default)]
pub struct ModuleCrashState {
    pub last_crash_ms: i64,
    pub consecutive: u32,
}

impl ModuleCrashState {
    /// Record a crash and decide how to recover. A module that survived the
    /// full window resets its history (a one-off crash restarts the prebuilt
    /// binary); a repeat crash within the window escalates to a rebuild.
    pub fn record_crash(&mut self, now_ms: i64) -> (bool, u32) {
        if now_ms - self.last_crash_ms >= CRASH_WINDOW_MS {
            self.consecutive = 0;
        }
        self.consecutive += 1;
        self.last_crash_ms = now_ms;
        (self.consecutive >= 2, self.consecutive)
    }
}

/// How long a module must stay crash-free to clear its crash history.
pub const CRASH_WINDOW_MS: i64 = 30 * 60 * 1000;
/// Consecutive crashes before the "disable autostart?" prompt appears.
pub const CONSECUTIVE_CRASH_PROMPT: u32 = 3;
/// Cap for the exponential crash-restart backoff (10 minutes).
pub const CRASH_BACKOFF_MAX_SECS: u64 = 600;

/// Exponential crash-restart backoff: 1s, 2s, 4s, 8s ... capped at
/// `CRASH_BACKOFF_MAX_SECS` (10 min). `consecutive` is the module's crash
/// count within the current window (from `ModuleCrashState::record_crash`).
pub fn crash_backoff(consecutive: u32) -> Duration {
    let shift = (consecutive.saturating_sub(1)).min(10);
    let secs = (1u64 << shift).min(CRASH_BACKOFF_MAX_SECS);
    Duration::from_secs(secs)
}

impl AppState {
    pub fn new(colors: ColorConfig, hotkeys: HotkeyConfig) -> Self {
        Self {
            active_window: WindowId::Logo,
            windows: Vec::new(),
            terminal_emulator: None,
            force_full_redraw: false,
            stats: GlobalStats::default(),
            colors,
            hotkeys,
            connected: false,
            engine_detached: Arc::new(AtomicBool::new(false)),
            pending_engine_removal: None,
            layout: LayoutState::default(),
            popped_out: HashSet::new(),
            credential_session: None,
            pending_launch: None,
            pending_launch_confirmed: false,
            module_runs: Arc::new(Mutex::new(HashMap::new())),
            rebuild_tx: None,
            rebuild_attempts: Arc::new(Mutex::new(HashMap::new())),
            restart_tx: None,
            retry_tx: None,
            crash_state: Arc::new(Mutex::new(HashMap::new())),
            launch_tx: None,
            pending_prompt: VecDeque::new(),
            selected_prompt: 0,
            last_esc_press: None,
            module_logs: Arc::new(Mutex::new(VecDeque::new())),
            module_errors: Arc::new(Mutex::new(HashMap::new())),
            module_started_at: Arc::new(Mutex::new(HashMap::new())),
            module_last_error: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[allow(dead_code)]
    pub fn active_window_name(&self) -> &str {
        self.active_window.name()
    }

    /// True once the TUI has deliberately forgotten its engine.
    ///
    /// Every engine-sourced event is dropped from this point on, and the client
    /// task will not dial again: the removal is terminal for this session, and a
    /// client that reconnects to a removed engine is the orphan this crate has
    /// already been bitten by.
    pub fn engine_forgotten(&self) -> bool {
        self.engine_detached.load(Ordering::SeqCst)
    }

    /// Raise the stop switch, so the websocket client stops reconnecting.
    pub fn detach_engine(&mut self) {
        self.engine_detached.store(true, Ordering::SeqCst);
    }

    /// A cheap fingerprint of everything that changes the SHAPE of the screen
    /// rather than its contents: terminal size, the draggable layout
    /// percentages, which window set is mounted, which window is focused, and
    /// whether a config editor is taking over the modules pane.
    ///
    /// ratatui only repaints cells that differ from the previous frame, so a
    /// window that has just moved or grown can leave stale cells from wherever
    /// it used to be. Comparing this between frames lets the draw loop detect
    /// "the screen just changed shape" and force a full repaint instead of a
    /// diff — which is both the fix and a general safety net for any future
    /// layout glitch.
    pub fn screen_shape(&self, width: u16, height: u16) -> ScreenShape {
        ScreenShape {
            width,
            height,
            left_width_pct: self.layout.left_width_pct,
            top_height_pct: self.layout.top_height_pct,
            log_height_pct: self.layout.log_height_pct,
            prompts_width_pct: self.layout.prompts_width_pct,
            window_count: self.windows.len(),
            active: self.active_window,
            editing: self.windows.iter().any(|w| w.in_editor()),
            prompting: self.credential_session.is_some(),
            prompt_count: self.pending_prompt.len(),
        }
    }

    /// Whether the next frame must be a FULL repaint rather than a diff.
    ///
    /// True when the screen's shape changed since the last drawn frame, or when
    /// the Ctrl+L escape hatch was raised. This is the single decision point, so
    /// the draw loop and the tests cannot disagree about it.
    pub fn needs_full_repaint(&self, shape: ScreenShape, last: Option<ScreenShape>) -> bool {
        self.force_full_redraw || last != Some(shape)
    }

    /// Tick count per half-cycle of the PAUSED indicator's flash. The draw
    /// loop's `redraw` ticker fires every 100ms, so 6 ticks ≈ 0.6s per phase
    /// (a ~0.8Hz flash: slow enough to read as text, fast enough to catch out
    /// of the corner of your eye).
    pub const PAUSE_FLASH_TICKS: u64 = 6;

    /// Whether the PAUSED indicator is in its visible phase for redraw `tick`.
    ///
    /// Deliberately NOT part of [`ScreenShape`]. The shape fingerprint exists to
    /// force `terminal.clear()` when the screen's geometry moves, and a blink
    /// moves no geometry — it changes the glyphs of one span in an existing row.
    /// ratatui's diff already emits exactly those cells, and the `redraw` ticker
    /// already produces a frame every 100ms, so the flash rides that repaint
    /// tick for free. Folding the phase into the shape would instead clear and
    /// repaint the ENTIRE terminal twice a second to change a few characters.
    pub fn pause_flash_on(tick: u64) -> bool {
        (tick / Self::PAUSE_FLASH_TICKS).is_multiple_of(2)
    }

    pub fn handle_global_key(&mut self, key: KeyEvent) -> Option<Action> {
        if let Some(action) = self.hotkeys.global.get(&key) {
            match action {
                Action::FocusLeft => {
                    let idx = WindowId::all().iter().position(|w| *w == self.active_window).unwrap_or(0);
                    let prev = if idx == 0 { WindowId::all().len() - 1 } else { idx - 1 };
                    self.active_window = WindowId::all()[prev];
                    return Some(Action::Noop);
                }
                Action::FocusRight => {
                    let idx = WindowId::all().iter().position(|w| *w == self.active_window).unwrap_or(0);
                    let next = (idx + 1) % WindowId::all().len();
                    self.active_window = WindowId::all()[next];
                    return Some(Action::Noop);
                }
                Action::FocusDown => {
                    let idx = WindowId::all().iter().position(|w| *w == self.active_window).unwrap_or(0);
                    let next = (idx + 1) % WindowId::all().len();
                    self.active_window = WindowId::all()[next];
                    return Some(Action::Noop);
                }
                Action::FocusUp => {
                    let idx = WindowId::all().iter().position(|w| *w == self.active_window).unwrap_or(0);
                    let prev = if idx == 0 { WindowId::all().len() - 1 } else { idx - 1 };
                    self.active_window = WindowId::all()[prev];
                    return Some(Action::Noop);
                }
                Action::FocusNext => {
                    let idx = WindowId::all().iter().position(|w| *w == self.active_window).unwrap_or(0);
                    let next = (idx + 1) % WindowId::all().len();
                    self.active_window = WindowId::all()[next];
                    return Some(Action::Noop);
                }
                Action::FocusPrev => {
                    let idx = WindowId::all().iter().position(|w| *w == self.active_window).unwrap_or(0);
                    let prev = if idx == 0 { WindowId::all().len() - 1 } else { idx - 1 };
                    self.active_window = WindowId::all()[prev];
                    return Some(Action::Noop);
                }
                Action::Quit => return Some(Action::Quit),
                // A global action that the engine/supervisor performs (e.g. the
                // pipeline pause toggle). Handed back to the caller, which runs
                // it through the same dispatch path as a window action — a
                // global binding must work from ANY focused window.
                Action::TogglePipelinePause => return Some(Action::TogglePipelinePause),
                _ => {}
            }
        }
        None
    }

    pub fn get_window_mut(&mut self, id: WindowId) -> Option<&mut Box<dyn Window>> {
        self.windows.iter_mut().find(|w| w.id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare state: `handle_global_key` only reads the key map, so these tests
    /// need no mounted windows.
    fn state() -> AppState {
        AppState::new(
            crate::colors::load_colors(&std::path::PathBuf::from("")),
            crate::hotkeys::default_hotkeys(),
        )
    }

    #[test]
    fn crash_ladder_escalates_and_resets() {
        let t0 = 1_000_000_000i64;

        // First crash → prebuilt restart, consecutive = 1.
        let mut st = ModuleCrashState::default();
        let (rebuild, consecutive) = st.record_crash(t0);
        assert!(!rebuild);
        assert_eq!(consecutive, 1);

        // Second crash within the window → escalate to a rebuild.
        let (rebuild, consecutive) = st.record_crash(t0 + 60_000);
        assert!(rebuild);
        assert_eq!(consecutive, 2);

        // Third crash within the window → still rebuilding, prompt threshold met.
        let (rebuild, consecutive) = st.record_crash(t0 + 120_000);
        assert!(rebuild);
        assert_eq!(consecutive, 3);
        assert!(consecutive >= CONSECUTIVE_CRASH_PROMPT);

        // The module survived a full window → a fresh crash goes back to
        // prebuilt-first. (Last crash was t0+120s; need now ≥ last+30min.)
        let (rebuild, consecutive) = st.record_crash(t0 + 33 * 60 * 1000);
        assert!(!rebuild);
        assert_eq!(consecutive, 1);
    }

    #[test]
    fn crash_backoff_is_exponential_and_capped() {
        assert_eq!(crash_backoff(0), Duration::from_secs(1));
        assert_eq!(crash_backoff(1), Duration::from_secs(1));
        assert_eq!(crash_backoff(2), Duration::from_secs(2));
        assert_eq!(crash_backoff(3), Duration::from_secs(4));
        assert_eq!(crash_backoff(4), Duration::from_secs(8));
        assert_eq!(crash_backoff(10), Duration::from_secs(512));
        // Capped at 10 minutes, never grows further.
        assert_eq!(crash_backoff(11), Duration::from_secs(CRASH_BACKOFF_MAX_SECS));
        assert_eq!(crash_backoff(20), Duration::from_secs(CRASH_BACKOFF_MAX_SECS));
    }

    #[test]
    fn the_pause_flash_alternates_and_both_phases_are_reachable() {
        let t = AppState::PAUSE_FLASH_TICKS;
        assert!(t > 0, "a zero half-cycle would make the phase a division by zero");
        // On at the first tick of a cycle, off at the first tick of the next.
        assert!(AppState::pause_flash_on(0));
        assert!(AppState::pause_flash_on(t - 1));
        assert!(!AppState::pause_flash_on(t));
        assert!(!AppState::pause_flash_on(2 * t - 1));
        assert!(AppState::pause_flash_on(2 * t));

        // A phase must not flicker inside itself (a sub-tick change is a stutter,
        // not a flash), and over a long run both phases must actually occur.
        let mut seen = [false; 2];
        for tick in 0..60 * t {
            let phase = AppState::pause_flash_on(tick);
            seen[phase as usize] = true;
            // Everything within one half-cycle agrees.
            assert_eq!(phase, AppState::pause_flash_on(tick / t * t));
        }
        assert!(seen[0] && seen[1], "only one phase ever occurred: {:?}", seen);
    }

    #[test]
    fn a_global_toggle_binding_is_handed_back_to_the_caller() {
        let mut s = state();
        let key = KeyEvent::new(crossterm::event::KeyCode::Char('p'), crossterm::event::KeyModifiers::empty());
        assert_eq!(s.handle_global_key(key), Some(Action::TogglePipelinePause));
        // Focus moves still resolve to their own Noop, unchanged.
        let tab = KeyEvent::new(crossterm::event::KeyCode::Tab, crossterm::event::KeyModifiers::empty());
        assert_eq!(s.handle_global_key(tab), Some(Action::Noop));
        // An unbound key is still inert.
        let z = KeyEvent::new(crossterm::event::KeyCode::Char('Z'), crossterm::event::KeyModifiers::empty());
        assert_eq!(s.handle_global_key(z), None);
    }
}

#[cfg(test)]
mod screen_shape_tests {
    use super::*;
    use crate::app::{CredentialSession, ScreenShape};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    fn state_with_windows() -> AppState {
        let mut s = AppState::new(
            crate::colors::load_colors(&std::path::PathBuf::from("")),
            crate::hotkeys::default_hotkeys(),
        );
        s.windows = vec![
            Box::new(crate::windows::LogoWindow),
            Box::new(crate::windows::LogWindow::new()),
            Box::new(crate::windows::ModulesWindow::new()),
        ];
        s
    }

    fn shape(s: &AppState) -> ScreenShape {
        s.screen_shape(120, 40)
    }

    #[test]
    fn a_quiet_frame_keeps_the_same_shape() {
        // Content changes (logs arriving, a message moving) must NOT change the
        // shape, or every frame would trigger a needless full repaint.
        let mut s = state_with_windows();
        let before = shape(&s);
        for i in 0..50 {
            s.windows[1].push_log(crate::windows::log::LogEntry {
                timestamp: String::new(),
                source: format!("mod-{}", i),
                message: "something happened".to_string(),
                event_type: 1,
            });
        }
        s.stats.total_commands += 50;
        assert_eq!(shape(&s), before, "content churn must not count as a shape change");
    }

    #[test]
    fn every_shape_change_is_detected() {
        let base = {
            let s = state_with_windows();
            shape(&s)
        };

        // Terminal resize.
        let s = state_with_windows();
        assert_ne!(s.screen_shape(121, 40), base, "width change");
        assert_ne!(s.screen_shape(120, 41), base, "height change");

        // Each draggable layout percentage.
        let mut s = state_with_windows();
        s.layout.left_width_pct += 1;
        assert_ne!(shape(&s), base, "left_width_pct");
        let mut s = state_with_windows();
        s.layout.top_height_pct += 1;
        assert_ne!(shape(&s), base, "top_height_pct");
        let mut s = state_with_windows();
        s.layout.log_height_pct += 1;
        assert_ne!(shape(&s), base, "log_height_pct");
        // The new prompts drag.
        let mut s = state_with_windows();
        s.layout.prompts_width_pct += 1;
        assert_ne!(shape(&s), base, "prompts_width_pct");

        // A window being popped out changes the mounted set.
        let mut s = state_with_windows();
        s.windows.pop();
        assert_ne!(shape(&s), base, "window_count");

        // Focus change.
        let mut s = state_with_windows();
        s.active_window = WindowId::Log;
        assert_ne!(shape(&s), base, "active window");

        // A prompt arriving.
        let mut s = state_with_windows();
        s.pending_prompt.push_back(PendingPrompt {
            deadline: Instant::now() + Duration::from_secs(30),
            prompt: cockatiel_client::proto::Prompt::default(),
            text_input: String::new(),
        });
        assert_ne!(shape(&s), base, "prompt_count");

        // A credential session opening.
        let mut s = state_with_windows();
        s.credential_session = Some(CredentialSession {
            module_name: "m".to_string(),
            fields: Vec::new(),
            collected: HashMap::new(),
        });
        assert_ne!(shape(&s), base, "credential session");
    }

    #[test]
    fn the_manual_redraw_flag_is_a_latch_until_cleared() {
        let mut s = state_with_windows();
        assert!(!s.force_full_redraw);
        s.force_full_redraw = true;
        // The draw loop consumes it; screen_shape does not, so the flag stays a
        // pure "repaint on the next frame" latch.
        let _ = shape(&s);
        assert!(s.force_full_redraw, "reading the shape must not swallow the request");
    }
}

#[cfg(test)]
mod full_repaint_tests {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Widget};
    use ratatui::Terminal;

    fn paint(area: Rect, buf: &mut ratatui::buffer::Buffer, text: &str, color: Color) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        Paragraph::new(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(color),
        )))
        .render(area, buf);
    }

    /// ratatui's diff is already correct for cells that go blank, so the value
    /// of clearing on a shape change is INDEPENDENCE from the previous frame:
    /// whatever was on screen before cannot influence the new one. This is the
    /// property the draw loop relies on, and the safety net for any renderer
    /// edge case (a resize mid-frame, a window that stops being drawn).
    #[test]
    fn a_cleared_frame_does_not_depend_on_what_was_there_before() {
        let (w, h) = (40u16, 6u16);
        let new_frame = |f: &mut ratatui::Frame| {
            paint(Rect::new(0, 0, 10, h), f.buffer_mut(), "LOG", Color::Red);
        };

        // Frame 1 covers the whole width with modules text.
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            paint(Rect::new(0, 0, w, h), f.buffer_mut(), "MODULES", Color::Green);
        })
        .unwrap();
        assert_eq!(term.backend().buffer()[(0, 0)].symbol(), "M");

        // Same new frame, diffed (no clear): the backend still shows the old
        // frame's leftovers to the right of the new content.
        term.draw(new_frame).unwrap();
        let diffed = term.backend().buffer().clone();
        assert_eq!(
            diffed[(30, 0)].symbol(),
            " ",
            "a blank cell is correctly cleared by the diff"
        );

        // With the clear the loop performs, the result is byte-identical to a
        // first-ever render of the same frame.
        let mut cleared = Terminal::new(TestBackend::new(w, h)).unwrap();
        cleared.draw(|f| {
            paint(Rect::new(0, 0, w, h), f.buffer_mut(), "MODULES", Color::Green);
        })
        .unwrap();
        let _ = cleared.clear();
        cleared.draw(new_frame).unwrap();
        let cleared_buf = cleared.backend().buffer().clone();

        let mut fresh = Terminal::new(TestBackend::new(w, h)).unwrap();
        fresh.draw(new_frame).unwrap();
        assert_eq!(
            cleared_buf,
            fresh.backend().buffer().clone(),
            "a cleared frame must be identical to a first-ever render"
        );
    }

    #[test]
    fn the_draw_loop_clears_exactly_when_the_shape_changes_or_ctrl_l_asks() {
        use super::AppState;
        let mut s = AppState::new(
            crate::colors::load_colors(&std::path::PathBuf::from("")),
            crate::hotkeys::default_hotkeys(),
        );
        s.windows = vec![Box::new(crate::windows::LogoWindow)];

        // First frame ever: nothing drawn before, so repaint in full.
        let shape = s.screen_shape(100, 30);
        assert!(s.needs_full_repaint(shape, None), "the first frame must repaint");

        // Steady frames: no clear.
        for _ in 0..5 {
            assert!(!s.needs_full_repaint(shape, Some(shape)), "a quiet frame must not clear");
        }

        // Every shape change forces a repaint.
        s.layout.prompts_width_pct += 5;
        let moved = s.screen_shape(100, 30);
        assert!(s.needs_full_repaint(moved, Some(shape)), "a layout drag must repaint");
        s.active_window = crate::app::WindowId::Log;
        let focused = s.screen_shape(100, 30);
        assert!(s.needs_full_repaint(focused, Some(moved)), "a focus change must repaint");

        // Ctrl+L forces a repaint even with nothing changed.
        s.force_full_redraw = true;
        assert!(
            s.needs_full_repaint(focused, Some(focused)),
            "Ctrl+L must repaint on demand"
        );
    }
}
