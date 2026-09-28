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
    /// Pause / resume the engine's dispatch gate. Global (engine-wide, not
    /// module-scoped), so it is bound in the `nav` section and dispatched with
    /// the negation of the currently-known state.
    TogglePipelinePause,
    /// Forget the engine: ask it to shut down (it decides whether to obey), then
    /// drop the connection and everything the TUI learned from it.
    ///
    /// Engine-row only, and TUI-side only — "remove" means the TUI forgets the
    /// engine, NOT that the operator's `config.json` / `.env` are destroyed. The
    /// verb is deliberately not `Delete`, because that is what the neighbouring
    /// `DeleteModule` does.
    RemoveEngine,
    /// Kill the supervised engine child and launch it again — the action the
    /// config editor's "restart the engine to apply" warning points at. Refused
    /// when the TUI is not the engine's parent, and when the engine has been
    /// removed from the TUI.
    RestartEngine,
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
        "TogglePipelinePause" => Action::TogglePipelinePause,
        "RemoveEngine" => Action::RemoveEngine,
        "RestartEngine" => Action::RestartEngine,
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
        Action::TogglePipelinePause => "pause",
        // `detach`, not `remove`: the row it acts on is a connection, not a
        // directory, and a hint bar that says "remove" next to `del:[d]` reads
        // as "delete the engine's files". Nothing is deleted here.
        Action::RemoveEngine => "detach",
        Action::RestartEngine => "restart",
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
    // A capital letter IS the shift — `E` already reads as "shift+e", so naming
    // the modifier as well would print one key twice (`creds:[C|shift+C]` for
    // the single key `C`) and would make `edit:[shift+E]` out of a key the
    // operator pressed as `E`. Non-alphabetic shifted keys (`ctrl+shift+left`)
    // still need the prefix, because the glyph does not carry it.
    if k.modifiers.contains(KeyModifiers::SHIFT)
        && k.code != KeyCode::BackTab
        && !matches!(k.code, KeyCode::Char(c) if c.is_uppercase())
    {
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
    let mut labels: Vec<&'static str> = order.to_vec();
    for action in map.values() {
        let label = action_label(action);
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    render_labels(map, &labels)
}

/// As [`format_bindings`], but ONLY for the labels asked for. A window's bar
/// advertises one global action (`pause`) from a map that also holds `quit` and
/// the focus keys; appending the unlisted ones would print that window's bar as
/// a duplicate of the status bar.
fn format_selected_bindings(map: &HashMap<KeyEvent, Action>, order: &[&'static str]) -> String {
    render_labels(map, order)
}

/// `(label, key)` pairs whose label is printed as ONE key in the bar, taken from
/// the keys bound to it — `key` is the name [`key_to_str`] writes (`"E"`,
/// `"ctrl+shift+v"`, ...).
///
/// Only for actions that deliberately have MORE than one binding, and only for
/// the bar: both keys keep working. A bar has one line and this window's action
/// row is already crowded, so `edit:[E|e]` spends it twice on one action — and
/// the whole point of binding a second key is that the operator presses the new
/// one. If the named key is not bound to that label (a rebind, or a config file
/// that omits it) every key is printed instead, so the hint can never go blank
/// for an action that still works.
type PrimaryKeys<'a> = &'a [(&'static str, &'static str)];

/// Render `order` from `map`, narrowing the labels named in `primary` to a
/// single key each.
fn format_selected_bindings_primary(
    map: &HashMap<KeyEvent, Action>,
    order: &[&'static str],
    primary: PrimaryKeys<'_>,
) -> String {
    render_labels_primary(map, order, primary)
}

fn render_labels(map: &HashMap<KeyEvent, Action>, labels: &[&'static str]) -> String {
    render_labels_primary(map, labels, &[])
}

fn render_labels_primary(
    map: &HashMap<KeyEvent, Action>,
    labels: &[&'static str],
    primary: PrimaryKeys<'_>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for label in labels {
        let mut keys: Vec<String> = map
            .iter()
            .filter(|(_, a)| action_label(a) == *label)
            .map(|(k, _)| key_to_str(k))
            .collect();
        if keys.is_empty() {
            continue;
        }
        keys.sort();
        keys.dedup();
        if let Some((_, want)) = primary.iter().find(|(l, _)| l == label) {
            if keys.iter().any(|k| k == want) {
                keys = vec![want.to_string()];
            }
        }
        parts.push(format!("{}:[{}]", label, keys.join("|")));
    }
    parts.join(", ")
}

impl HotkeyConfig {
    /// Global (nav) bindings as `<command>:[<keys>], ...`.
    pub fn format_global(&self) -> String {
        format_bindings(&self.global, &["quit", "window-next", "window-prev"])
    }

    /// A subset of the global (nav) bindings, by label, as `<command>:[<keys>]`.
    /// A window's own bar uses this to advertise a GLOBAL action it cannot
    /// bind for itself (`format_window` only ever sees `window_actions`), so the
    /// hint goes through the same wrapped bar as every other one instead of
    /// being hardcoded into the window. Only the labels asked for are printed.
    pub fn format_global_actions(&self, order: &[&'static str]) -> String {
        format_selected_bindings(&self.global, order)
    }

    /// A window's bindings as `<command>:[<keys>], ...`.
    pub fn format_window(&self, name: &str, order: &[&'static str]) -> String {
        let map = self.window_actions.get(name).cloned().unwrap_or_default();
        format_bindings(&map, order)
    }

    /// A window's bindings, ONLY for the labels asked for, with the labels
    /// named in `primary` narrowed to the one key the operator is most likely
    /// to press (see [`PrimaryKeys`]).
    ///
    /// [`HotkeyConfig::format_window`] appends every binding in the window's
    /// map that the order list did not name, which is right for a static bar
    /// but wrong for one that narrows to the selected row: the "hidden" actions
    /// would print anyway, and the bar would advertise keys the app refuses to
    /// dispatch on that row.
    pub fn format_window_selected(
        &self,
        name: &str,
        order: &[&'static str],
        primary: PrimaryKeys<'_>,
    ) -> String {
        let map = self.window_actions.get(name).cloned().unwrap_or_default();
        format_selected_bindings_primary(&map, order, primary)
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
    // `p` → pause/resume the engine's dispatch gate. Global, because the gate
    // is engine-wide: an operator must be able to resume from any window, not
    // just the modules one. `p` was chosen for the mnemonic and because it is
    // bound nowhere else — not in this map, and not handled by any window's
    // internal key handling (which uses hjkl, arrows, w and 1-5) — so it
    // collides with no existing binding.
    global.insert(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::empty()), Action::TogglePipelinePause);

    let mut module_actions = HashMap::new();
    module_actions.insert(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::empty()), Action::StartModule(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()), Action::StopModule(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()), Action::DeleteModule(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()), Action::ToggleAutostart(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty()), Action::DuplicateModule(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::empty()), Action::ClearModuleConfig(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT), Action::EditCredentials(String::new()));
    module_actions.insert(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::empty()), Action::EditConfig(String::new()));
    // `E` opens the SAME editor as `e`. The operator asked for it: a capital
    // key is one row up from the `c`/`b`/`a`/`x` cluster it sits beside, so it
    // needs no reaching over, and it reads as "engine" — which is exactly the
    // row it acts on when the engine row is selected. Both keys stay bound, so
    // nothing that worked before changes. The hint bar prints only `E` (see
    // `format_window_selected_primary`): one action, one advertised key.
    module_actions.insert(KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT), Action::EditConfig(String::new()));
    // `R` restarts the engine, `X` removes it from the TUI. Both are capitals
    // because both are engine-only and the window's whole module-action row is
    // lowercase — capital = a different KIND of action, the convention `C`
    // (credentials) already set. `X` sits directly above `x` (stop): a mis-hit
    // lands on `x`, which is refused on the engine row with a log line, so the
    // worst case of reaching for the wrong one is a message, not a kill.
    module_actions.insert(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT), Action::RestartEngine);
    module_actions.insert(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT), Action::RemoveEngine);
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

    #[test]
    fn p_binds_the_pipeline_pause_toggle_globally() {
        let cfg = load_hotkeys(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hotkey_config.json"),
        );
        let p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::empty());
        assert_eq!(cfg.global.get(&p), Some(&Action::TogglePipelinePause));
        // The default alone is enough — the keymap file merely restates it.
        assert_eq!(
            default_hotkeys().global.get(&p),
            Some(&Action::TogglePipelinePause)
        );
        // The name round-trips, so the file entry is a real binding and not a
        // silent Noop.
        assert_eq!(parse_action("TogglePipelinePause"), Action::TogglePipelinePause);
        assert_eq!(action_label(&Action::TogglePipelinePause), "pause");
        // Global actions advertise themselves in the status bar and can be
        // pulled into a window's own wrapped bar.
        assert!(cfg.format_global().contains("pause:[p]"), "status bar: {}", cfg.format_global());
        assert_eq!(cfg.format_global_actions(&["pause"]), "pause:[p]");
        // A label with no such global binding yields nothing (no dangling
        // "pause:[]" in a bar).
        assert_eq!(cfg.format_global_actions(&["nope"]), "");
    }

    #[test]
    fn the_pause_toggle_key_collides_with_nothing() {
        let cfg = load_hotkeys(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hotkey_config.json"),
        );
        let p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::empty());
        // No window section may claim `p`, or the global binding would shadow it
        // (global keys are matched before the focused window's own handling).
        for (window, map) in &cfg.window_actions {
            assert!(
                !map.contains_key(&p),
                "'p' is also bound in the {} section: {:?}",
                window,
                map.get(&p)
            );
        }
        assert!(!cfg.editor_actions.contains_key(&p));
    }

    // ── the capital keys: E (edit), R (restart engine), X (remove engine) ──

    fn keymap() -> HotkeyConfig {
        load_hotkeys(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hotkey_config.json"))
    }

    /// The operator asked for `E`, and it is an ADDITIONAL binding for the same
    /// action as `e` — not a move. Both have to work, or a muscle-memory `e`
    /// would stop opening the config.
    ///
    /// Both modifier forms are checked because the terminal is what decides
    /// which one arrives: crossterm reports Shift-E with the SHIFT modifier, and
    /// the keymap file's `"E"` entry (no modifier, as every file entry is
    /// written) covers a terminal that reports the capital without it. The same
    /// reason `C` is present twice.
    #[test]
    fn capital_e_opens_the_editor_and_lowercase_e_still_does() {
        let cfg = keymap();
        let modules = cfg.window_actions.get("modules").expect("modules map");
        let is_edit = |mods| {
            matches!(
                modules.get(&KeyEvent::new(KeyCode::Char('E'), mods)),
                Some(Action::EditConfig(_))
            )
        };
        assert!(is_edit(KeyModifiers::SHIFT), "shift+e must open the config editor");
        assert!(is_edit(KeyModifiers::empty()), "a bare E must open it too");
        // `e` is untouched.
        assert!(matches!(
            modules.get(&KeyEvent::new(KeyCode::Char('e'), KeyModifiers::empty())),
            Some(Action::EditConfig(_))
        ));
        // The DEFAULTS alone are enough, so a deleted or replaced keymap file
        // cannot lose the binding the operator asked for.
        let defaults = default_hotkeys();
        let d = defaults.window_actions.get("modules").expect("default modules map");
        assert!(matches!(
            d.get(&KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT)),
            Some(Action::EditConfig(_))
        ));
        assert!(matches!(
            d.get(&KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT)),
            Some(&Action::RestartEngine)
        ));
        assert!(matches!(
            d.get(&KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT)),
            Some(&Action::RemoveEngine)
        ));
        // The names round-trip, so the file entries are real bindings and not
        // silent `Noop`s.
        assert_eq!(parse_action("EditConfig"), Action::EditConfig(String::new()));
        assert_eq!(parse_action("RestartEngine"), Action::RestartEngine);
        assert_eq!(parse_action("RemoveEngine"), Action::RemoveEngine);
        // The labels the hint bar prints.
        assert_eq!(action_label(&Action::RestartEngine), "restart");
        assert_eq!(action_label(&Action::RemoveEngine), "detach");
    }

    /// All three capitals are new, so each has to be shown to land on nothing
    /// else. The global map is the dangerous one: global bindings are matched
    /// BEFORE the focused window's own handling, so a global `E` would shadow
    /// the editor key in every window. The editor map matters too — a key
    /// bound there is consumed by the config editor and never reaches the
    /// window.
    #[test]
    fn the_capital_keys_collide_with_nothing() {
        let cfg = keymap();
        for ch in ['E', 'R', 'X'] {
            for mods in [KeyModifiers::SHIFT, KeyModifiers::empty()] {
                let k = KeyEvent::new(KeyCode::Char(ch), mods);
                assert_eq!(cfg.global.get(&k), None, "'{}' is bound globally", ch);
                assert_eq!(cfg.editor_actions.get(&k), None, "'{}' is bound in the editor", ch);
                for (window, map) in &cfg.window_actions {
                    if window != "modules" {
                        assert!(
                            !map.contains_key(&k),
                            "'{}' is also bound in the {} section: {:?}",
                            ch,
                            window,
                            map.get(&k)
                        );
                    }
                }
            }
        }
        // And the keys that WERE taken are still taken by what they were bound
        // to — a new capital must not have displaced a lowercase action.
        let modules = cfg.window_actions.get("modules").expect("modules map");
        assert!(matches!(modules.get(&KeyEvent::new(KeyCode::Char('e'), KeyModifiers::empty())), Some(Action::EditConfig(_))), "e");
        assert!(matches!(modules.get(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty())), Some(Action::StopModule(_))), "x");
        assert!(matches!(modules.get(&KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty())), Some(Action::DeleteModule(_))), "d");
        assert!(matches!(modules.get(&KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT)), Some(Action::EditCredentials(_))), "C");
        // `p` stays global and `q` still quits — the two bindings the brief of
        // record protects.
        assert_eq!(cfg.global.get(&KeyEvent::new(KeyCode::Char('p'), KeyModifiers::empty())), Some(&Action::TogglePipelinePause));
        assert_eq!(cfg.global.get(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty())), Some(&Action::Quit));
    }

    /// `edit` has two bindings, so the bar has to choose. It prints `E` — the
    /// key the operator asked for and the one they will reach for — and the
    /// choice is only a display decision: both keys keep working.
    #[test]
    fn the_hint_bar_prints_one_key_for_an_action_with_two_bindings() {
        let cfg = keymap();
        let labels = ["start", "stop", "edit"];
        // Without a primary, every binding is listed (the old behaviour).
        let all = cfg.format_window_selected("modules", &labels, &[]);
        assert!(all.contains("edit:[E|e]"), "bar: {}", all);
        // With one, exactly that key — and NOT a half-narrowed `edit:[E|`.
        let one = cfg.format_window_selected("modules", &labels, &[("edit", "E")]);
        assert!(one.contains("edit:[E]"), "bar: {}", one);
        assert!(!one.contains("edit:[E|"), "the alias must not also print: {}", one);
        assert!(one.contains("start:[s]"), "the other labels are unaffected: {}", one);
        // A primary that is NOT bound to the label falls back to every key
        // rather than printing nothing for an action that still works.
        let stale = cfg.format_window_selected("modules", &labels, &[("edit", "ctrl+alt+q")]);
        assert!(stale.contains("edit:[E|e]"), "bar: {}", stale);
    }
}
