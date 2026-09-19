use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            WindowId::Logo => "cockatiel",
            WindowId::Log => "log",
            WindowId::Modules => "modules",
            WindowId::Chart => "message chart",
            WindowId::Prompts => "prompts",
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
    /// Module to launch once its credential entry completes (the engine saves
    /// the config, then the TUI supervisor starts the process).
    pub pending_launch: Option<String>,
    /// Supervisor-side module lifecycle status: "starting" / "connected" /
    /// "crashed" / "stopped". Merged over the engine-reported status at render.
    pub module_runs: Arc<Mutex<HashMap<String, String>>>,
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
