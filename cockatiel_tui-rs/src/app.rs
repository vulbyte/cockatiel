use std::collections::{HashMap, HashSet, VecDeque};
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
use crate::db::GlobalStats;
use crate::layout::LayoutState;
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

pub trait Window {
    fn id(&self) -> WindowId;
    fn render(&mut self, area: Rect, buf: &mut Buffer, is_active: bool, stats: &GlobalStats, colors: &ColorConfig, hotkeys: &HotkeyConfig, prompts: &[PendingPrompt]);
    fn handle_key(&mut self, _key: KeyEvent, _stats: &mut GlobalStats) -> Option<Action> { None }
    fn handle_mouse(&mut self, _mouse: MouseEvent, _area: Rect) -> Option<Action> { None }
    /// The module currently selected in this window (used to fill in the name
    /// for module actions resolved from the hotkey config).
    fn selected_module_name(&self, _stats: &GlobalStats) -> Option<String> { None }
    /// A clickable link rendered by this window (e.g. a prompt's link), if any.
    fn pending_link(&self) -> Option<(Rect, String)> { None }
    /// Append a log entry to this window (the log window displays them).
    fn push_log(&mut self, _entry: LogEntry) {}
    /// Which prompt in the queue this window should highlight (only the
    /// prompts window uses this).
    fn set_prompt_selected(&mut self, _idx: usize) {}
    /// Enter the window's inline config editor for a module (loads `.env` +
    /// `config.json` into editable rows).
    fn start_config_editor(&mut self, _module_name: &str, _dir: std::path::PathBuf) {}
    /// True when the window's config editor is active (it consumes all keys).
    fn in_editor(&self) -> bool { false }
    /// Handle a key while the config editor is active. Returns true when the
    /// key was consumed by the editor.
    fn editor_key(&mut self, _key: KeyEvent, _hotkeys: &HotkeyConfig) -> bool { false }
    /// Paste text into the active config editor (at the cursor). Consumed?
    fn editor_paste(&mut self, _text: &str) -> bool { false }
}

pub struct AppState {
    pub active_window: WindowId,
    pub windows: Vec<Box<dyn Window>>,
    pub stats: GlobalStats,
    pub colors: ColorConfig,
    pub hotkeys: HotkeyConfig,
    pub connected: bool,
    pub layout: LayoutState,
    pub popped_out: HashSet<String>,
    /// Active credential-entry session (driven through the prompt subwindow
    /// instead of a pop-up modal). None when no credential entry is in flight.
    pub credential_session: Option<CredentialSession>,
    /// Active config-editing session (driven through the prompt subwindow).
    /// Module to launch once its credential entry completes (the engine saves
    /// the config, then the TUI supervisor starts the process).
    pub pending_launch: Option<String>,
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
/// Delay before relaunching a crashed module (avoids a tight crash loop).
pub const RESTART_BACKOFF: Duration = Duration::from_secs(2);

impl AppState {
    pub fn new(colors: ColorConfig, hotkeys: HotkeyConfig) -> Self {
        Self {
            active_window: WindowId::Logo,
            windows: Vec::new(),
            stats: GlobalStats::default(),
            colors,
            hotkeys,
            connected: false,
            layout: LayoutState::default(),
            popped_out: HashSet::new(),
            credential_session: None,
            pending_launch: None,
            module_runs: Arc::new(Mutex::new(HashMap::new())),
            rebuild_tx: None,
            rebuild_attempts: Arc::new(Mutex::new(HashMap::new())),
            restart_tx: None,
            crash_state: Arc::new(Mutex::new(HashMap::new())),
            launch_tx: None,
            pending_prompt: VecDeque::new(),
            selected_prompt: 0,
            last_esc_press: None,
            module_logs: Arc::new(Mutex::new(VecDeque::new())),
            module_errors: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[allow(dead_code)]
    pub fn active_window_name(&self) -> &str {
        self.active_window.name()
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
}
