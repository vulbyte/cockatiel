use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    FocusNext,
    FocusPrev,
    Quit,
    ToggleModule,
    DisconnectModule,
    StartModule(String),
    StopModule(String),
    DeleteModule(String),
    ToggleAutostart(String),
    /// Duplicate the selected module under a NEW module name (the engine gives
    /// it a fresh instance UUID) and launch it as its own process — a second,
    /// independent instance sharing the original's binary + config.
    DuplicateModule(String),
    AddNote,
    ShowInfo,
    SelectModule,
    TimeWindow5m,
    TimeWindow1h,
    TimeWindow6h,
    TimeWindow24h,
    ZoomIn,
    ZoomOut,
    TogglePlatform,
    PopOut(String),
    EditCredentials(String),
    EditConfig(String),
    /// Confirm then empty a module's `.env` + `config.json` values (keys kept).
    ClearModuleConfig(String),
    RunTests,
    /// One-shot user-database query from the detached users window
    /// (`query_id`, JSON payload in `sql`), sent via `WsCommand::SendQuery`.
    UserQuery(String, String),
    Noop,
}

#[derive(Debug, Deserialize)]
struct HotkeyFile {
    nav: HashMap<String, String>,
    #[serde(default)]
    modules: HashMap<String, String>,
    #[serde(default)]
    chart: HashMap<String, String>,
    #[serde(default)]
    editor: HashMap<String, String>,
}

/// Config-editor actions (bound via the `editor` section of the key map).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditorAction {
    MoveUp,
    MoveDown,
    CursorLeft,
    CursorRight,
    Commit,
    SaveExit,
}

#[derive(Debug, Clone)]
pub struct HotkeyConfig {
    pub global: HashMap<KeyEvent, Action>,
    #[allow(dead_code)]
    pub window_actions: HashMap<String, HashMap<KeyEvent, Action>>,
    /// Config-editor bindings (j/k/arrows/Enter/Esc by default).
    pub editor_actions: HashMap<KeyEvent, EditorAction>,
}

impl HotkeyConfig {
    /// The config-editor action bound to `key`, if any.
    pub fn editor_action(&self, key: &KeyEvent) -> Option<EditorAction> {
        self.editor_actions.get(key).copied()
    }
}

fn parse_key(s: &str) -> Option<KeyEvent> {
    let parts: Vec<&str> = s.split('+').collect();
    let mut modifiers = KeyModifiers::empty();
    let mut code = None;

    for part in &parts {
        match *part {
            "Ctrl" | "ctrl" => modifiers |= KeyModifiers::CONTROL,
            "Alt" | "alt" => modifiers |= KeyModifiers::ALT,
            "Shift" | "shift" => modifiers |= KeyModifiers::SHIFT,
            "Left" => code = Some(KeyCode::Left),
            "Right" => code = Some(KeyCode::Right),
            "Up" => code = Some(KeyCode::Up),
            "Down" => code = Some(KeyCode::Down),
            "Tab" => code = Some(KeyCode::Tab),
            "BackTab" => {
                // crossterm reports Shift+Tab as BackTab with SHIFT modifier
                modifiers |= KeyModifiers::SHIFT;
                code = Some(KeyCode::BackTab);
            }
            "Enter" => code = Some(KeyCode::Enter),
            "Esc" => code = Some(KeyCode::Esc),
            "Space" => code = Some(KeyCode::Char(' ')),
            "Backspace" => code = Some(KeyCode::Backspace),
            "Delete" => code = Some(KeyCode::Delete),
            "Home" => code = Some(KeyCode::Home),
            "End" => code = Some(KeyCode::End),
            "PageUp" => code = Some(KeyCode::PageUp),
            "PageDown" => code = Some(KeyCode::PageDown),
            c if c.len() == 1 => {
                let ch = c.chars().next()?;
                code = Some(KeyCode::Char(ch));
            }
            _ => return None,
        }
    }

    Some(KeyEvent::new(code?, modifiers))
}

fn parse_action(s: &str) -> Action {
    match s {
        "FocusLeft" => Action::FocusLeft,
        "FocusDown" => Action::FocusDown,
        "FocusUp" => Action::FocusUp,
        "FocusRight" => Action::FocusRight,
        "FocusNext" => Action::FocusNext,
        "FocusPrev" => Action::FocusPrev,
        "Quit" => Action::Quit,
        "ToggleModule" => Action::ToggleModule,
        "DisconnectModule" => Action::DisconnectModule,
        "StartModule" => Action::StartModule(String::new()),
        "StopModule" => Action::StopModule(String::new()),
        "DeleteModule" => Action::DeleteModule(String::new()),
        "ToggleAutostart" => Action::ToggleAutostart(String::new()),
        "DuplicateModule" => Action::DuplicateModule(String::new()),
        "AddNote" => Action::AddNote,
        "ShowInfo" => Action::ShowInfo,
        "SelectModule" => Action::SelectModule,
        "TimeWindow5m" => Action::TimeWindow5m,
        "TimeWindow1h" => Action::TimeWindow1h,
        "TimeWindow6h" => Action::TimeWindow6h,
        "TimeWindow24h" => Action::TimeWindow24h,
        "ZoomIn" => Action::ZoomIn,
        "ZoomOut" => Action::ZoomOut,
        "TogglePlatform" => Action::TogglePlatform,
        "EditCredentials" => Action::EditCredentials(String::new()),
        "EditConfig" => Action::EditConfig(String::new()),
        "ClearModuleConfig" => Action::ClearModuleConfig(String::new()),
        "RunTests" => Action::RunTests,
        _ => Action::Noop,
    }
}

pub fn load_hotkeys(path: &PathBuf) -> HotkeyConfig {
    // Start from the defaults and overlay the file on top, so default
    // bindings (e.g. `e` → EditConfig) always apply unless the file rebinds
    // that key.
    let mut cfg = default_hotkeys();

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return cfg,
    };

    let file: HotkeyFile = match serde_json::from_str(&content) {
        Ok(f) => f,
        Err(_) => return cfg,
    };

    for (key, action) in &file.nav {
        if let Some(k) = parse_key(key) {
            cfg.global.insert(k, parse_action(action));
        }
    }

    for (key, action) in &file.modules {
        if let Some(k) = parse_key(key) {
            cfg.window_actions
                .entry("modules".to_string())
                .or_default()
                .insert(k, parse_action(action));
        }
    }

    for (key, action) in &file.chart {
        if let Some(k) = parse_key(key) {
            cfg.window_actions
                .entry("chart".to_string())
                .or_default()
                .insert(k, parse_action(action));
        }
    }

    for (key, action) in &file.editor {
        if let Some(k) = parse_key(key) {
            cfg.editor_actions.insert(k, parse_editor_action(action));
        }
    }

    cfg
}

/// Human-readable label for an action, used in the `<command>:[<keys>]` bars.
pub fn action_label(action: &Action) -> &'static str {
    match action {
        Action::StartModule(_) => "start",
        Action::StopModule(_) => "stop",
        Action::DeleteModule(_) => "del",
        Action::ToggleAutostart(_) => "auto",
        Action::DuplicateModule(_) => "copy",
        Action::EditCredentials(_) => "creds",
        Action::EditConfig(_) => "edit",
        Action::ClearModuleConfig(_) => "clear",
        Action::RunTests => "test",
        Action::SelectModule => "select",
        // The users pop-out gets its own label (`users:[u]`); the per-window
        // `w` pop-out stays `popout`.
        Action::PopOut(name) if name == "users" => "users",
        Action::PopOut(_) => "popout",
        Action::UserQuery(_, _) => "userdb",
        Action::Quit => "quit",
        Action::FocusNext => "window-next",
        Action::FocusPrev => "window-prev",
        Action::FocusLeft => "left",
        Action::FocusRight => "right",
        Action::FocusUp => "up",
        Action::FocusDown => "down",
        Action::TimeWindow5m => "5m",
        Action::TimeWindow1h => "1h",
        Action::TimeWindow6h => "6h",
        Action::TimeWindow24h => "24h",
        Action::ZoomIn => "zoom-in",
        Action::ZoomOut => "zoom-out",
        Action::TogglePlatform => "toggle",
        Action::AddNote => "note",
        Action::ShowInfo => "info",
        Action::ToggleModule => "toggle-module",
        Action::DisconnectModule => "disconnect",
        Action::Noop => "noop",
    }
}

fn key_to_str(k: &KeyEvent) -> String {
    let mut prefix = String::new();
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        prefix.push_str("ctrl+");
    }
    if k.modifiers.contains(KeyModifiers::ALT) {
        prefix.push_str("alt+");
    }
    if k.modifiers.contains(KeyModifiers::SHIFT) && k.code != KeyCode::BackTab {
        prefix.push_str("shift+");
    }
    let code = match k.code {
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "shift+tab".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pgup".to_string(),
        KeyCode::PageDown => "pgdn".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        other => format!("{:?}", other),
    };
    format!("{}{}", prefix, code)
}

/// Format a binding map as `<command>:[<key1|key2>], ...` in the given order
/// (any bindings not listed are appended at the end).
fn format_bindings(map: &HashMap<KeyEvent, Action>, order: &[&'static str]) -> String {
    let mut labels: Vec<&'static str> = order.iter().copied().collect();
    for action in map.values() {
        let label = action_label(action);
        if !labels.contains(&label) {
            labels.push(label);
        }
    }

    let mut parts: Vec<String> = Vec::new();
    for label in labels {
        let mut keys: Vec<String> = map
            .iter()
            .filter(|(_, a)| action_label(a) == label)
            .map(|(k, _)| key_to_str(k))
            .collect();
        if keys.is_empty() {
            continue;
        }
        keys.sort();
        keys.dedup();
        parts.push(format!("{}:[{}]", label, keys.join("|")));
    }
    parts.join(", ")
}

impl HotkeyConfig {
    /// Global (nav) bindings as `<command>:[<keys>], ...`.
    pub fn format_global(&self) -> String {
        format_bindings(&self.global, &["quit", "window-next", "window-prev"])
    }

    /// A window's bindings as `<command>:[<keys>], ...`.
    pub fn format_window(&self, name: &str, order: &[&'static str]) -> String {
        let map = self.window_actions.get(name).cloned().unwrap_or_default();
        format_bindings(&map, order)
    }
}

fn parse_editor_action(s: &str) -> EditorAction {
    match s {
        "MoveUp" => EditorAction::MoveUp,
        "MoveDown" => EditorAction::MoveDown,
        "CursorLeft" => EditorAction::CursorLeft,
        "CursorRight" => EditorAction::CursorRight,
        "Commit" => EditorAction::Commit,
        "SaveExit" => EditorAction::SaveExit,
        _ => EditorAction::SaveExit,
    }
}

/// Default config-editor bindings: j/k + arrows navigate, ←/→ move the cursor,
/// Enter commits, Esc saves + exits.
fn default_editor_actions() -> HashMap<KeyEvent, EditorAction> {
    let mut m = HashMap::new();
    m.insert(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()), EditorAction::MoveDown);
    m.insert(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()), EditorAction::MoveDown);
    m.insert(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()), EditorAction::MoveUp);
    m.insert(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()), EditorAction::MoveUp);
    m.insert(KeyEvent::new(KeyCode::Left, KeyModifiers::empty()), EditorAction::CursorLeft);
    m.insert(KeyEvent::new(KeyCode::Right, KeyModifiers::empty()), EditorAction::CursorRight);
    m.insert(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), EditorAction::Commit);
    m.insert(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), EditorAction::SaveExit);
    m
}

pub fn default_hotkeys() -> HotkeyConfig {
    let mut global = HashMap::new();
    // Window focus is Tab / Shift+Tab only.
    // hjkl + arrows are window-internal navigation handled by each window.
    // NOTE: Ctrl+C is deliberately NOT bound to quit — in a terminal it is the
    // copy shortcut, so quitting on it breaks copy/paste. Exit with double-Esc
    // (handled by the app) or `q`.
    global.insert(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()), Action::FocusNext);
    global.insert(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), Action::FocusPrev);
    global.insert(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()), Action::Quit);

    let mut module_actions = HashMap::new();
    module_actions.insert(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::empty()), Action::StartModule(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()), Action::StopModule(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()), Action::DeleteModule(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()), Action::ToggleAutostart(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty()), Action::DuplicateModule(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::empty()), Action::ClearModuleConfig(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT), Action::EditCredentials(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::empty()), Action::EditConfig(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::empty()), Action::RunTests);
    module_actions.insert(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::empty()), Action::PopOut("users".to_string()));

    let mut chart_actions = HashMap::new();
    chart_actions.insert(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::empty()), Action::TimeWindow5m);
    chart_actions.insert(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::empty()), Action::TimeWindow1h);
    chart_actions.insert(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::empty()), Action::TimeWindow6h);
    chart_actions.insert(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::empty()), Action::TimeWindow24h);
    chart_actions.insert(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::empty()), Action::ZoomIn);
    chart_actions.insert(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::empty()), Action::ZoomOut);
    chart_actions.insert(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), Action::TogglePlatform);
    chart_actions.insert(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()), Action::TogglePlatform);

    let mut window_actions = HashMap::new();
    window_actions.insert("modules".to_string(), module_actions);
    window_actions.insert("chart".to_string(), chart_actions);

    HotkeyConfig {
        global,
        window_actions,
        editor_actions: default_editor_actions(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e_binds_editconfig_and_defaults_survive_the_file() {
        let cfg = load_hotkeys(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hotkey_config.json"),
        );
        let modules = cfg.window_actions.get("modules").expect("modules map");
        // `e` → EditConfig (present in the file and/or defaults).
        assert!(modules.contains_key(&KeyEvent::new(KeyCode::Char('e'), KeyModifiers::empty())));
        assert!(modules
            .values()
            .any(|a| matches!(a, Action::EditConfig(_))));
        // A default binding NOT in the file still survives the merge:
        // `u` → PopOut("users") is a default that the file omits.
        assert!(modules
            .values()
            .any(|a| matches!(a, Action::PopOut(ref w) if w == "users")));
        // `c` duplicates the selected module; `b` clears its config.
        assert!(modules
            .values()
            .any(|a| matches!(a, Action::DuplicateModule(_))));
        assert!(modules
            .values()
            .any(|a| matches!(a, Action::ClearModuleConfig(_))));
    }

    #[test]
    fn editor_bindings_load_from_the_key_map() {
        let cfg = load_hotkeys(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hotkey_config.json"),
        );
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(cfg.editor_action(&j), Some(EditorAction::MoveDown));
        assert_eq!(cfg.editor_action(&esc), Some(EditorAction::SaveExit));
    }

    #[test]
    fn u_binds_users_popout_in_modules_window() {
        let cfg = load_hotkeys(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hotkey_config.json"),
        );
        let modules = cfg.window_actions.get("modules").expect("modules map");
        let u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::empty());
        // The `u` default survives the config-file merge.
        assert_eq!(modules.get(&u), Some(&Action::PopOut("users".to_string())));
        // And it renders the dedicated `users` label in the hotkey bar.
        assert_eq!(action_label(&Action::PopOut("users".to_string())), "users");
        assert_eq!(action_label(&Action::PopOut("log".to_string())), "popout");
        let bar = cfg.format_window("modules", &["start", "stop", "del", "auto", "creds", "edit", "test", "select"]);
        assert!(bar.contains("users:[u]"), "bar: {}", bar);
    }
}
