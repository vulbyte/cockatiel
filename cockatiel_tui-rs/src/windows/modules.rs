use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::app::{PendingPrompt, Window, WindowId};

use crate::colors::ColorConfig;
use crate::db::GlobalStats;
use crate::hotkeys::{Action, HotkeyConfig};

/// Whether the flashing PAUSED indicator belongs on screen this frame.
///
/// `connected` is the engine link, not "has a pipeline": a disconnected TUI
/// has no gate to report, and a connected-but-running one has nothing to warn
/// about. `flash_on` is the blink phase — folding it in here keeps the rule
/// testable as one truth instead of an `if` buried in the renderer.
pub fn paused_indicator_visible(connected: bool, paused: bool, flash_on: bool) -> bool {
    connected && paused && flash_on
}

/// The label the engine row shows in the config editor's title bar. Not a
/// module name — the engine is not in the plugin list, and a row titled after
/// some module would be a lie.
const ENGINE_ROW_LABEL: &str = "engine";

/// What the engine row says once the operator has removed the engine from this
/// TUI. Not "disconnected": a disconnected engine is coming back on the client's
/// reconnect backoff, and a removed one is not. The row stays (it is where the
/// operator comes back to, via `E`) and says the one true thing about it.
const ENGINE_REMOVED_STATUS: &str = "no engine";

/// The key the hint bar prints for the `edit` label, which has two bindings.
///
/// `E` and not `e`, because `E` is the key the operator asked for and the one
/// they will reach for; `e` stays bound so nothing that used to work stops
/// working. Printing both would spend two of the bar's one line on one action,
/// on the row whose bar is the most crowded in the app. See
/// [`crate::hotkeys::PrimaryKeys`].
const EDIT_PRIMARY_KEY: (&str, &str) = ("edit", "E");

/// Row 0 of the modules window's list. The engine is a real, selectable row
/// ahead of the modules, not a status line above them.
pub const ENGINE_ROW: usize = 0;

/// One selectable row in the modules window's unified list.
///
/// The list is the ENGINE followed by every entry of `GlobalStats::module_entries`,
/// so `self.selected` indexes THIS space — one past where it used to point.
/// Modelling the rows explicitly is what keeps that honest: every site that
/// used to index `module_entries` directly now goes through [`EntryRow`], and
/// `Module(_)` is the only variant that can yield a module name, so a
/// module-scoped action cannot silently fire on a neighbouring row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryRow {
    Engine,
    /// Index into `GlobalStats::module_entries`.
    Module(usize),
}

impl EntryRow {
    /// How many selectable rows a list of `module_count` modules has. Never
    /// zero: the engine row exists even with no modules registered.
    pub fn total(module_count: usize) -> usize {
        module_count + 1
    }

    /// The row at `index`, or `None` when `index` is past the end of the list.
    pub fn at(index: usize, module_count: usize) -> Option<Self> {
        if index == ENGINE_ROW {
            Some(EntryRow::Engine)
        } else {
            index
                .checked_sub(1)
                .filter(|i| *i < module_count)
                .map(EntryRow::Module)
        }
    }

    /// The index into `module_entries`, or `None` for the engine row.
    pub fn module_index(self) -> Option<usize> {
        match self {
            EntryRow::Engine => None,
            EntryRow::Module(i) => Some(i),
        }
    }

    /// The module name this row names, or `None` for the engine row.
    ///
    /// `None` is the point: the engine is not a module, so a caller that needs
    /// a module has nothing to do here. Handing back a neighbouring row's name
    /// instead is how "stop" ends up killing a module nobody selected.
    pub fn module_name(self, stats: &GlobalStats) -> Option<String> {
        self.module_index()
            .and_then(|i| stats.module_entries.get(i))
            .map(|m| m.name.clone())
    }
}

/// Clamp a stored selection into a list of `total` rows.
pub fn clamp_selected(selected: usize, total: usize) -> usize {
    if total == 0 {
        0
    } else {
        selected.min(total - 1)
    }
}

/// The scroll offset that keeps row `selected` inside a `visible`-row viewport
/// over a list of `total` rows, given the current `scroll`.
///
/// A pure function on purpose. This arithmetic used to be inline in the
/// renderer with `self.selected` indexing `module_entries` directly; adding the
/// engine row shifted the index space underneath it, and the failure mode is
/// silent — a list that scrolls wrong, or a selection that drifts off the
/// viewport. One tested truth beats three inline `min()`s.
pub fn scroll_for(selected: usize, scroll: usize, visible: usize, total: usize) -> usize {
    if total == 0 || visible == 0 {
        return 0;
    }
    // Never past the end: the last row must land on the last line rather than
    // the view overshooting into blank rows below it.
    let max_scroll = total.saturating_sub(visible);
    let scroll = scroll.min(max_scroll);
    if selected < scroll {
        // Above the viewport: bring it to the top line.
        selected.min(max_scroll)
    } else if selected >= scroll + visible {
        // Below the viewport: scroll exactly far enough to land it on the
        // last line, so stepping down one row moves the view one row.
        (selected + 1).saturating_sub(visible).min(max_scroll)
    } else {
        scroll
    }
}

/// The window hint bar's action labels for one selected row.
///
/// `HotkeyConfig::format_window` APPENDS every binding in the window's map that
/// the order list did not name, so a bar that narrows to the selected row has
/// to go through `format_window_selected` — otherwise the "hidden" actions
/// print anyway and the narrowing is a lie.
///
/// `engine_removed` narrows the engine row a second time, for the same reason:
/// after the operator removes the engine there is no connection to restart and
/// no engine to detach, and offering keys whose press can only be refused is a
/// bar that lies about the row it is describing.
fn hint_labels(row: EntryRow, engine_removed: bool) -> &'static [&'static str] {
    match row {
        // The engine: open its own config, start/stop it, and — while there IS
        // one — restart or remove it. `select`/`popout`/`users` act on the
        // WINDOW, so they mean the same thing on every row. The pause toggle is
        // global and is appended by the caller. `start`/`stop` are the SAME
        // keys the modules use (s/x): on the engine row the dispatcher turns
        // them into launch/kill of the engine process rather than of a module.
        // The rest — del/auto/copy/creds/clear/test — is module-only and is
        // refused on the engine row (see `is_module_scoped`), so advertising it
        // would promise an action the key press refuses.
        EntryRow::Engine if engine_removed => &["edit", "select", "popout", "users"],
        EntryRow::Engine => &[
            "edit", "start", "stop", "restart", "detach", "select", "popout", "users",
        ],
        EntryRow::Module(_) => &[
            "start", "stop", "del", "auto", "copy", "creds", "edit", "clear", "test", "select",
            "popout", "users",
        ],
    }
}

/// Whether a saved engine config key takes effect on its own, or only after the
/// engine restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reload {
    /// The running engine re-reads it without being restarted.
    HotReload,
    /// The running engine keeps the old value until it restarts.
    NeedsRestart,
}

/// One engine config key and the reason for its classification.
///
/// Data, not a comment beside a log line: the reason is what gets printed in
/// the post-save warning, so the operator is told WHY a change is not live yet
/// instead of only being told that it is not.
///
/// Grounded in the engine rather than guessed. In
/// `cockatiel_engine-rs/src/config.rs`, `get_config` re-reads `config.json`
/// only when the file's SIZE changed, and the 3s config-poll task in
/// `main.rs` pushes only the three pre/in/post ordering lists into the
/// orchestrator. Everything else is consumed once, on the startup path.
struct EngineConfigKey {
    /// A `config.json` TOP-LEVEL key, or a `.env` variable name.
    key: &'static str,
    reload: Reload,
    /// Printed verbatim in the save warning.
    why: &'static str,
}

const ENGINE_CONFIG_KEYS: &[EngineConfigKey] = &[
    EngineConfigKey {
        key: "inputs",
        reload: Reload::HotReload,
        why: "re-read for every input broadcast",
    },
    EngineConfigKey {
        key: "preprocessModules",
        reload: Reload::HotReload,
        why: "the config poll task pushes the pre-process chain every 3s",
    },
    EngineConfigKey {
        key: "inprocessModules",
        reload: Reload::HotReload,
        why: "the config poll task pushes the in-process chain every 3s",
    },
    EngineConfigKey {
        key: "postprocessModules",
        reload: Reload::HotReload,
        why: "the config poll task pushes the post-process chain every 3s",
    },
    EngineConfigKey {
        // Read live, per request, by the `engine_shutdown` branch via
        // `get_config(config_state)`. This one matters more than its size
        // suggests: the flag exists so the operator can allow the engine to be
        // stopped over the wire, and telling them to restart the engine to
        // apply it would be actively counterproductive — the restart is the
        // very thing they are being told they need in order to avoid being
        // asked to restart.
        key: "shutdown_on_request",
        reload: Reload::HotReload,
        why: "read live on every shutdown request, no restart needed",
    },
    EngineConfigKey {
        // `TcpListener::bind(format!("{}:{}", bind_ip, config.port))` runs once
        // on the startup path; nothing rebinds the socket afterwards.
        key: "port",
        reload: Reload::NeedsRestart,
        why: "the listening socket is bound once at boot",
    },
    EngineConfigKey {
        // Read exactly once, at boot, by `config::start_paused` — and the
        // engine's own comment says the config poll task deliberately does not
        // touch it, because pausing is an operator action rather than a
        // setting. So an edit here changes the NEXT boot only.
        key: "start_paused",
        reload: Reload::NeedsRestart,
        why: "the boot gate is read once at startup, never by the config poll",
    },
    EngineConfigKey {
        // `ensure_secrets` resolves the PIN into `ConfigState.pin` at boot and
        // `verify_pin` compares every pairing request against that in-memory
        // value. Editing `.env` therefore does not change what the RUNNING
        // engine accepts, and a client that paired with the old PIN is what
        // has to reconnect anyway.
        key: "COCKATIEL_PIN",
        reload: Reload::NeedsRestart,
        why: "the PIN is resolved into memory at boot, and bound clients paired with the old one",
    },
    EngineConfigKey {
        // The JWT secret is handed to `AuthStore::new` once and used to verify
        // every token for the life of the process. Tokens already signed with
        // the old secret would stop verifying the moment it changed in place,
        // so this is a restart or a mass logout, not a live edit.
        key: "COCKATIEL_JWT_SECRET",
        reload: Reload::NeedsRestart,
        why: "the signing secret is loaded at boot, so tokens already issued with it stay valid only until restart",
    },
];

/// The verdict for a key that is not in the table: restart.
///
/// Deliberate, and the safe direction to be wrong in. "Not in the table" means
/// nobody has read the engine's code to prove the key is live, not that it is
/// live — and the engine's remaining settings really are boot-bound
/// (`timeline_database_*` builds the `DatabaseManager` once,
/// `max_connections` / `handshake_timeout_secs` size the listener once,
/// `module_probe_*` and `module_approval_policy` are read once by the module
/// manager, `recovery_grace_secs` is read once before its task). Same fail-safe
/// direction as `GlobalStats::pipeline_paused` defaulting to paused: being
/// wrong costs one restart, being wrong the other way costs an operator
/// believing a change is live when the running engine never looked at it.
const UNKNOWN_KEY_RELOAD: Reload = Reload::NeedsRestart;

/// The reason reported for a key that is not in the table.
const UNKNOWN_KEY_WHY: &str = "not a key the running engine is known to re-read";

/// How the engine will treat `key` once it has been saved: `key` is a
/// top-level `config.json` key or a `.env` variable name.
pub fn engine_key_reload(key: &str) -> Reload {
    ENGINE_CONFIG_KEYS
        .iter()
        .find(|k| k.key == key)
        .map(|k| k.reload)
        .unwrap_or(UNKNOWN_KEY_RELOAD)
}

/// The operator-facing reason for `key`'s classification, printed after a save.
pub fn engine_key_why(key: &str) -> &'static str {
    ENGINE_CONFIG_KEYS
        .iter()
        .find(|k| k.key == key)
        .map(|k| k.why)
        .unwrap_or(UNKNOWN_KEY_WHY)
}

/// An edited engine setting that will not take effect until the engine is
/// restarted, with the reason to show the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineRestartNote {
    pub key: String,
    pub why: &'static str,
}

/// The modules window: engine row + module list, databases, platforms.
#[derive(Debug, Clone)]
pub struct ModulesWindow {

    /// First VISIBLE row of the unified list, in the same index space as
    /// `selected` (see [`EntryRow`]).
    pub scroll: usize,

    /// The selected row of the unified list: 0 is the engine, 1..=n are the
    /// modules. Always an index into THAT list, never into `module_entries`.
    pub selected: usize,

    /// Clickable region of the pending prompt's link, set during render.

    pub link_rect: Option<Rect>,

    pub link_url: Option<String>,

    /// Active config editor (takes over the window until Esc).

    editing: Option<ConfigEditor>,

    /// Module whose config was most recently saved by the editor (cleared on
    /// read) — lets the app warn that the module must be restarted.

    last_saved_module: Option<String>,

    /// Engine settings the editor most recently saved that will NOT take
    /// effect until the engine restarts (cleared on read). The engine mirror
    /// of `last_saved_module`: most of what the editor can change is re-read
    /// by the running engine, so this is a per-key list rather than a flag.

    last_saved_engine: Option<Vec<EngineRestartNote>>,

}

/// One step in a config path (a map key or an array index).
#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key(String),
    Idx(usize),
}

/// What a row is: a leaf value, a "+" row that appends to a map/list, or a
/// non-editable group label (an object/array's key).
#[derive(Debug, Clone, PartialEq)]
enum RowKind {
    Scalar,
    AddMap,
    AddList,
    Group,
}

/// One editable/structural row in the config editor.
#[derive(Debug, Clone)]
struct EditorRow {
    /// "env" or "json".
    source: String,
    /// Location in the tree (map keys + array indices).
    path: Vec<Seg>,
    /// Scalar value, or the pending input for a "+" row.
    value: String,
    cursor: usize,
    /// `.env` rows are secrets — censored on screen.
    is_secret: bool,
    kind: RowKind,
    /// Parent display label for "+" rows (used to name new children).
    add_base: String,
    /// The value this row was LOADED with, so a save can tell an actual edit
    /// from a re-write of what was already on disk. Empty for a row a "+"
    /// commit just created (it never existed on disk), which is what makes a
    /// newly added setting count as a change.
    original: String,
}

/// Which files the open config editor is pointed at. The engine owns a
/// `.env` + `config.json` like any module but is not a plugin, so the post-save
/// restart warning is decided by the TARGET rather than by looking the name up
/// in the module list (where it would never be found).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorTarget {
    Engine,
    Module,
}

/// The config editor: a tree of the target's `.env` + `config.json` flattened
/// into editable rows, with "+" rows to add keys (maps) / items (arrays).
#[derive(Debug, Clone)]
struct ConfigEditor {
    target: EditorTarget,
    /// Shown in the editor's title bar; a module name, or `ENGINE_ROW_LABEL`.
    label: String,
    dir: PathBuf,
    rows: Vec<EditorRow>,
    selected: usize,
    /// Scroll offset in DISPLAY LINES (see [`EditorLine`]), not row indices.
    scroll: usize,
    /// Set after Esc is pressed: await y/n before saving (or discarding).
    confirm_save: bool,
}

/// How many lines of context the editor keeps between the cursor and the top or
/// bottom edge before it scrolls.
const EDITOR_SCROLL_MARGIN: usize = 3;

/// One rendered line of the config editor.
///
/// Rows and the chrome around them are tracked explicitly because they are not
/// 1:1: a section header consumes a line without being an editable row, and
/// every row is followed by a blank continuation line. Scrolling used to treat
/// one row as one line, so the viewport was ~2x too small and the selected row
/// fell off the bottom of the window without the view ever following it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorLine {
    /// The `.env` / `config.json` section banner.
    Header { source: String },
    /// An editable row.
    Row { index: usize, prefix: String },
    /// The blank continuation line drawn under a row.
    Blank { prefix: String },
}

/// The full ordered list of display lines for `rows`.
///
/// Pure and independent of the window size so the scroll arithmetic can be
/// tested without rendering anything.
fn editor_lines(rows: &[EditorRow]) -> Vec<EditorLine> {
    let mut out: Vec<EditorLine> = Vec::new();
    let mut prev: Option<String> = None;
    for (i, row) in rows.iter().enumerate() {
        if prev.as_deref() != Some(row.source.as_str()) {
            out.push(EditorLine::Header {
                source: row.source.clone(),
            });
            prev = Some(row.source.clone());
        }
        // "+" rows hang one level under their parent; groups/scalars sit at
        // their own depth.
        let depth = if matches!(row.kind, RowKind::AddMap | RowKind::AddList) {
            row.path.len() + 1
        } else {
            row.path.len()
        };
        let prefix = ModulesWindow::tree_prefix(rows, i, depth);
        out.push(EditorLine::Row {
            index: i,
            prefix: prefix.clone(),
        });
        if i + 1 < rows.len() {
            out.push(EditorLine::Blank { prefix });
        }
    }
    out
}

/// The display-line index of the selected row.
fn editor_selected_line(lines: &[EditorLine], selected: usize) -> usize {
    lines
        .iter()
        .position(|l| matches!(l, EditorLine::Row { index, .. } if *index == selected))
        .unwrap_or(0)
}

/// The scroll offset that keeps `line` visible inside a `visible`-line viewport
/// with at least `margin` lines of context above and below it.
///
/// Scrolling only kicks in once the cursor comes within `margin` of an edge, so
/// moving one row at a time does not shuffle the view; by the time it does, the
/// cursor has been pushed to the 3rd line from the relevant edge. The margin is
/// halved on short viewports so the two constraints can never cross and push the
/// cursor off screen, and the result is pulled back so the view never scrolls
/// past the end of the content.
fn editor_scroll_for(
    line: usize,
    scroll: usize,
    visible: usize,
    margin: usize,
    total: usize,
) -> usize {
    if visible == 0 {
        return 0;
    }
    let margin = margin.min(visible.saturating_sub(1) / 2);
    // Cursor must sit >= margin from the top:  scroll <= line - margin
    let upper = line.saturating_sub(margin);
    // ...and >= margin from the bottom:            scroll >= line + margin + 1 - visible
    let lower = (line + margin + 1).saturating_sub(visible);
    let mut s = if scroll > upper {
        upper
    } else if scroll < lower {
        lower
    } else {
        scroll
    };
    // Don't scroll past the end. The bottom margin is only a preference — near
    // the end of the content it is unsatisfiable, and overshooting instead
    // would leave blank rows under the last one, so settle for the cursor
    // sitting lower in the viewport as long as it is still visible.
    let max_scroll = total.saturating_sub(visible);
    if s > max_scroll && line >= max_scroll {
        s = max_scroll;
    }
    s
}

impl ModulesWindow {
    pub fn new() -> Self {
        Self {
            scroll: 0,
            selected: 0,
            link_rect: None,
            link_url: None,
            editing: None,
            last_saved_module: None,
            last_saved_engine: None,
        }
    }

    /// Re-encode an edited value back to a `config.json` value, preserving the
    /// type: numbers/bools → native, everything else → string.
    fn json_value(text: &str, is_list: bool) -> serde_json::Value {
        if is_list {
            let items: Vec<String> = text
                .split('\n')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            return serde_json::Value::Array(items.into_iter().map(serde_json::Value::String).collect());
        }
        let t = text.trim();
        if let Ok(n) = t.parse::<i64>() {
            serde_json::Value::Number(n.into())
        } else if let Ok(f) = t.parse::<f64>() {
            serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| serde_json::Value::String(t.to_string()))
        } else if t == "true" {
            serde_json::Value::Bool(true)
        } else if t == "false" {
            serde_json::Value::Bool(false)
        } else {
            serde_json::Value::String(t.to_string())
        }
    }

    /// A scalar value as its JSON string form.
    fn scalar_text(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => String::new(),
        }
    }

    /// Flatten a JSON value into rows (recursively), appending "+" rows after
    /// every map and list.
    fn flatten_value(out: &mut Vec<EditorRow>, source: &str, path: Vec<Seg>, display: String, v: &serde_json::Value) {
        match v {
            serde_json::Value::Object(map) => {
                if !path.is_empty() {
                    out.push(EditorRow {
                        source: source.to_string(),
                        path: path.clone(),
                        value: String::new(),
                        cursor: 0,
                        is_secret: false,
                        kind: RowKind::Group,
                        add_base: String::new(),
                        original: String::new(),
                    });
                }
                for (k, val) in map {
                    let mut p = path.clone();
                    p.push(Seg::Key(k.clone()));
                    let d = if display.is_empty() { k.clone() } else { format!("{}.{}", display, k) };
                    Self::flatten_value(out, source, p, d, val);
                }
                out.push(EditorRow {
                    source: source.to_string(),
                    path,
                    value: String::new(),
                    cursor: 0,
                    is_secret: false,
                    kind: RowKind::AddMap,
                    add_base: display.clone(),
                    original: String::new(),
                });
            }
            serde_json::Value::Array(arr) => {
                if !path.is_empty() {
                    out.push(EditorRow {
                        source: source.to_string(),
                        path: path.clone(),
                        value: String::new(),
                        cursor: 0,
                        is_secret: false,
                        kind: RowKind::Group,
                        add_base: String::new(),
                        original: String::new(),
                    });
                }
                for (i, val) in arr.iter().enumerate() {
                    let mut p = path.clone();
                    p.push(Seg::Idx(i));
                    let d = format!("{}[{}]", display, i);
                    Self::flatten_value(out, source, p, d, val);
                }
                out.push(EditorRow {
                    source: source.to_string(),
                    path,
                    value: String::new(),
                    cursor: 0,
                    is_secret: false,
                    kind: RowKind::AddList,
                    add_base: display.clone(),
                    original: String::new(),
                });
            }
            other => {
                let value = Self::scalar_text(other);
                out.push(EditorRow {
                    source: source.to_string(),
                    path,
                    value: value.clone(),
                    cursor: value.chars().count(),
                    is_secret: false,
                    kind: RowKind::Scalar,
                    add_base: String::new(),
                    original: value.clone(),
                });
            }
        }
    }

    /// Load a module's `.env` + `config.json` into a flat, editable row list.
    fn load_rows(module_name: &str, dir: &std::path::Path) -> Vec<EditorRow> {
        let mut rows: Vec<EditorRow> = Vec::new();

        if let Ok(content) = std::fs::read_to_string(dir.join(".env")) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    let value = v.trim().to_string();
                    rows.push(EditorRow {
                        source: "env".into(),
                        path: vec![Seg::Key(k.trim().to_string())],
                        value: value.clone(),
                        cursor: value.chars().count(),
                        is_secret: true,
                        kind: RowKind::Scalar,
                        add_base: String::new(),
                        original: value.clone(),
                    });
                }
            }
        }

        rows.push(EditorRow {
            source: "env".into(),
            path: vec![],
            value: String::new(),
            cursor: 0,
            is_secret: false,
            kind: RowKind::AddMap,
            add_base: String::new(),
            original: String::new(),
        });

        if let Ok(content) = std::fs::read_to_string(dir.join("config.json")) {
            if let Ok(root) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(obj) = root.as_object() {
                    for (k, val) in obj {
                        if k == "module_specific" {
                            if let Some(ms) = val.as_object() {
                                rows.push(EditorRow {
                                    source: "json".into(),
                                    path: vec![Seg::Key("module_specific".into())],
                                    value: String::new(),
                                    cursor: 0,
                                    is_secret: false,
                                    kind: RowKind::Group,
                                    add_base: String::new(),
                                    original: String::new(),
                                });
                                for (mk, mv) in ms {
                                    Self::flatten_value(
                                        &mut rows, "json",
                                        vec![Seg::Key("module_specific".into()), Seg::Key(mk.clone())],
                                        format!("{}.{}", k, mk),
                                        mv,
                                    );
                                }
                                rows.push(EditorRow {
                                    source: "json".into(),
                                    path: vec![Seg::Key("module_specific".into())],
                                    value: String::new(),
                                    cursor: 0,
                                    is_secret: false,
                                    kind: RowKind::AddMap,
                                    add_base: String::new(),
                                    original: String::new(),
                                });
                            }
                        } else {
                            Self::flatten_value(&mut rows, "json", vec![Seg::Key(k.clone())], k.clone(), val);
                        }
                    }
                }
            }
        }
        rows.push(EditorRow {
            source: "json".into(),
            path: vec![],
            value: String::new(),
            cursor: 0,
            is_secret: false,
            kind: RowKind::AddMap,
            add_base: String::new(),
            original: String::new(),
        });
        let _ = module_name;
        rows
    }

    /// Insert `value` into a JSON tree at `path` (creating maps/arrays as needed).
    fn insert_path(root: &mut serde_json::Value, path: &[Seg], value: serde_json::Value) {
        if path.is_empty() {
            *root = value;
            return;
        }
        match &path[0] {
            Seg::Key(k) => {
                let obj = root.as_object_mut().expect("key path needs an object");
                let next_is_idx = matches!(path.get(1), Some(Seg::Idx(_)));
                if path.len() == 1 {
                    obj.insert(k.clone(), value);
                    return;
                }
                let entry = obj.entry(k.clone()).or_insert_with(|| {
                    if next_is_idx {
                        serde_json::Value::Array(vec![])
                    } else {
                        serde_json::Value::Object(Default::default())
                    }
                });
                if next_is_idx && !entry.is_array() {
                    *entry = serde_json::Value::Array(vec![]);
                }
                if !next_is_idx && !entry.is_object() {
                    *entry = serde_json::Value::Object(Default::default());
                }
                Self::insert_path(entry, &path[1..], value);
            }
            Seg::Idx(i) => {
                let arr = root.as_array_mut().expect("index path needs an array");
                while arr.len() <= *i {
                    arr.push(serde_json::Value::Null);
                }
                if path.len() == 1 {
                    arr[*i] = value;
                    return;
                }
                let next_is_key = matches!(path.get(1), Some(Seg::Key(_)));
                let entry = &mut arr[*i];
                if next_is_key && (entry.is_null() || !entry.is_object()) {
                    *entry = serde_json::Value::Object(Default::default());
                }
                Self::insert_path(entry, &path[1..], value);
            }
        }
    }

    /// Remove an edited scalar row: a map key / env var disappears from the
    /// saved file, and removing an array element re-indexes its siblings so the
    /// array stays contiguous.
    fn remove_scalar_row(rows: &mut Vec<EditorRow>, idx: usize) {
        let is_array_el = matches!(rows[idx].path.last(), Some(Seg::Idx(_)));
        if is_array_el {
            let removed_index = match rows[idx].path.last() {
                Some(Seg::Idx(i)) => *i,
                _ => 0,
            };
            let parent: Vec<Seg> = rows[idx].path[..rows[idx].path.len() - 1].to_vec();
            rows.remove(idx);
            for r in rows.iter_mut() {
                if r.path.len() > parent.len() && r.path[..parent.len()] == parent {
                    if let Some(Seg::Idx(i)) = r.path.last_mut() {
                        if *i > removed_index {
                            *i -= 1;
                        }
                    }
                }
            }
        } else {
            rows.remove(idx);
        }
    }

    /// Write the edited rows back to `.env` + `config.json`.
    fn save_editor(&mut self) {
        let Some(ed) = self.editing.take() else { return };
        let label = ed.label.clone();

        let mut env_lines: Vec<String> = Vec::new();
        let mut json_root: serde_json::Value = serde_json::json!({});
        for row in &ed.rows {
            if row.kind != RowKind::Scalar {
                continue;
            }
            if row.source == "env" {
                let key = match row.path.first() {
                    Some(Seg::Key(k)) => k.clone(),
                    _ => String::new(),
                };
                env_lines.push(format!("{}={}", key, row.value));
            } else {
                let mut root = json_root.clone();
                Self::insert_path(&mut root, &row.path, Self::json_value(&row.value, false));
                json_root = root;
            }
        }

        let env_path = ed.dir.join(".env");
        let mut env_content = env_lines.join("\n");
        if !env_content.is_empty() {
            env_content.push('\n');
        }
        let _ = crate::supervisor::write_atomic_0600(&env_path, &env_content);

        let json_path = ed.dir.join("config.json");
        if let Ok(pretty) = serde_json::to_string_pretty(&json_root) {
            let _ = crate::supervisor::write_atomic_0600(&json_path, &pretty);
        }
        crate::app::supervisor_log_global(format!(
            "[supervisor] saved config for {} (.env + config.json)",
            label
        ));
        // Which restart unit the save invalidated, and what it has to say about
        // it. The engine is not a module and has no relaunch key, so its
        // warning is per changed KEY rather than a blanket "restart the module".
        match ed.target {
            EditorTarget::Module => self.last_saved_module = Some(label),
            EditorTarget::Engine => self.last_saved_engine = Some(engine_restart_notes(&ed.rows)),
        }
    }

/// Vertical-line prefix for a row in the tree: each ancestor level shows
/// `│ ` while it still has later siblings, else `  `.
fn tree_prefix(rows: &[EditorRow], i: usize, depth: usize) -> String {
    let mut s = String::new();
    for level in 0..depth {
        let continues = rows[i + 1..].iter().any(|r| {
            r.path.len() > level && r.path[..level] == rows[i].path[..level]
        });
        s.push_str(if continues { "\u{2502} " } else { "  " });
    }
    s
}

/// The row's own label: the last map key, or empty for array elements / "+" rows.
fn row_label(row: &EditorRow) -> String {
    match row.path.last() {
        Some(Seg::Key(k)) => k.clone(),
        _ => String::new(),
    }
}

/// Render the config editor as a tree: `.env` (censored) and `config.json`
/// (visible) sections, `key : value` rows, `+` rows for maps/arrays/files.
fn render_editor(&mut self, area: Rect, buf: &mut Buffer, is_active: bool, colors: &ColorConfig, prompts: &[PendingPrompt]) {
        let Some(ed) = &mut self.editing else { return };
        if ed.rows.is_empty() {
            ed.selected = 0;
        } else {
            ed.selected = ed.selected.min(ed.rows.len() - 1);
        }

        let border_color = if is_active {
            colors.active_border_color("modules")
        } else {
            colors.border_color("inactive")
        };
        let block = Block::default()
            .title(format!(" config editor · {} ", ed.label))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let inner = area.inner(ratatui::layout::Margin { horizontal: 1, vertical: 1 });
        let mut y = inner.y;

        if !prompts.is_empty() {
            let expiring = prompts
                .iter()
                .filter(|p| p.deadline.saturating_duration_since(Instant::now()).as_secs() <= 10)
                .count();
            let line = Line::from(Span::styled(
                format!(" {} prompts waiting ({} expiring)", prompts.len(), expiring),
                Style::default().fg(Color::Yellow),
            ));
            line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            y += 1;
        }

        if y < inner.y + inner.height {
            let help = if ed.confirm_save {
                " Save changes?  y = save & exit   n = discard & exit   Esc = keep editing "
            } else {
                " \u{2191}/\u{2193} or j/k move \u{00b7} type to edit \u{00b7} Enter commit/add \u{00b7} Esc save & exit "
            };
            let style = if ed.confirm_save {
                Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let line = Line::from(Span::styled(help, style));
            line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            y += 1;
        }

        let visible = (inner.y + inner.height).saturating_sub(y) as usize;
        let lines = editor_lines(&ed.rows);
        let total = lines.len();
        let selected_line = editor_selected_line(&lines, ed.selected);
        ed.scroll = editor_scroll_for(
            selected_line,
            ed.scroll,
            visible,
            EDITOR_SCROLL_MARGIN,
            total,
        );

        for line in &lines[ed.scroll..] {
            if y >= inner.y + inner.height {
                break;
            }
            match line {
                EditorLine::Header { source } => {
                    let header = if source == "env" {
                        ".env (secrets — always censored)"
                    } else {
                        "config.json (settings — visible)"
                    };
                    let hline = Line::from(Span::styled(
                        format!(" {}", header),
                        Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                    ));
                    hline.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
                    y += 1;
                }
                // Blank continuation line after each row (matches the tree look).
                EditorLine::Blank { prefix } => {
                    let sep = Line::from(Span::styled(
                        prefix.clone(),
                        Style::default().fg(Color::DarkGray),
                    ));
                    sep.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
                    y += 1;
                }
                EditorLine::Row { index, prefix } => {
                    let i = *index;
                    let row = &ed.rows[i];
                    let is_selected = i == ed.selected && is_active;
                    let is_add = matches!(row.kind, RowKind::AddMap | RowKind::AddList);
                    let is_group = row.kind == RowKind::Group;
                    let label = Self::row_label(row);
                    let masked = row.is_secret;
                    let row_style = if is_selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else if is_group {
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                    } else if is_add {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::White)
                    };

                    let mut spans = vec![Span::styled(
                        prefix.clone(),
                        Style::default().fg(Color::DarkGray),
                    )];

                    if is_group {
                        // Non-editable object/array key.
                        spans.push(Span::styled(label, row_style));
                    } else if is_add {
                        // "+" row: show "+" plus any pending input. Highlight the "+"
                        // itself when the row is selected so the cursor is obvious.
                        let plus_style = if is_selected {
                            row_style.add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                        };
                        let chars: Vec<char> = row.value.chars().collect();
                        let pos = row.cursor.min(chars.len());
                        let b: String = chars[..pos].iter().collect();
                        let a: String = chars[pos..].iter().collect();
                        spans.push(Span::styled("+", plus_style));
                        if !row.value.is_empty() {
                            if is_selected {
                                spans.push(Span::styled(b, row_style));
                                spans.push(Span::styled(
                                    "\u{2588}",
                                    Style::default().fg(Color::White).bg(Color::Red),
                                ));
                                spans.push(Span::styled(a, row_style));
                            } else {
                                spans.push(Span::styled(
                                    format!("{}{}", b, a),
                                    Style::default().fg(Color::Green),
                                ));
                            }
                        }
                    } else if masked {
                        // `.env` secrets: always censored.
                        spans.push(if !label.is_empty() {
                            Span::styled(format!("{} : ", label), row_style)
                        } else {
                            Span::raw("")
                        });
                        spans.push(Span::styled("*****", row_style));
                    } else {
                        // `config.json`: visible value with a cursor block on the
                        // selected row.
                        if !label.is_empty() {
                            spans.push(Span::styled(
                                format!("{} : ", label),
                                Style::default().fg(Color::White),
                            ));
                        }
                        let chars: Vec<char> = row.value.chars().collect();
                        let pos = row.cursor.min(chars.len());
                        let b: String = chars[..pos].iter().collect();
                        let a: String = chars[pos..].iter().collect();
                        if is_selected {
                            spans.push(Span::styled(b, row_style));
                            spans.push(Span::styled(
                                "\u{2588}",
                                Style::default().fg(Color::White).bg(Color::Red),
                            ));
                            spans.push(Span::styled(a, row_style));
                        } else {
                            spans.push(Span::styled(format!("{}{}", b, a), row_style));
                        }
                    }

                    let line = Line::from(spans);
                    line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
                    y += 1;
                }
            }
        }

        block.render(area, buf);
    }    fn commit_add_map(rows: &mut Vec<EditorRow>, idx: usize) {
        let input = rows[idx].value.clone();
        let (key, rhs) = match input.split_once('=') {
            Some((k, r)) => (k.trim().to_string(), r.trim().to_string()),
            None => return, // need `key=value`
        };
        if key.is_empty() {
            return;
        }
        let value: serde_json::Value = if rhs.starts_with('[') && rhs.ends_with(']') {
            let inner = &rhs[1..rhs.len().saturating_sub(1)];
            let items: Vec<String> = inner
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            serde_json::Value::Array(items.into_iter().map(serde_json::Value::String).collect())
        } else {
            Self::json_value(&rhs, false)
        };

        let base = rows[idx].add_base.clone();
        let mut parent = rows[idx].path.clone();
        parent.push(Seg::Key(key.clone()));
        let new_display = if base.is_empty() { key.clone() } else { format!("{}.{}", base, key) };

        let mut new_rows = Vec::new();
        let src = rows[idx].source.clone();
        Self::flatten_value(&mut new_rows, &src, parent, new_display, &value);

        rows[idx].value.clear();
        rows[idx].cursor = 0;
        for (offset, row) in new_rows.into_iter().enumerate() {
            // Everything a "+" row commits is brand new: it has no value on
            // disk to compare against. Clearing `original` is what makes a
            // newly added setting count as a CHANGE in the restart warning
            // rather than looking like a no-op re-save.
            let mut row = row;
            row.original = String::new();
            rows.insert(idx + offset, row);
        }
    }

    /// Commit a "+ add item" row: append the typed text as a new list element.
    fn commit_add_list(rows: &mut Vec<EditorRow>, idx: usize) {
        let input = rows[idx].value.clone();
        let parent = rows[idx].path.clone();
        // Count existing elements under this list (path == parent + one Idx).
        let count = rows
            .iter()
            .filter(|r| {
                r.path.len() == parent.len() + 1
                    && matches!(r.path.last(), Some(Seg::Idx(_)))
                    && r.path[..parent.len()] == parent[..]
            })
            .count();

        let mut elem_path = parent.clone();
        elem_path.push(Seg::Idx(count));
        let base = rows[idx].add_base.clone();
        let new_display = format!("{}[{}]", base, count);

        let mut new_rows = Vec::new();
        let val: serde_json::Value = Self::json_value(&input, false);
        let src = rows[idx].source.clone();
        Self::flatten_value(&mut new_rows, &src, elem_path, new_display, &val);

        rows[idx].value.clear();
        rows[idx].cursor = 0;
        for (offset, row) in new_rows.into_iter().enumerate() {
            // Everything a "+" row commits is brand new: it has no value on
            // disk to compare against. Clearing `original` is what makes a
            // newly added setting count as a CHANGE in the restart warning
            // rather than looking like a no-op re-save.
            let mut row = row;
            row.original = String::new();
            rows.insert(idx + offset, row);
        }
    }

}

/// The config key an edited row belongs to, which is what the reload
/// classification is keyed on.
///
/// The engine's `Config` is flat apart from the four ordering lists, so a
/// nested path is decided by its CONTAINER and never by its leaf:
/// `inputs[0].name` follows `inputs`, not a key called "name". For a `.env`
/// row it is the variable name, which is where the engine keeps the PIN and
/// the JWT secret.
fn edited_config_key(row: &EditorRow) -> Option<&str> {
    match row.path.first() {
        Some(Seg::Key(k)) => Some(k.as_str()),
        _ => None,
    }
}

/// The changed engine settings that will not take effect until the engine is
/// restarted, each with the reason to show the operator.
///
/// Only rows whose value actually CHANGED count. A save re-writes the whole
/// file, so asking "is this key in the file" would report every key on every
/// save and the warning would stop meaning anything; the comparison is against
/// the value the editor loaded. Rows a "+" commit just created are included:
/// they have no original value, and a setting the operator has only just added
/// is exactly as boot-bound as one they edited.
fn engine_restart_notes(rows: &[EditorRow]) -> Vec<EngineRestartNote> {
    let mut notes: Vec<EngineRestartNote> = Vec::new();
    for row in rows {
        if row.kind != RowKind::Scalar || row.value == row.original {
            continue;
        }
        let Some(key) = edited_config_key(row) else { continue };
        if engine_key_reload(key) != Reload::NeedsRestart {
            continue;
        }
        // A key with several rows (the elements of an ordering list, a nested
        // group) is one setting and gets one warning.
        if notes.iter().any(|n| n.key == key) {
            continue;
        }
        notes.push(EngineRestartNote {
            key: key.to_string(),
            why: engine_key_why(key),
        });
    }
    notes
}

impl ModulesWindow {
    /// The row `self.selected` names, clamped into the current row space.
    ///
    /// Never fails: the engine row always exists, so a selection left pointing
    /// past the end of a shrunken module list resolves to the ENGINE rather
    /// than to some module the operator is not looking at. Every selection
    /// consumer goes through here, which is what keeps one index space for
    /// `selected` instead of two.
    fn selected_row(&self, stats: &GlobalStats) -> EntryRow {
        EntryRow::at(self.selected, stats.module_entries.len()).unwrap_or(EntryRow::Engine)
    }
}

impl Window for ModulesWindow {
    fn id(&self) -> WindowId {
        WindowId::Modules
    }

    fn selected_module_name(&self, stats: &GlobalStats) -> Option<String> {
        self.selected_row(stats).module_name(stats)
    }

    fn selection_is_module(&self, stats: &GlobalStats) -> bool {
        // The engine row is a real selection that is deliberately not a module,
        // and the app's fallback (act on the first known module) is exactly the
        // wrong thing to do with it.
        self.selected_row(stats).module_index().is_some()
    }

    fn selection_is_engine(&self, stats: &GlobalStats) -> bool {
        // Row 0. The two engine-only actions (restart / remove) are guarded on
        // this rather than on "an engine exists", so they can never be fired
        // from a MODULE row where the app would otherwise fall back to the first
        // known module.
        //
        // Stays `true` for a removed engine: the row is still selected, and
        // answering "no" there would hand the press to that same fallback.
        // Whether there is anything TO restart is a separate question, answered
        // by the action itself (`RestartOutcome::Removed`).
        matches!(self.selected_row(stats), EntryRow::Engine)
    }

    fn pending_link(&self) -> Option<(Rect, String)> {
        match (&self.link_rect, &self.link_url) {
            (Some(rect), Some(url)) => Some((*rect, url.clone())),
            _ => None,
        }
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, is_active: bool, stats: &GlobalStats, colors: &ColorConfig, hotkeys: &HotkeyConfig, prompts: &[PendingPrompt]) {
        // Config editor takes over the whole window.
        if self.editing.is_some() {
            self.render_editor(area, buf, is_active, colors, prompts);
            return;
        }

        let border_color = if is_active {
            colors.active_border_color("modules")
        } else {
            colors.border_color("inactive")
        };

        let block = Block::default()
            .title(" modules ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = area.inner(ratatui::layout::Margin { horizontal: 1, vertical: 1 });

        // Hotkey bar: wrapped to this window's width, so hints are no longer
        // clipped off the right edge on a narrow terminal. `inner` is shadowed
        // with the bar's content rect, so every bound below already stops short
        // of any rows the wrapped bar claims.
        //
        // It also NARROWS to the selected row: the engine is not a module, so
        // start/stop/delete are refused on its row, and advertising keys whose
        // press does nothing would be a bar that lies. `format_window_selected`
        // (not `format_window`) is what makes the narrowing real — the other
        // appends every binding in the window's map regardless of the order.
        //
        // The selected row is resolved ONCE, here, and used by the bar AND the
        // list below: with the engine in the list, `selected` indexes the
        // combined rows rather than `module_entries`, and the two have to agree
        // on which row that is or the bar describes a row the highlight is not
        // on.
        let module_count = stats.module_entries.len();
        let total_rows = EntryRow::total(module_count);
        let selected = clamp_selected(self.selected, total_rows);
        // Cannot be None: the clamp keeps `selected` inside the list and the
        // list always contains the engine row.
        let selected_row = EntryRow::at(selected, module_count).unwrap_or(EntryRow::Engine);
        let mut hotkey_text = "nav:[j|k|arrows]".to_string();
        hotkey_text.push(' ');
        // `edit` has two bindings on purpose (`e` and `E`); the bar prints the
        // one the operator presses, which is `E` (see `EDIT_PRIMARY_KEY`).
        hotkey_text.push_str(&hotkeys.format_window_selected(
            "modules",
            hint_labels(selected_row, stats.engine_removed),
            &[EDIT_PRIMARY_KEY],
        ));
        // The pause toggle is a GLOBAL (nav) binding, so it is not in
        // `window_actions` and `format_window` cannot see it — pull it in
        // explicitly so the one window that shows the pause state also shows
        // the key that changes it. It then wraps like every other hint.
        let global_pause = hotkeys.format_global_actions(&["pause"]);
        if !global_pause.is_empty() {
            hotkey_text.push(' ');
            hotkey_text.push_str(&global_pause);
        }
        let hotkey = crate::windows::hotkey_wrap::layout(
            &[(hotkey_text, Style::default().fg(Color::DarkGray))],
            area,
            inner,
        );
        let inner = hotkey.content;

        let mut y = inner.y;

        // ── the list: row 0 is the ENGINE, rows 1.. are the modules ──
        //
        // One list, one selection, one scroll. The engine used to be a status
        // line pinned above a modules-only list, which made it the one thing in
        // this window you could not act on. It is a row now, and the [ENGINE]
        // section header is gone with it: a header above row 0 would read as a
        // title for the module list underneath it.
        let engine_status_color = if stats.engine_status == "connected" && !stats.engine_removed {
            colors.status_color("online")
        } else {
            colors.status_color("offline")
        };
        // "There is a live engine to talk to" — the ONE fact the row's colour,
        // its text and its PAUSED badge all hang off. False once the operator
        // has removed the engine, whatever `engine_status` still says: a late
        // `Disconnected` must not be able to put a connection back on screen.
        let engine_live = !stats.engine_removed && stats.engine_status == "connected";

        let available_lines = (inner.y + inner.height).saturating_sub(y) as usize;
        let scroll = scroll_for(selected, self.scroll, available_lines, total_rows);

        // Modules that currently have an unanswered prompt waiting.
        let prompts_waiting: std::collections::HashSet<&str> =
            prompts.iter().map(|p| p.prompt.origin.as_str()).collect();

        let mut rendered_rows: HashMap<String, u16> = HashMap::new();
        for idx in scroll..total_rows {
            if y >= inner.y + inner.height || idx >= scroll + available_lines {
                break;
            }
            let Some(row) = EntryRow::at(idx, module_count) else { continue };
            let is_selected = idx == selected && is_active;
            let (line, name) = match row {
                EntryRow::Engine => {
                    // The engine is a different KIND of thing from a module and
                    // the row has to say so: a diamond instead of a name, one
                    // column in (modules sit two in), and no `[position]` tag,
                    // because it is in no stage. Selection is still the same
                    // full-row highlight a module gets, so "which row am I on"
                    // never depends on the row's shape.
                    let row_style = if is_selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    let mut spans = vec![
                        Span::styled(
                            "\u{25c6} ",
                            if is_selected { row_style } else { Style::default().fg(Color::Cyan) },
                        ),
                        Span::styled(
                            "cockatiel: ",
                            if is_selected { row_style } else { Style::default().fg(Color::DarkGray) },
                        ),
                    ];
                    // The row is STILL the engine's row after a removal — it is
                    // the record of "this TUI has no engine", and it is where
                    // the operator comes back to (`E`) to read
                    // `shutdown_on_request` and relaunch. What it must not show
                    // is a connection that no longer exists, so the status
                    // becomes a flat "no engine" instead of the last known
                    // connection, and the PAUSED badge (a fact about a live
                    // gate) goes with it.
                    let status_text = if stats.engine_removed {
                        ENGINE_REMOVED_STATUS
                    } else {
                        stats.engine_status.as_str()
                    };
                    spans.push(Span::styled(
                        status_text,
                        row_style.fg(if is_selected { Color::Black } else { engine_status_color }),
                    ));
                    // Flashing PAUSED, inline with the engine status so the row
                    // never changes shape (a blink must not trigger a full
                    // repaint). Uses the same warning-but-not-broken colour as
                    // the NEAR-LIMIT row below rather than a hard red: a held
                    // pipeline is the engine working as designed, not a fault.
                    if paused_indicator_visible(
                        engine_live,
                        stats.pipeline_paused,
                        crate::app::AppState::pause_flash_on(stats.pause_flash_tick),
                    ) {
                        spans.push(Span::styled(
                            "  PAUSED",
                            row_style
                                .fg(if is_selected { Color::Black } else { colors.status_color("stopped") })
                                .add_modifier(Modifier::BOLD),
                        ));
                    }
                    (Line::from(spans), None)
                }
                EntryRow::Module(mi) => {
                    let module = &stats.module_entries[mi];
                    let waiting = prompts_waiting.contains(module.name.as_str());
                    let status_text = if waiting { "waiting for prompt" } else { module.status.as_str() };
                    let status_color = if waiting {
                        Color::Cyan
                    } else {
                        colors.status_color(&module.status)
                    };
                    let row_style = if is_selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    (
                        Line::from(vec![
                            Span::styled(format!("  {:<20}", module.name), row_style.fg(if is_selected { Color::Black } else { Color::White })),
                            Span::styled(status_text, row_style.fg(status_color)),
                            Span::styled(format!("  [{}]", module.position), Style::default().fg(Color::DarkGray)),
                        ]),
                        Some(module.name.clone()),
                    )
                }
            };
            line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            if let Some(name) = name {
                rendered_rows.insert(name, y);
            }
            y += 1;
        }

        // Blank line
        y += 1;

        // ── [DATABASES] section ──
        if y < inner.y + inner.height {
            let header = Line::from(Span::styled("  [DATABASES]:", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
            header.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            y += 1;
        }

        let at_limit = stats.db_size_mb > (stats.db_target_mb as f64 * 0.95);
        let db_status = if at_limit {
            "NEAR-LIMIT"
        } else if stats.db_size_mb > 0.0 {
            "connected"
        } else {
            "disconnected"
        };
        let db_status_color = if db_status == "connected" {
            colors.status_color("online")
        } else if db_status == "NEAR-LIMIT" {
            colors.status_color("stopped")
        } else {
            colors.status_color("offline")
        };

        if y < inner.y + inner.height {
            let line = Line::from(vec![
                Span::styled("    timeline: ", Style::default().fg(Color::DarkGray)),
                Span::styled(db_status, Style::default().fg(db_status_color)),
                Span::styled(format!(" ({:.1}MB)", stats.db_size_mb), Style::default().fg(Color::DarkGray)),
            ]);
            line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            y += 1;
        }

        if y < inner.y + inner.height {
            let tl_bk = if stats.timeline_backup {
                ("set", colors.status_color("online"))
            } else {
                ("NONE", Color::Red)
            };
            let ud_bk = if stats.userdb_backup {
                ("set", colors.status_color("online"))
            } else {
                ("NONE", Color::Red)
            };
            let line = Line::from(vec![
                Span::styled("    backup:   timeline=", Style::default().fg(Color::DarkGray)),
                Span::styled(tl_bk.0, Style::default().fg(tl_bk.1)),
                Span::styled(" userdb=", Style::default().fg(Color::DarkGray)),
                Span::styled(ud_bk.0, Style::default().fg(ud_bk.1)),
            ]);
            line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            y += 1;
        }

        // Blank line
        y += 1;

        // ── [PLATFORMS] section ──
        if y < inner.y + inner.height {
            let header = Line::from(Span::styled("  [PLATFORMS]:", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
            header.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            y += 1;
        }

        let mut platforms: Vec<(&String, &u64)> = stats.platform_counts.iter().collect();
        platforms.sort_by_key(|(_, c)| std::cmp::Reverse(**c));

        if platforms.is_empty() {
            if y < inner.y + inner.height {
                let line = Line::from(Span::styled("    (no platforms connected)", Style::default().fg(Color::DarkGray)));
                line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            }
        } else {
            for (platform, count) in &platforms {
                if y >= inner.y + inner.height {
                    break;
                }
                let errors = stats.platform_errors.get(*platform).copied().unwrap_or(0);
                let platform_color = colors.platform_color(platform);
                let status_color = if errors > 0 {
                    colors.status_color("crashed")
                } else {
                    colors.status_color("online")
                };

                let line = Line::from(vec![
                    Span::styled(format!("    {:<12}", platform), Style::default().fg(platform_color)),
                    Span::styled("CONNECTED", Style::default().fg(status_color)),
                    Span::styled(format!("  {} msgs", count), Style::default().fg(Color::DarkGray)),
                    if errors > 0 {
                        Span::styled(format!("  {} err", errors), Style::default().fg(Color::Red))
                    } else {
                        Span::raw("")
                    },
                ]);
                line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
                y += 1;
            }
        }

        // Render border on top
        block.render(area, buf);

        // Hotkey bar (already wrapped above).
        Paragraph::new(hotkey.lines).render(hotkey.area, buf);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent, stats: &mut GlobalStats) -> Option<Action> {
        // Row 0 is the engine, so the row count is one MORE than the module
        // count and is never zero: with no modules registered at all there is
        // still the engine row to sit on and look at, which is why the old
        // "nothing to navigate" early return is gone. Every clamp is against
        // the combined space, so the up/down bounds can never land on a row
        // that does not exist (or, worse, on `module_entries[selected]` for a
        // `selected` that now means something else).
        let total = EntryRow::total(stats.module_entries.len());
        self.selected = clamp_selected(self.selected, total);
        self.scroll = clamp_selected(self.scroll, total);

        match key.code {
            crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => {
                self.selected = clamp_selected(self.selected + 1, total);
                self.scroll = self.selected;
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => {
                self.selected = clamp_selected(self.selected.saturating_sub(1), total);
                self.scroll = self.selected;
                Some(Action::Noop)
            }
            // `p` is NOT handled here. It is a global (nav) binding, matched
            // before the focused window's own key handling, so the pause toggle
            // works from every window and keeps working with the engine row
            // selected — reaching for it in here would shadow the global.
            crossterm::event::KeyCode::Char('w') => Some(Action::PopOut("modules".to_string())),
            _ => None,
        }
    }

    fn start_config_editor(&mut self, target: crate::app::ConfigTarget, label: &str, dir: PathBuf) {
        let rows = Self::load_rows(label, &dir);
        self.editing = Some(ConfigEditor {
            target: match target {
                crate::app::ConfigTarget::Engine => EditorTarget::Engine,
                crate::app::ConfigTarget::Module => EditorTarget::Module,
            },
            label: label.to_string(),
            dir,
            rows,
            selected: 0,
            scroll: 0,
            confirm_save: false,
        });
    }

    fn config_editor_target(&self, stats: &GlobalStats) -> Option<(crate::app::ConfigTarget, String, PathBuf)> {
        // Only the engine is answerable from in here: its files are its own and
        // no plugin list contains it, so `Action::EditConfig` would otherwise
        // have nothing to open. A module row returns None so the caller
        // resolves the module's directory from the plugin manifest, which is
        // where that knowledge lives.
        match self.selected_row(stats) {
            EntryRow::Engine => Some((
                crate::app::ConfigTarget::Engine,
                ENGINE_ROW_LABEL.to_string(),
                crate::supervisor::engine_dir(),
            )),
            EntryRow::Module(_) => None,
        }
    }

    fn in_editor(&self) -> bool {
        self.editing.is_some()
    }

fn editor_key(&mut self, key: crossterm::event::KeyEvent, hotkeys: &HotkeyConfig) -> bool {
        if self.editing.is_none() {
            return false;
        }
        use crate::hotkeys::EditorAction;
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut ed = self.editing.take().unwrap();

        if ed.confirm_save {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.editing = Some(ed);
                    self.save_editor();
                    return true;
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    // Discard changes: exit without saving.
                    self.editing = None;
                    return true;
                }
                KeyCode::Esc => {
                    // Cancel the confirmation, keep editing.
                    ed.confirm_save = false;
                    self.editing = Some(ed);
                    return true;
                }
                _ => {
                    self.editing = Some(ed);
                    return true;
                }
            }
        }

        match hotkeys.editor_action(&key) {
            Some(EditorAction::SaveExit) => {
                ed.confirm_save = true;
                self.editing = Some(ed);
                true
            }
            Some(EditorAction::MoveDown) => {
                if !ed.rows.is_empty() {
                    ed.selected = (ed.selected + 1).min(ed.rows.len() - 1);
                }
                self.editing = Some(ed);
                true
            }
            Some(EditorAction::MoveUp) => {
                ed.selected = ed.selected.saturating_sub(1);
                self.editing = Some(ed);
                true
            }
            Some(EditorAction::CursorLeft) => {
                if !ed.rows.is_empty() && ed.rows[ed.selected].kind != RowKind::Group {
                    ed.rows[ed.selected].cursor = ed.rows[ed.selected].cursor.saturating_sub(1);
                }
                self.editing = Some(ed);
                true
            }
            Some(EditorAction::CursorRight) => {
                if !ed.rows.is_empty() && ed.rows[ed.selected].kind != RowKind::Group {
                    let row = &mut ed.rows[ed.selected];
                    row.cursor = (row.cursor + 1).min(row.value.chars().count());
                }
                self.editing = Some(ed);
                true
            }
            Some(EditorAction::Commit) => {
                if ed.rows.is_empty() {
                    self.editing = Some(ed);
                    return true;
                }
                let idx = ed.selected;
                match ed.rows[idx].kind {
                    RowKind::AddMap => Self::commit_add_map(&mut ed.rows, idx),
                    RowKind::AddList => Self::commit_add_list(&mut ed.rows, idx),
                    _ => {
                        // Commit current value and move down (groups just move).
                        ed.rows[idx].cursor = ed.rows[idx].value.chars().count();
                        ed.selected = (idx + 1).min(ed.rows.len() - 1);
                    }
                }
                self.editing = Some(ed);
                true
            }
            None => {
                // Text editing on the selected row (scalar or "+" input).
                match key.code {
                    KeyCode::Backspace => {
                        if !ed.rows.is_empty() && ed.rows[ed.selected].kind != RowKind::Group {
                            let empty = ed.rows[ed.selected].value.is_empty();
                            let cursor = ed.rows[ed.selected].cursor;
                            let is_scalar = ed.rows[ed.selected].kind == RowKind::Scalar;
                            if empty && is_scalar && cursor == 0 {
                                // Backspace on an empty value removes the row
                                // (map key / env var / array element).
                                let idx = ed.selected;
                                Self::remove_scalar_row(&mut ed.rows, idx);
                                ed.selected = idx.min(ed.rows.len().saturating_sub(1));
                            } else {
                                let row = &mut ed.rows[ed.selected];
                                let mut chars: Vec<char> = row.value.chars().collect();
                                let pos = row.cursor.min(chars.len());
                                if pos > 0 {
                                    chars.remove(pos - 1);
                                    row.value = chars.into_iter().collect();
                                    row.cursor = pos - 1;
                                }
                            }
                        }
                    }
                    KeyCode::Delete => {
                        if !ed.rows.is_empty() && ed.rows[ed.selected].kind != RowKind::Group {
                            let row = &mut ed.rows[ed.selected];
                            let mut chars: Vec<char> = row.value.chars().collect();
                            if row.cursor < chars.len() {
                                chars.remove(row.cursor);
                                row.value = chars.into_iter().collect();
                            }
                        }
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        if !ed.rows.is_empty() && ed.rows[ed.selected].kind != RowKind::Group {
                            let row = &mut ed.rows[ed.selected];
                            let mut chars: Vec<char> = row.value.chars().collect();
                            let pos = row.cursor.min(chars.len());
                            chars.insert(pos, c);
                            row.value = chars.into_iter().collect();
                            row.cursor = pos + 1;
                        }
                    }
                    _ => {}
                }
                self.editing = Some(ed);
                true
            }
        }
    }

    fn editor_paste(&mut self, text: &str) -> bool {
        if self.editing.is_none() {
            return false;
        }
        let mut ed = self.editing.take().unwrap();
        if !ed.rows.is_empty() {
            let row = &mut ed.rows[ed.selected];
            let mut chars: Vec<char> = row.value.chars().collect();
            let pos = row.cursor.min(chars.len());
            for c in text.chars() {
                chars.insert(pos, c);
            }
            row.value = chars.into_iter().collect();
            row.cursor = pos + text.chars().count();
        }
        self.editing = Some(ed);
        true
    }

    fn take_saved_module(&mut self) -> Option<String> {
        self.last_saved_module.take()
    }

    fn take_saved_engine(&mut self) -> Option<Vec<EngineRestartNote>> {
        self.last_saved_engine.take()
    }
}
#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::ModulesWindow;
    use crate::app::{ConfigTarget, Window};
    use crate::db::ModuleStatus;
    use crate::hotkeys::default_hotkeys;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }

    /// `ModuleStatus` has no `Default`, and building one by hand in every test
    /// drowns the assertion in field names.
    pub(super) fn module(name: &str) -> ModuleStatus {
        ModuleStatus {
            name: name.to_string(),
            description: String::new(),
            status: "connected".to_string(),
            position: "preprocess".to_string(),
            credentials: Vec::new(),
            directory: String::new(),
            credential_values: Default::default(),
            config_complete: true,
            alive: true,
            last_seen: 0,
        }
    }

    /// Stats with `n` connected modules, so the unified list has 1 + n rows.
    pub(super) fn stats_with_modules(n: usize) -> crate::db::GlobalStats {
        crate::db::GlobalStats {
            module_entries: (0..n).map(|i| module(&format!("m{}", i))).collect(),
            ..Default::default()
        }
    }

    /// The window rendered to a plain string, for asserting on what is on
    /// screen rather than on the state that produced it.
    pub(super) fn render(win: &mut ModulesWindow, stats: &crate::db::GlobalStats, w: u16, h: u16) -> String {
        render_with(win, stats, w, h, &default_hotkeys())
    }

    /// As [`render`], with an explicit key map: the running app loads
    /// `hotkey_config.json` on top of the defaults, so a test that asserts on
    /// the hint bar has to see the same bindings the operator does.
    pub(super) fn render_with(
        win: &mut ModulesWindow,
        stats: &crate::db::GlobalStats,
        w: u16,
        h: u16,
        hotkeys: &crate::hotkeys::HotkeyConfig,
    ) -> String {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
        let colors = crate::colors::load_colors(&std::path::PathBuf::from(""));
        win.render(Rect::new(0, 0, w, h), &mut buf, true, stats, &colors, hotkeys, &[]);
        let mut all = String::new();
        for y in 0..h {
            for x in 0..w {
                all.push_str(buf[(x, y)].symbol());
            }
            all.push('\n');
        }
        all
    }

    #[test]
    fn json_value_preserves_types() {
        assert_eq!(ModulesWindow::json_value("0.5", false), serde_json::json!(0.5));
        assert_eq!(ModulesWindow::json_value("123", false), serde_json::json!(123));
        assert_eq!(ModulesWindow::json_value("true", false), serde_json::json!(true));
        assert_eq!(ModulesWindow::json_value("hello world", false), serde_json::json!("hello world"));
    }

    #[test]
    fn editor_loads_nested_and_adds_rows() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-editor-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".env"), "K1=v1
SECRET=s3
").unwrap();
        std::fs::write(
            tmp.join("config.json"),
            r#"{"model":"mms","module_specific":{"servers":{"1543036894273732640":["ch1","ch2"]}}}"#,
        )
        .unwrap();

        let mut w = ModulesWindow::new();
        w.start_config_editor(ConfigTarget::Module, "test-mod", tmp.clone());
        assert!(w.in_editor());
        let hk = default_hotkeys();

        // Nested object expanded: servers.<guild>[0] and [1] are editable rows.
        fn path_str(rows: &[super::EditorRow]) -> Vec<String> {
            rows.iter()
                .map(|r| {
                    use super::Seg;
                    let mut s = String::new();
                    for seg in &r.path {
                        match seg {
                            Seg::Key(k) => {
                                if !s.is_empty() { s.push('.'); }
                                s.push_str(k);
                            }
                            Seg::Idx(i) => s.push_str(&format!("[{}]", i)),
                        }
                    }
                    s
                })
                .collect()
        }
        let rows = path_str(&w.editing.as_ref().unwrap().rows);
        assert!(rows.iter().any(|d| d == "module_specific.servers.1543036894273732640[0]"), "rows: {:?}", rows);
        assert!(rows.iter().any(|d| d == "module_specific.servers.1543036894273732640[1]"), "rows: {:?}", rows);
        // There is a "+ add item" (AddList) under the server's channel array.
        assert!(w.editing.as_ref().unwrap().rows.iter().any(|r| r.kind == super::RowKind::AddList), "no AddList row");

        // Find the first "+ add item" row and add a channel via Commit.
        let add_idx = w.editing.as_ref().unwrap().rows.iter().position(|r| r.kind == super::RowKind::AddList).unwrap();
        // Navigate to it.
        while w.editing.as_ref().unwrap().selected < add_idx {
            w.editor_key(key('j'), &hk);
        }
        // Type a new channel and commit.
        for c in "ch3".chars() {
            w.editor_key(key(c), &hk);
        }
        w.editor_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), &hk);
        // The new element row exists now.
        let rows2 = path_str(&w.editing.as_ref().unwrap().rows);
        assert!(rows2.iter().any(|d| d.ends_with("[2]")), "rows2: {:?}", rows2);

        // Esc asks to confirm; y saves + exits.
        w.editor_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &hk);
        assert!(w.in_editor(), "should still be editing until confirmed");
        assert!(w.editing.as_ref().unwrap().confirm_save, "confirm_save not set");
        w.editor_key(key('y'), &hk);
        assert!(!w.in_editor());

        let env = std::fs::read_to_string(tmp.join(".env")).unwrap();
        assert!(env.contains("K1=v1"), "env: {}", env);
        assert!(env.contains("SECRET=s3"), "env: {}", env);

        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg["model"], "mms", "cfg: {}", cfg);
        assert_eq!(
            cfg["module_specific"]["servers"]["1543036894273732640"],
            serde_json::json!(["ch1", "ch2", "ch3"]),
            "cfg: {}",
            cfg
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
    /// The reported bug: on a narrow window the tail of the hotkey bar was
    /// clipped away, so hints only reappeared when the terminal was fullscreened.
    /// Render the same window at two widths and assert nothing is lost.
    ///
    /// Driven with a MODULE row selected, because the bar narrows to the
    /// selected row and this is the set with the most to wrap.
    /// `the_engine_row_offers_only_engine_actions` covers the narrowed bar.
    #[test]
    fn hotkey_bar_wraps_instead_of_clipping_in_a_narrow_window() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let render_to_string = |w: u16, h: u16| -> String {
            let mut win = ModulesWindow::new();
            // Row 1 is the first module, so the per-module hint set is on show.
            win.selected = 1;
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
            let colors = crate::colors::load_colors(&std::path::PathBuf::from(""));
            let stats = stats_with_modules(2);
            let hk = default_hotkeys();
            win.render(Rect::new(0, 0, w, h), &mut buf, true, &stats, &colors, &hk, &[]);
            let mut all = String::new();
            for y in 0..h {
                for x in 0..w {
                    all.push_str(buf[(x, y)].symbol());
                }
                all.push('\n');
            }
            all
        };

        let wide = render_to_string(200, 40);
        let narrow = render_to_string(46, 40);

        // Every hint the bar contains must be visible at BOTH widths.
        //
        // `format_window_selected` with the renderer's primary keys, not
        // `format_window`: `edit` has two bindings on purpose and the bar
        // prints ONE of them (see `EDIT_PRIMARY_KEY`), so the expectation has
        // to be the narrowed form or this test would be asking for a hint the
        // bar never prints. WHICH key that is is pinned by its own test — this
        // one is about wrapping, not about bindings.
        let bar = default_hotkeys().format_window_selected(
            "modules",
            &["start", "stop", "del", "auto", "copy", "creds", "edit", "clear", "test", "select", "popout"],
            &[super::EDIT_PRIMARY_KEY],
        );
        let mut hints: Vec<String> = bar
            .split(", ")
            .filter(|s| !s.is_empty())
            .map(|s| s.trim().to_string())
            .collect();
        // The pause toggle is a global binding advertised by this bar, so it
        // must wrap like the rest rather than be dropped on a narrow terminal.
        let pause_hint = default_hotkeys().format_global_actions(&["pause"]);
        assert_eq!(pause_hint, "pause:[p]");
        hints.push(pause_hint);
        assert!(hints.len() >= 8, "expected a populated bar, got {:?}", bar);
        for hint in &hints {
            assert!(wide.contains(hint.as_str()), "wide render lost {:?}", hint);
            assert!(
                narrow.contains(hint.as_str()),
                "narrow render lost {:?} — the bar clipped instead of wrapping:\n{}",
                hint,
                narrow
            );
        }
        // And the narrow render genuinely had to wrap rather than fit.
        assert_ne!(
            narrow.lines().filter(|l| l.contains(":[")).count(),
            0,
            "expected hint rows in the narrow render"
        );
    }

    /// The engine link decides whether a pause is even meaningful, so the
    /// indicator is a three-way rule, not a two-way one. Asserted both as the
    /// pure rule and through the real renderer, because a renderer's `if` can
    /// drift from the function it is supposed to be calling.
    #[test]
    fn the_paused_indicator_needs_a_connected_paused_engine() {
        use super::paused_indicator_visible;
        // Connected + paused → shown (in its visible phase).
        assert!(paused_indicator_visible(true, true, true));
        // Connected + running → nothing to report.
        assert!(!paused_indicator_visible(true, false, true));
        // Disconnected + paused → there is no engine holding anything.
        assert!(!paused_indicator_visible(false, true, true));
        // Disconnected + running.
        assert!(!paused_indicator_visible(false, false, true));
        // The blink's dark phase hides it even when paused: that is the flash.
        assert!(!paused_indicator_visible(true, true, false));
    }

    #[test]
    fn the_indicator_flashes_only_while_connected_and_paused() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let render = |engine_status: &str, paused: bool, tick: u64| -> String {
            let mut win = ModulesWindow::new();
            let (w, h) = (120u16, 40u16);
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
            let stats = crate::db::GlobalStats {
                engine_status: engine_status.to_string(),
                pipeline_paused: paused,
                // Phase 0 is the visible half of the flash (see AppState::PAUSE_FLASH_TICKS).
                pause_flash_tick: tick,
                ..Default::default()
            };
            let colors = crate::colors::load_colors(&std::path::PathBuf::from(""));
            win.render(
                Rect::new(0, 0, w, h),
                &mut buf,
                true,
                &stats,
                &colors,
                &default_hotkeys(),
                &[],
            );
            let mut all = String::new();
            for y in 0..h {
                for x in 0..w {
                    all.push_str(buf[(x, y)].symbol());
                }
                all.push('\n');
            }
            all
        };
        let visible_tick = 0u64;
        let dark_tick = crate::app::AppState::PAUSE_FLASH_TICKS;

        let paused = render("connected", true, visible_tick);
        assert!(paused.contains("PAUSED"), "connected+paused must show PAUSED:\n{}", paused);
        // Next to the engine status on the same row, and in the same warning
        // colour family the NEAR-LIMIT row uses.
        let row = paused.lines().find(|l| l.contains("PAUSED")).expect("paused row");
        assert!(row.contains("cockatiel: connected"), "indicator not beside the engine status: {:?}", row);

        assert!(
            !render("connected", true, dark_tick).contains("PAUSED"),
            "the flash's dark phase must clear the indicator"
        );
        let running = render("connected", false, visible_tick);
        assert!(running.contains("cockatiel: connected"), "baseline render: {}", running);
        assert!(!running.contains("PAUSED"), "connected+running must not show PAUSED:\n{}", running);

        let gone = render("disconnected", true, visible_tick);
        assert!(!gone.contains("PAUSED"), "disconnected+paused must not show PAUSED:\n{}", gone);
    }

    #[test]
    fn render_shows_tree_masking_and_pluses() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-render-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".env"), "DISCORD_BOT_TOKEN=sekret123\n").unwrap();
        std::fs::write(
            tmp.join("config.json"),
            r#"{"model":"mms","module_specific":{"servers":{"1543036894273732640":["ch1","ch2"]}}}"#,
        )
        .unwrap();

        let mut w = ModulesWindow::new();
        w.start_config_editor(ConfigTarget::Module, "test-mod", tmp.clone());

        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let mut buf = Buffer::empty(Rect::new(0, 0, 120, 60));
        let colors = crate::colors::load_colors(&std::path::PathBuf::from(""));
        let stats = crate::db::GlobalStats::default();
        let hk = default_hotkeys();
        w.render(Rect::new(0, 0, 120, 60), &mut buf, true, &stats, &colors, &hk, &[]);

        let mut all = String::new();
        for y in 0..60u16 {
            for x in 0..120u16 {
                all.push_str(buf[(x, y)].symbol());
            }
            all.push('\n');
        }
        assert!(all.contains(".env (secrets"), "no env header");
        assert!(all.contains("*****"), "env value not masked: {}", all);
        assert!(all.contains("DISCORD_BOT_TOKEN"), "env key missing: {}", all);
        assert!(all.contains("model : mms"), "json value not visible: {}", all);
        assert!(all.contains("1543036894273732640"), "nested key missing: {}", all);
        assert!(all.contains('+'), "no add rows: {}", all);
        assert!(all.contains('\u{2502}'), "no tree lines: {}", all);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn editor_esc_n_discards_changes() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-discard-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".env"), "SECRET=s3\n").unwrap();
        std::fs::write(tmp.join("config.json"), r#"{"model":"mms"}"#).unwrap();

        let mut w = ModulesWindow::new();
        w.start_config_editor(ConfigTarget::Module, "test-mod", tmp.clone());
        let hk = default_hotkeys();

        // Edit a value.
        w.editor_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()), &hk);
        w.editor_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()), &hk);
        for c in "MODIFIED".chars() {
            w.editor_key(key(c), &hk);
        }
        // Esc → n discards: editor exits, file untouched.
        w.editor_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &hk);
        assert!(w.in_editor(), "should await confirmation");
        w.editor_key(key('n'), &hk);
        assert!(!w.in_editor(), "discard should exit");

        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg["model"], "mms", "discard should not save: {}", cfg);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn editor_backspace_on_empty_removes_row() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-editor-rem-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("config.json"), r#"{"model":"mms","channels":["a","b"]}"#).unwrap();

        let mut w = ModulesWindow::new();
        w.start_config_editor(ConfigTarget::Module, "test-mod", tmp.clone());
        let hk = default_hotkeys();

        // Remove `model`: select it, clear its value, then backspace to trigger
        // removal of the empty row.
        let model_idx = w
            .editing
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|r| r.kind == super::RowKind::Scalar && r.path.last() == Some(&super::Seg::Key("model".into())))
            .unwrap();
        while w.editing.as_ref().unwrap().selected < model_idx {
            w.editor_key(key('j'), &hk);
        }
        for _ in 0.."mms".chars().count() {
            w.editor_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()), &hk);
        }
        w.editor_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()), &hk);
        assert!(
            !w.editing.as_ref().unwrap().rows.iter().any(|r| r.path.last() == Some(&super::Seg::Key("model".into()))),
            "model row should be removed"
        );

        // Remove channels[0]; channels[1] must re-index to [0].
        let c0 = w
            .editing
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|r| r.path.last() == Some(&super::Seg::Idx(0)))
            .unwrap();
        // Navigate to c0 in whichever direction it lies from the current row.
        loop {
            let sel = w.editing.as_ref().unwrap().selected;
            if sel < c0 {
                w.editor_key(key('j'), &hk);
            } else if sel > c0 {
                w.editor_key(key('k'), &hk);
            } else {
                break;
            }
        }
        // Clear "a" then backspace again to remove the now-empty row.
        w.editor_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()), &hk);
        w.editor_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()), &hk);
        let remaining: Vec<usize> = w
            .editing
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .filter_map(|r| match r.path.last() {
                Some(super::Seg::Idx(i)) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(remaining, vec![0], "channels[1] should re-index to [0]: {:?}", remaining);

        // Esc → y saves; model gone, channels = ["b"].
        w.editor_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &hk);
        w.editor_key(key('y'), &hk);
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.join("config.json")).unwrap()).unwrap();
        assert!(cfg.get("model").is_none(), "cfg: {}", cfg);
        assert_eq!(cfg["channels"], serde_json::json!(["b"]), "cfg: {}", cfg);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod editor_scroll_tests {
    use super::{
        editor_lines, editor_scroll_for, editor_selected_line, EditorLine, EditorRow, ModulesWindow,
        RowKind, Seg, EDITOR_SCROLL_MARGIN,
    };
    use crate::app::ConfigTarget;

    fn row(source: &str, depth: usize) -> EditorRow {
        EditorRow {
            source: source.to_string(),
            path: vec![Seg::Key(format!("k{}", depth))],
            value: "v".to_string(),
            cursor: 0,
            is_secret: source == "env",
            kind: RowKind::Scalar,
            add_base: String::new(),
            original: "v".to_string(),
        }
    }

    /// `n` rows in one section.
    fn rows_in_one_section(n: usize) -> Vec<EditorRow> {
        (0..n).map(|i| row("json", i)).collect()
    }

    // ── the display-line model ──────────────────────────────────────────

    #[test]
    fn a_row_costs_two_display_lines_and_a_section_costs_one_header() {
        let lines = editor_lines(&rows_in_one_section(3));
        // 1 header + (3 rows x 2 lines) - 1 (no trailing blank)
        assert_eq!(lines.len(), 1 + 3 * 2 - 1);
        assert!(matches!(lines[0], EditorLine::Header { .. }));
        assert_eq!(
            lines.iter().filter(|l| matches!(l, EditorLine::Header { .. })).count(),
            1
        );
    }

    #[test]
    fn a_header_appears_once_per_section_not_once_per_scrolled_row() {
        // The old renderer compared against an empty previous source, so it drew
        // a header for the first visible row no matter which section it was in.
        let mut rows = rows_in_one_section(3);
        rows.extend((0..3).map(|i| row("env", i)));
        let lines = editor_lines(&rows);
        assert_eq!(
            lines.iter().filter(|l| matches!(l, EditorLine::Header { .. })).count(),
            2,
            "exactly two headers: one per source"
        );
    }

    #[test]
    fn display_lines_are_not_one_to_one_with_rows() {
        // This mismatch is the bug: scroll maths that assumed 1 line per row
        // under-counted the viewport and let the cursor row fall off the bottom.
        let rows = rows_in_one_section(10);
        assert!(
            editor_lines(&rows).len() > rows.len(),
            "display lines must exceed row count, or the old maths was fine"
        );
    }

    #[test]
    fn the_selected_row_maps_to_its_own_display_line() {
        let lines = editor_lines(&rows_in_one_section(8));
        for (row_idx, _) in rows_in_one_section(8).iter().enumerate() {
            let at = editor_selected_line(&lines, row_idx);
            assert!(
                matches!(lines[at], EditorLine::Row { index, .. } if index == row_idx),
                "row {} resolved to the wrong display line {}",
                row_idx,
                at
            );
        }
    }

    // ── the 3-line scroll margin ────────────────────────────────────────

    #[test]
    fn the_view_does_not_scroll_until_the_cursor_is_within_the_margin() {
        // 20 visible lines, cursor at visual line 5 (0-indexed): 5 lines of
        // context above, so scroll must stay put.
        let s = editor_scroll_for(5, 0, 20, EDITOR_SCROLL_MARGIN, 200);
        assert_eq!(s, 0, "scrolled too eagerly at line 5");
    }

    #[test]
    fn the_cursor_is_pushed_to_the_third_line_from_the_top() {
        // Cursor at line 2 with scroll 0: too close to the top, so scroll back
        // until the cursor sits `margin` lines down.
        let s = editor_scroll_for(2, 0, 20, EDITOR_SCROLL_MARGIN, 200);
        assert_eq!(s, 0, "cannot scroll above the start of the content");
        // Now from a scrolled position: line 3 with scroll 0 -> stay.
        assert_eq!(editor_scroll_for(3, 0, 20, EDITOR_SCROLL_MARGIN, 200), 0);
        // Line 4 is one past the margin; still no scroll needed.
        assert_eq!(editor_scroll_for(4, 0, 20, EDITOR_SCROLL_MARGIN, 200), 0);
    }

    #[test]
    fn the_cursor_is_pushed_to_the_third_line_from_the_bottom() {
        // Cursor at line 25, viewport 20, scroll 0: needs scroll = 25+3+1-20 = 9,
        // leaving the cursor at index 16, which is 3 lines up from the bottom.
        let s = editor_scroll_for(25, 0, 20, EDITOR_SCROLL_MARGIN, 200);
        assert_eq!(s, 9);
        let cursor_at = 25 - s;
        assert_eq!(cursor_at, 16);
        assert_eq!(
            (20 - 1) - cursor_at,
            EDITOR_SCROLL_MARGIN,
            "cursor should sit 3 lines up from the bottom edge"
        );
    }

    #[test]
    fn the_view_only_scrolls_once_the_cursor_enters_the_margin_band() {
        // Stepping down one line at a time, the view must hold still until the
        // cursor is within the margin, then track it so the cursor stays exactly
        // 3 lines up from the bottom edge.
        let visible = 20usize;
        let mut scroll = 0usize;
        let mut first_scroll_at = None;
        for line in 0..40 {
            let next = editor_scroll_for(line, scroll, visible, EDITOR_SCROLL_MARGIN, 200);
            if next != scroll {
                if first_scroll_at.is_none() {
                    first_scroll_at = Some(line);
                }
                // Once scrolling has begun, the cursor is pinned 3 from the
                // bottom on every subsequent step.
                assert_eq!(
                    line - next,
                    visible - 1 - EDITOR_SCROLL_MARGIN,
                    "cursor drifted from the margin at line {}",
                    line
                );
            }
            scroll = next;
        }
        // Scrolling starts when the cursor first has fewer than `margin` lines
        // below it: line 16 still has 3, line 17 has only 2.
        assert_eq!(first_scroll_at, Some(visible - EDITOR_SCROLL_MARGIN));
    }

    #[test]
    fn the_view_is_still_while_the_cursor_has_room_to_move() {
        // No scrolling at all while the cursor travels through the middle band.
        let visible = 20usize;
        let scroll = 0usize;
        for line in EDITOR_SCROLL_MARGIN..=(visible - 1 - EDITOR_SCROLL_MARGIN) {
            assert_eq!(
                editor_scroll_for(line, scroll, visible, EDITOR_SCROLL_MARGIN, 200),
                scroll,
                "scrolled at line {} but the cursor had room on both sides",
                line
            );
        }
    }

    #[test]
    fn the_cursor_is_always_visible_no_matter_where_it_goes() {
        // Walk the cursor across a long list, keeping the real scroll, and assert
        // it never lands outside the viewport.
        let total = 500usize;
        let visible = 20usize;
        let mut scroll = 0usize;
        for line in 0..total {
            scroll = editor_scroll_for(line, scroll, visible, EDITOR_SCROLL_MARGIN, total);
            assert!(
                line >= scroll && line < scroll + visible,
                "cursor at display line {} is outside the viewport {}..{} (scroll {})",
                line,
                scroll,
                scroll + visible,
                scroll
            );
        }
    }

    #[test]
    fn a_short_viewport_degrades_to_keeping_the_cursor_visible() {
        // With fewer than 2*margin+1 lines there is no way to honour both
        // margins; the cursor must still never be pushed off screen.
        for visible in 1..=(EDITOR_SCROLL_MARGIN * 2) {
            let total = 40usize;
            for line in 0..total {
                let s = editor_scroll_for(line, 0, visible, EDITOR_SCROLL_MARGIN, total);
                assert!(
                    line >= s && line < s + visible,
                    "visible={} line={} scroll={} -> cursor off screen",
                    visible,
                    line,
                    s
                );
            }
        }
    }

    #[test]
    fn the_view_never_scrolls_past_the_end_of_the_content() {
        // Cursor on the very last line: the view should sit flush with the end,
        // not overshoot into blank space.
        let total = 25usize;
        let visible = 10usize;
        let s = editor_scroll_for(total - 1, 0, visible, EDITOR_SCROLL_MARGIN, total);
        assert_eq!(s, total - visible, "should sit flush with the end");
    }

    #[test]
    fn a_zero_height_viewport_is_safe() {
        assert_eq!(editor_scroll_for(5, 3, 0, EDITOR_SCROLL_MARGIN, 10), 0);
    }

    /// The end-to-end version of the reported bug: drive the REAL editor through
    /// real keypresses in a short window and assert the selected row's text is
    /// always on screen.
    #[test]
    fn the_selected_row_stays_on_screen_in_a_short_window() {
        use crate::app::Window;
        use crate::hotkeys::default_hotkeys;
        use crossterm::event::{KeyCode, KeyEvent};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use std::path::PathBuf;

        let tmp = std::env::temp_dir().join(format!("cockatiel-editor-scroll-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()));
        std::fs::create_dir_all(&tmp).unwrap();
        // 12 settings -> plenty of rows for a 10-row window.
        let mut cfg = serde_json::Map::new();
        let mut ms = serde_json::Map::new();
        for i in 0..12 {
            ms.insert(format!("setting_number_{:02}", i), serde_json::json!(i));
        }
        cfg.insert("module_specific".to_string(), serde_json::Value::Object(ms));
        std::fs::write(
            tmp.join("config.json"),
            serde_json::to_string_pretty(&serde_json::Value::Object(cfg)).unwrap(),
        )
        .unwrap();

        // Short window: 10 rows tall, so the content overflows.
        let (w, h) = (60u16, 10u16);
        let mut win = ModulesWindow::new();
        win.start_config_editor(ConfigTarget::Module, "test-mod", tmp.clone());

        let colors = crate::colors::load_colors(&PathBuf::from(""));
        let stats = crate::db::GlobalStats::default();
        let hk = default_hotkeys();
        let area = Rect::new(0, 0, w, h);

        for step in 0..14 {
            let mut buf = Buffer::empty(area);
            win.render(area, &mut buf, true, &stats, &colors, &hk, &[]);
            let mut screen = String::new();
            for y in 0..h {
                for x in 0..w {
                    screen.push_str(buf[(x, y)].symbol());
                }
                screen.push('\n');
            }
            let ed = win.editing.as_ref().expect("still editing");
            // The selected row is drawn with a cyan highlight, so the presence
            // of cyan cells proves the selected row is actually on screen --
            // independent of what the row is called.
            let highlighted = (0..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .filter(|(x, y)| buf[(*x, *y)].bg == ratatui::style::Color::Cyan)
                .count();
            assert!(
                highlighted > 0,
                "step {}: the selected row (row {} of {}) is scrolled off screen -- \
                 nothing is highlighted:\n{}",
                step,
                ed.selected,
                ed.rows.len(),
                screen
            );
            if step < 13 {
                win.editor_key(
                    KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::NONE),
                    &hk,
                );
            }
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod editor_shape_tests {
    use super::ModulesWindow;
    use crate::app::{AppState, ConfigTarget, Window};

    fn app_with(win: ModulesWindow) -> AppState {
        let mut s = AppState::new(
            crate::colors::load_colors(&std::path::PathBuf::from("")),
            crate::hotkeys::default_hotkeys(),
        );
        s.windows = vec![Box::new(win)];
        s
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cockatiel-shape-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), r#"{"module_specific":{"a":1,"b":2,"c":3}}"#).unwrap();
        dir
    }

    /// Entering the config editor takes over the whole modules pane with a
    /// completely different layout, so the draw loop must treat it as a screen
    /// shape change and repaint in full rather than diff a stale frame.
    #[test]
    fn entering_the_editor_changes_the_screen_shape() {
        let idle = app_with(ModulesWindow::new()).screen_shape(80, 24);

        let dir = scratch("edit");
        let mut w = ModulesWindow::new();
        w.start_config_editor(ConfigTarget::Module, "test-mod", dir.clone());
        assert!(w.in_editor(), "the editor should be open");
        let editing = app_with(w).screen_shape(80, 24);
        assert_ne!(
            editing, idle,
            "entering the config editor must register as a shape change"
        );

        // A window that never opened the editor is back to the idle shape, so
        // leaving the editor drops back to the same fingerprint.
        assert_eq!(app_with(ModulesWindow::new()).screen_shape(80, 24), idle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod engine_row_tests {
    //! The engine became a selectable row (row 0) rather than a status line
    //! above the module list. `self.selected` therefore indexes a list that is
    //! one longer than `stats.module_entries`, and every site that used to
    //! index `module_entries` directly had to be re-derived. These tests pin the
    //! new index space and the scroll arithmetic, because the failure mode of
    //! getting it wrong is silent: the wrong row highlights, the wrong module
    //! gets stopped, or the list scrolls wrong.
    use super::{
        clamp_selected, scroll_for, EntryRow, ENGINE_REMOVED_STATUS, ENGINE_ROW,
        ENGINE_ROW_LABEL, Reload, ENGINE_CONFIG_KEYS,
    };
    use crate::app::{ConfigTarget, Window};
    use crate::db::GlobalStats;
    use crate::hotkeys::default_hotkeys;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::tests::stats_with_modules;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }

    fn down() -> KeyEvent {
        KeyEvent::new(KeyCode::Down, KeyModifiers::empty())
    }

    fn up() -> KeyEvent {
        KeyEvent::new(KeyCode::Up, KeyModifiers::empty())
    }

    // ── the row model ────────────────────────────────────────────────────

    #[test]
    fn the_list_is_the_engine_then_every_module() {
        let stats = stats_with_modules(3);
        // 3 modules + the engine.
        assert_eq!(EntryRow::total(stats.module_entries.len()), 4);

        assert_eq!(EntryRow::at(0, 3), Some(EntryRow::Engine));
        assert_eq!(EntryRow::at(1, 3), Some(EntryRow::Module(0)));
        assert_eq!(EntryRow::at(2, 3), Some(EntryRow::Module(1)));
        assert_eq!(EntryRow::at(3, 3), Some(EntryRow::Module(2)));
        // Past the end is not a row, rather than a wrapped index into
        // module_entries (which is how "select module 0 twice" happens).
        assert_eq!(EntryRow::at(4, 3), None);
    }

    #[test]
    fn the_engine_row_has_no_module() {
        let stats = stats_with_modules(2);
        // The accessor must refuse rather than hand back a neighbouring row's
        // module: `x` with the engine selected would otherwise stop module 0.
        assert_eq!(EntryRow::Engine.module_index(), None);
        assert_eq!(EntryRow::Engine.module_name(&stats), None);
        assert_eq!(EntryRow::Module(1).module_name(&stats), Some("m1".to_string()));
    }

    #[test]
    fn the_row_count_is_never_zero() {
        // The engine row exists even with no modules, so an empty list still has
        // something selectable (and the old "nothing to navigate" early return
        // is gone).
        assert_eq!(EntryRow::total(0), 1);
        assert_eq!(EntryRow::at(0, 0), Some(EntryRow::Engine));
        assert_eq!(EntryRow::at(1, 0), None);
    }

    #[test]
    fn the_window_reports_the_engine_row_as_not_a_module() {
        let mut w = super::ModulesWindow::new();
        let stats = stats_with_modules(2);
        // Default selection is the engine.
        assert_eq!(w.selected, ENGINE_ROW);
        assert!(!w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats), None);

        w.selected = 1;
        assert!(w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats), Some("m0".to_string()));
    }

    // ── selection + scroll arithmetic ─────────────────────────────────────

    #[test]
    fn navigation_clamps_at_both_ends_of_the_combined_list() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(2);
        let total = EntryRow::total(2);
        assert_eq!(total, 3);

        // The first row is the engine and up must not go above it.
        w.handle_key(up(), &mut stats);
        assert_eq!(w.selected, 0);
        assert_eq!(w.selected, ENGINE_ROW);

        // The last row is the last MODULE, and down must not wrap to 0.
        for _ in 0..10 {
            w.handle_key(down(), &mut stats);
        }
        assert_eq!(w.selected, total - 1);
        assert_eq!(w.selected, 2, "row 2 is module_entries[1], not module_entries[2]");

        // ...and back up to the engine, crossing the boundary exactly once.
        w.handle_key(up(), &mut stats);
        assert_eq!(w.selected, 1);
        assert_eq!(w.selected_module_name(&stats).as_deref(), Some("m0"));
        w.handle_key(up(), &mut stats);
        assert_eq!(w.selected, ENGINE_ROW);
        assert_eq!(w.selected_module_name(&stats), None);
    }

    #[test]
    fn a_shrunken_module_list_pulls_the_selection_back_into_range() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(3);
        for _ in 0..3 {
            w.handle_key(down(), &mut stats);
        }
        assert_eq!(w.selected, 3);
        // The engine drops two of its modules (a module removed itself).
        stats.module_entries.truncate(1);
        w.handle_key(key('j'), &mut stats);
        // 1 module + engine = 2 rows, so the selection clamps to row 1 = m0 and
        // the next press cannot walk off the end.
        assert_eq!(w.selected, 1);
        w.handle_key(down(), &mut stats);
        assert_eq!(w.selected, 1);
        // With NO modules at all the engine row is still reachable.
        stats.module_entries.clear();
        w.handle_key(down(), &mut stats);
        assert_eq!(w.selected, 0);
        assert_eq!(w.selected, ENGINE_ROW);
    }

    #[test]
    fn clamp_selected_keeps_a_selection_inside_the_list() {
        assert_eq!(clamp_selected(0, 4), 0);
        assert_eq!(clamp_selected(3, 4), 3);
        assert_eq!(clamp_selected(9, 4), 3);
        assert_eq!(clamp_selected(0, 1), 0);
        assert_eq!(clamp_selected(5, 1), 0);
    }

    #[test]
    fn the_scroll_keeps_the_selected_row_visible() {
        // 4 rows (engine + 3 modules) in a 2-row viewport.
        let total = 4;
        let visible = 2;
        // Engine selected, not scrolled: already visible.
        assert_eq!(scroll_for(0, 0, visible, total), 0);
        // Last row selected: scroll just enough to put it on the last line.
        assert_eq!(scroll_for(3, 0, visible, total), 2);
        // And back up to the engine: the view returns to the top, so the engine
        // row is not left stranded above a scrolled list.
        assert_eq!(scroll_for(0, 2, visible, total), 0);
        // A selection in the middle of the viewport does not move the view.
        assert_eq!(scroll_for(1, 0, visible, total), 0);
        // A stale scroll past the end is pulled back first.
        assert_eq!(scroll_for(0, 99, visible, total), 0);
    }

    #[test]
    fn the_engine_row_comes_back_into_view_when_it_is_selected_again() {
        // The engine is row 0, so scrolled down it is legitimately off screen.
        // The property that matters is the round trip: drive the real window to
        // the bottom, then walk the selection back to the engine and assert the
        // list scrolled home and the engine row is actually drawn again. A
        // keep-visible that forgot the index shift would leave it stranded.
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(8);
        let (width, height) = (60u16, 12u16);

        for _ in 0..EntryRow::total(8) {
            w.handle_key(down(), &mut stats);
        }
        let bottom = super::tests::render(&mut w, &stats, width, height);
        assert!(!bottom.contains("cockatiel"), "expected the list to be scrolled away from row 0:\n{}", bottom);

        while w.selected > ENGINE_ROW {
            w.handle_key(up(), &mut stats);
        }
        let screen = super::tests::render(&mut w, &stats, width, height);
        assert!(
            screen.contains("cockatiel"),
            "the engine row must be visible whenever it is the selected row:\n{}",
            screen
        );
        // ...and the first module directly below it, so the list scrolled all
        // the way home rather than leaving a gap.
        assert!(screen.contains("m0"), "the list did not scroll home:\n{}", screen);
    }

    #[test]
    fn the_selected_row_is_highlighted_wherever_the_list_is_scrolled() {
        // The cyan background is the selection, independent of what the row is
        // called: assert there is exactly one highlighted row on screen and it
        // is the selected one.
        let mut w = super::ModulesWindow::new();
        let stats = stats_with_modules(8);
        let (width, height) = (60u16, 12u16);
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        for selected in 0..EntryRow::total(8) {
            w.selected = selected;
            let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
            let colors = crate::colors::load_colors(&std::path::PathBuf::from(""));
            w.render(
                Rect::new(0, 0, width, height),
                &mut buf,
                true,
                &stats,
                &colors,
                &default_hotkeys(),
                &[],
            );
            let highlighted: Vec<String> = (0..height)
                .map(|y| -> String {
                    (0..width)
                        .filter(|x| buf[(*x, y)].bg == ratatui::style::Color::Cyan)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect()
                })
                .filter(|line: &String| !line.is_empty())
                .collect();
            assert_eq!(
                highlighted.len(),
                1,
                "expected exactly one highlighted row at selection {}: {:?}",
                selected,
                highlighted
            );
            let row: String = highlighted[0]
                .chars()
                .filter(|c| *c != ' ')
                .collect::<String>();
            let expected = match EntryRow::at(selected, 8) {
                Some(EntryRow::Engine) => "cockatiel".to_string(),
                Some(EntryRow::Module(i)) => format!("m{}", i),
                None => panic!("no row at {}", selected),
            };
            assert!(
                row.contains(&expected),
                "selection {} highlighted {:?}, expected it to contain {:?}",
                selected,
                row.trim(),
                expected
            );
        }
    }

    // ── after the operator removes the engine ─────────────────────────────

    /// What the engine row shows once the TUI has forgotten its engine, and what
    /// it stops offering.
    ///
    /// The row STAYS. Two reasons, and the second is the load-bearing one: it is
    /// the record of "this TUI has no engine" (a blank list would leave the
    /// operator wondering whether the TUI is broken), and it is where they come
    /// back to — `E` on it opens the engine's `config.json`, which is how
    /// `shutdown_on_request` gets read and flipped. What must NOT survive is a
    /// connection that no longer exists, or keys whose press can only be
    /// refused.
    #[test]
    fn a_removed_engine_is_still_a_row_but_an_inert_one() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(2);
        stats.forget_engine();

        let screen = super::tests::render_with(&mut w, &stats, 220, 20, &keymap());
        // The row is there, and it says the one true thing.
        let row = screen.lines().find(|l| l.contains("cockatiel")).expect("the engine row");
        assert!(row.contains(ENGINE_REMOVED_STATUS), "row: {:?}", row);
        assert!(!row.contains("connected"), "a removed engine has no connection: {:?}", row);
        assert!(!screen.contains("PAUSED"), "a removed engine has no gate to hold:\n{}", screen);
        // The engine-sourced statistics are gone rather than frozen.
        assert!(screen.contains("(no platforms connected)"), "stale platform counts:\n{}", screen);
        // ...and the bar offers nothing that goes nowhere.
        assert_eq!(
            bar_labels(&screen),
            vec!["nav", "edit", "select", "users", "pause"],
            "the removed engine's bar must not offer restart/detach:\n{}",
            screen
        );
        for gone in ["restart:[R]", "detach:[X]"] {
            assert!(!screen.contains(gone), "{} must not be advertised:\n{}", gone, screen);
        }
        // `edit` is still there — it is the way back.
        assert!(screen.contains("edit:[E]"), "the config editor is still the way back:\n{}", screen);
    }

    /// The removal does not move the row arithmetic. That is the reason the row
    /// is kept rather than hidden: making the engine's PRESENCE conditional
    /// would put "is there an engine" into `EntryRow::total`/`EntryRow::at` and
    /// into every clamp, and a selection stored in the old index space would
    /// silently start naming a MODULE. With the row kept, `selected` means
    /// exactly what it meant before the removal.
    #[test]
    fn removing_the_engine_does_not_move_the_row_arithmetic() {
        let before = stats_with_modules(3);
        let mut after = before.clone();
        after.forget_engine();
        // `forget_engine` clears the module list, so the list is just the engine
        // row — and the engine is still row 0 of it, not row 1, and not gone.
        assert_eq!(EntryRow::total(before.module_entries.len()), 4);
        assert_eq!(EntryRow::total(after.module_entries.len()), 1);
        assert_eq!(EntryRow::at(0, after.module_entries.len()), Some(EntryRow::Engine));
        assert_eq!(EntryRow::at(0, before.module_entries.len()), Some(EntryRow::Engine));
        assert_eq!(EntryRow::at(1, after.module_entries.len()), None);
        assert_eq!(
            clamp_selected(3, EntryRow::total(after.module_entries.len())),
            0,
            "a stale selection lands on the engine, not on a module"
        );
        assert_eq!(scroll_for(0, 3, 2, EntryRow::total(after.module_entries.len())), 0);

        // And the window agrees: the engine row is still selected, still not a
        // module, and still the engine — so the per-module actions stay refused
        // and the engine-only ones are refused by the ACTION, not by the row
        // vanishing under it.
        let mut w = super::ModulesWindow::new();
        w.selected = 3;
        assert!(!w.selection_is_module(&after));
        assert!(w.selection_is_engine(&after));
        assert_eq!(w.selected_module_name(&after), None);
        assert_eq!(w.config_editor_target(&after).map(|(_, label, _)| label), Some(ENGINE_ROW_LABEL.to_string()));
    }

    /// The engine-only keys must be refused from a MODULE row, and the window
    /// has to say which row is the engine row for the app to be able to tell.
    #[test]
    fn the_engine_row_is_recognisable_by_the_app() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(2);
        assert!(w.selection_is_engine(&stats), "row 0 is the engine");
        assert!(!w.selection_is_module(&stats), "…and it is not a module");
        w.handle_key(down(), &mut stats);
        assert!(!w.selection_is_engine(&stats), "row 1 is a module");
        assert!(w.selection_is_module(&stats));
        // A window with no notion of the engine row must not claim it, or the
        // app's "act on the first known module" fallback would be one more way
        // to restart an engine nobody selected.
        let log = crate::windows::LogWindow::new();
        assert!(!log.selection_is_engine(&stats));
    }

    /// The engine row is drawn distinctly from a module row.
    #[test]
    fn the_engine_row_is_drawn_distinctly_from_a_module_row() {
        let mut w = super::ModulesWindow::new();
        let stats = stats_with_modules(2);
        let screen = super::tests::render(&mut w, &stats, 60, 24);
        let engine = screen
            .lines()
            .find(|l| l.contains("cockatiel"))
            .expect("the engine row");
        // Its own status text, not a module name...
        assert!(engine.contains("connected"), "engine row: {:?}", engine);
        // ...a marker, because it is a different KIND of row...
        assert!(engine.contains('\u{25c6}'), "engine row: {:?}", engine);
        // ...and no stage tag, because it is in no pipeline stage.
        assert!(!engine.contains('['), "the engine is not in a stage: {:?}", engine);
        // The [ENGINE] section header is gone: the engine is a row now, and a
        // header above row 0 read as a title for the module list under it.
        assert!(!screen.contains("[ENGINE]"), "stale section header:\n{}", screen);
        // And the modules are still there, still indented one level deeper.
        assert!(screen.contains("m0"), "module rows missing:\n{}", screen);
        assert!(screen.contains("m1"), "module rows missing:\n{}", screen);
    }

    // ── the action rule for the engine row ───────────────────────────────

    /// Per-module actions are REFUSED on the engine row, loudly, rather than
    /// falling through to a neighbouring module. The app side of this is
    /// `is_module_scoped` + `focused_selection_is_module` in main.rs; what the
    /// window owes them is a selection that says "not a module".
    #[test]
    fn a_module_action_has_no_target_on_the_engine_row() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(3);
        w.handle_key(down(), &mut stats);
        assert!(w.selection_is_module(&stats));
        let first = w.selected_module_name(&stats);
        // The fallback the app would otherwise use: the FIRST known module.
        assert_eq!(first.as_deref(), Some("m0"));

        w.handle_key(up(), &mut stats);
        assert!(!w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats), None);
    }

    /// The key map the running app actually uses (defaults + the repo's
    /// `hotkey_config.json`), so the bar under test is the bar an operator sees
    /// — the file is what binds `Enter` to SelectModule.
    fn keymap() -> crate::hotkeys::HotkeyConfig {
        crate::hotkeys::load_hotkeys(
            &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hotkey_config.json"),
        )
    }

    /// The `<command>:[<keys>]` labels a rendered hint bar actually shows.
    /// Asserting on the LABEL SET (rather than on a string built from the same
    /// helper the renderer calls) is what makes this a real check: the
    /// renderer could print the whole map and the test would still pass if it
    /// compared strings built the same way.
    fn bar_labels(screen: &str) -> Vec<String> {
        let line = screen.lines().find(|l| l.contains(":[")).unwrap_or("");
        line.split_whitespace()
            .filter_map(|tok| {
                let (label, keys) = tok.split_once(":[")?;
                // On a one-line layout the bar is drawn over the bottom border,
                // so the first token arrives glued to a box-drawing character.
                let label: String = label.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                (!label.is_empty() && !keys.is_empty()).then_some(label)
            })
            .collect()
    }

    #[test]
    fn the_hint_bar_offers_only_engine_actions_on_the_engine_row() {
        let mut w = super::ModulesWindow::new();
        let stats = stats_with_modules(3);
        let bar = super::tests::render_with(&mut w, &stats, 220, 20, &keymap());
        // What the engine can actually do: edit its own config, START/STOP it
        // (the same keys modules use), restart it, remove it from the TUI,
        // plus the window-level keys and the global pause toggle.
        assert_eq!(
            bar_labels(&bar),
            vec![
                "nav", "edit", "start", "stop", "restart", "detach", "select", "users", "pause"
            ],
            "engine bar:\n{}",
            bar
        );
        // Spelled out, so a regression names the key that leaked back in.
        for gone in ["del:[d]", "auto:[a]", "copy:[c]", "clear:[b]", "test:[t]"] {
            assert!(
                !bar.contains(gone),
                "the engine row must not advertise {:?}:\n{}",
                gone,
                bar
            );
        }
        assert!(!bar.contains("creds:"), "the engine has no credentials form:\n{}", bar);
        // The engine-specific keys, named so the failure is legible.
        assert!(bar.contains("start:[s]"), "engine bar:\n{}", bar);
        assert!(bar.contains("stop:[x]"), "engine bar:\n{}", bar);
        assert!(bar.contains("restart:[R]"), "engine bar:\n{}", bar);
        assert!(bar.contains("detach:[X]"), "engine bar:\n{}", bar);
    }

    #[test]
    fn the_hint_bar_offers_the_module_actions_on_a_module_row() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(3);
        w.handle_key(down(), &mut stats);
        let bar = super::tests::render_with(&mut w, &stats, 220, 20, &keymap());
        assert_eq!(
            bar_labels(&bar),
            vec![
                "nav", "start", "stop", "del", "auto", "copy", "creds", "edit", "clear", "test",
                "select", "users", "pause"
            ],
            "module bar:\n{}",
            bar
        );
    }

    /// The pause key is a GLOBAL (nav) binding, matched before any window's own
    /// key handling, so it must keep working from every window and must not be
    /// shadowed by the modules window now that the engine row is selectable.
    #[test]
    fn the_pause_key_is_not_shadowed_by_the_modules_window() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(2);
        // The window must not consume `p` itself...
        assert_eq!(w.handle_key(key('p'), &mut stats), None);
        // ...and the global lookup still resolves it to the toggle.
        let mut state = crate::app::AppState::new(
            crate::colors::load_colors(&std::path::PathBuf::from("")),
            default_hotkeys(),
        );
        state.windows = vec![Box::new(w)];
        state.active_window = crate::app::WindowId::Modules;
        let p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::empty());
        assert_eq!(
            state.handle_global_key(p),
            Some(crate::hotkeys::Action::TogglePipelinePause)
        );
    }

    // ── E: the engine's own config ───────────────────────────────────────

    #[test]
    fn the_edit_key_targets_the_engines_own_directory() {
        let mut w = super::ModulesWindow::new();
        let stats = stats_with_modules(2);
        // Engine row selected -> the engine's files. No plugin list contains
        // the engine, so this is the only way its config is reachable.
        let (target, label, dir) = w.config_editor_target(&stats).expect("engine target");
        assert_eq!(target, ConfigTarget::Engine);
        assert_eq!(label, "engine");
        assert_eq!(dir, crate::supervisor::engine_dir());
        assert!(dir.join("config.json").is_file() || dir.join(".env").is_file(),
            "the engine dir must be where its config lives: {}", dir.display());

        // A module row defers to the plugin manifest, which is where a module's
        // directory is known — the window must not guess it.
        let mut stats2 = stats.clone();
        w.selected = 1;
        assert_eq!(w.config_editor_target(&stats2), None);
        // ...and it is the engine row that is the special one, not "no modules".
        stats2.module_entries.clear();
        w.selected = 0;
        assert!(w.config_editor_target(&stats2).is_some());
    }

    #[test]
    fn the_engine_editor_masks_the_pin_and_saving_it_demands_a_restart() {
        // The engine's PIN is a secret in `.env`, NOT in config.json (the
        // engine's `ConfigState` documents this, and `ensure_secrets` writes
        // it there). So "edit the pin" is an `.env` edit, and it has to come
        // back out of the editor masked.
        let tmp = std::env::temp_dir().join(format!("cockatiel-engine-cfg-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join(".env"),
            "COCKATIEL_PIN=123456\nCOCKATIEL_JWT_SECRET=s3cret\nCOCKATIEL_BIND_IP=127.0.0.1\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("config.json"),
            r#"{"port":9734,"start_paused":true,"inputs":[],"preprocessModules":[{"name":"a","priority":100}]}"#,
        )
        .unwrap();

        let mut w = super::ModulesWindow::new();
        w.start_config_editor(ConfigTarget::Engine, "engine", tmp.clone());
        assert!(w.in_editor());
        let rows = &w.editing.as_ref().unwrap().rows;

        // The PIN is a `.env` row, and every `.env` row is a secret.
        let pin = rows
            .iter()
            .find(|r| r.path.first() == Some(&super::Seg::Key("COCKATIEL_PIN".into())))
            .expect("the PIN must be an editable row");
        assert_eq!(pin.source, "env", "the PIN lives in .env, not config.json");
        assert!(pin.is_secret, "the PIN must be censored on screen");
        // The port is a config.json setting and is NOT a secret.
        let port = rows
            .iter()
            .find(|r| r.path.last() == Some(&super::Seg::Key("port".into())))
            .expect("port row");
        assert_eq!(port.source, "json");
        assert!(!port.is_secret, "a setting is not a secret");

        // On screen: the key is named, the value is not.
        let screen = super::tests::render(&mut w, &GlobalStats::default(), 120, 60);
        assert!(screen.contains("COCKATIEL_PIN"), "PIN key missing:\n{}", screen);
        assert!(screen.contains("*****"), "PIN not masked:\n{}", screen);
        assert!(!screen.contains("123456"), "the PIN leaked on screen:\n{}", screen);
        assert!(!screen.contains("s3cret"), "the JWT secret leaked on screen:\n{}", screen);
        assert!(screen.contains("port : 9734"), "a setting should stay visible:\n{}", screen);

        // Now actually change the PIN through the editor and save.
        let hk = default_hotkeys();
        while w.editing.as_ref().unwrap().selected
            < w.editing
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .position(|r| r.path.first() == Some(&super::Seg::Key("COCKATIEL_PIN".into())))
                .unwrap()
        {
            w.editor_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()), &hk);
        }
        for _ in 0.."123456".len() {
            w.editor_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()), &hk);
        }
        for c in "9999".chars() {
            w.editor_key(key(c), &hk);
        }
        w.editor_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &hk);
        w.editor_key(key('y'), &hk);
        assert!(!w.in_editor());

        // Written back, masked or not.
        let env = std::fs::read_to_string(tmp.join(".env")).unwrap();
        assert!(env.contains("COCKATIEL_PIN=9999"), ".env was not written: {}", env);
        // An untouched secret round-trips rather than being blanked.
        assert!(env.contains("COCKATIEL_JWT_SECRET=s3cret"), ".env lost a secret: {}", env);

        // ...and the app is told exactly which change needs a restart.
        let notes = w.take_saved_engine().expect("the engine save must report itself");
        assert_eq!(
            notes.iter().map(|n| n.key.as_str()).collect::<Vec<_>>(),
            vec!["COCKATIEL_PIN"],
            "only the PIN changed, and it is boot-bound: {:?}",
            notes
        );
        assert_eq!(notes[0].why, super::engine_key_why("COCKATIEL_PIN"));
        // A module save is a different restart unit and must not report here.
        assert_eq!(w.take_saved_module(), None);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── what needs a restart ─────────────────────────────────────────────

    #[test]
    fn the_pipeline_ordering_lists_hot_reload() {
        // The engine's config poll task (3s) pushes the pre/in/post chains into
        // the orchestrator, and `broadcast_stage` re-reads `inputs` per
        // broadcast: none of these need a restart.
        for key in ["inputs", "preprocessModules", "inprocessModules", "postprocessModules"] {
            assert_eq!(super::engine_key_reload(key), Reload::HotReload, "{}", key);
            assert!(!super::engine_key_why(key).is_empty(), "{} has no reason", key);
        }
    }

    #[test]
    fn boot_bound_settings_need_a_restart() {
        // The listener is bound once; the boot gate is read once; the secrets
        // are resolved into memory once.
        for key in ["port", "start_paused", "COCKATIEL_PIN", "COCKATIEL_JWT_SECRET"] {
            assert_eq!(super::engine_key_reload(key), Reload::NeedsRestart, "{}", key);
            assert!(!super::engine_key_why(key).is_empty(), "{} has no reason", key);
        }
    }

    #[test]
    fn an_unrecognised_key_is_deliberately_treated_as_needing_a_restart() {
        // "Not in the table" means nobody proved the running engine re-reads
        // it, not that it does. The verdict is restart because that is the safe
        // direction to be wrong in, and the reason says so rather than
        // pretending to know.
        for key in [
            "timeline_database_location",
            "max_connections",
            "handshake_timeout_secs",
            "module_probe_interval_secs",
            "module_approval_policy",
            "recovery_grace_secs",
            "something_added_next_release",
            "",
        ] {
            assert_eq!(
                super::engine_key_reload(key),
                Reload::NeedsRestart,
                "{} should default to a restart",
                key
            );
            assert_eq!(super::engine_key_why(key), super::UNKNOWN_KEY_WHY);
        }
        // ...and it is a real default, not an accident of an empty table.
        assert!(!ENGINE_CONFIG_KEYS.is_empty());
    }

    #[test]
    fn the_table_covers_every_key_it_makes_a_claim_about() {
        // Guards the "per entry, with a comment" promise: every classified key
        // carries a reason, and the hot set is EXACTLY the keys someone has
        // actually proven the running engine re-reads — the four ordering lists
        // plus `shutdown_on_request`, which the shutdown branch reads per
        // request. Anything unproven falls to NeedsRestart, so this list is the
        // whole of the "no restart needed" claim and must not grow casually.
        for entry in ENGINE_CONFIG_KEYS {
            assert!(!entry.why.trim().is_empty(), "{} has no reason", entry.key);
        }
        let hot: Vec<&str> = ENGINE_CONFIG_KEYS
            .iter()
            .filter(|k| k.reload == Reload::HotReload)
            .map(|k| k.key)
            .collect();
        assert_eq!(
            hot,
            vec![
                "inputs",
                "preprocessModules",
                "inprocessModules",
                "postprocessModules",
                "shutdown_on_request",
            ]
        );
    }

    #[test]
    fn allowing_shutdown_does_not_ask_for_the_restart_it_exists_to_avoid() {
        // The flag's whole purpose is to let the operator stop the engine
        // without relaunching the stack. The engine re-reads it on every
        // request, so warning "restart the engine to apply" would tell them to
        // do the exact thing they are trying to avoid — and would be wrong.
        assert_eq!(
            super::engine_key_reload("shutdown_on_request"),
            Reload::HotReload
        );
        assert!(!super::engine_key_why("shutdown_on_request").is_empty());
    }

    #[test]
    fn only_changed_rows_become_restart_warnings() {
        // A save re-writes both files in full, so the warning has to be driven
        // by what CHANGED or it would fire on every save and mean nothing.
        let unchanged = vec![
            row("json", "port", "9734", "9734"),
            row("env", "COCKATIEL_PIN", "123456", "123456"),
        ];
        assert!(super::engine_restart_notes(&unchanged).is_empty());

        let changed = vec![
            row("json", "port", "9735", "9734"),
            row("json", "start_paused", "false", "true"),
            row("env", "COCKATIEL_JWT_SECRET", "new", "old"),
            // Hot-reload: changing it is not a restart reason.
            row("json", "preprocessModules", "b", "a"),
        ];
        let keys: Vec<String> = super::engine_restart_notes(&changed)
            .into_iter()
            .map(|n| n.key)
            .collect();
        assert_eq!(keys, vec!["port", "start_paused", "COCKATIEL_JWT_SECRET"]);
    }

    #[test]
    fn a_nested_row_is_classified_by_its_container() {
        // `inputs[0].name` is a change to `inputs`, which hot-reloads; the leaf
        // "name" is not a config key at all.
        let rows = vec![super::EditorRow {
            source: "json".to_string(),
            path: vec![
                super::Seg::Key("inputs".into()),
                super::Seg::Idx(0),
                super::Seg::Key("name".into()),
            ],
            value: "b".into(),
            cursor: 0,
            is_secret: false,
            kind: super::RowKind::Scalar,
            add_base: String::new(),
            original: "a".into(),
        }];
        assert_eq!(super::edited_config_key(&rows[0]), Some("inputs"));
        assert!(super::engine_restart_notes(&rows).is_empty());

        // The same shape under a boot-bound container is a restart.
        let mut boot_bound = rows;
        boot_bound[0].path = vec![super::Seg::Key("port".into()), super::Seg::Key("x".into())];
        assert_eq!(super::edited_config_key(&boot_bound[0]), Some("port"));
    }

    #[test]
    fn a_key_with_several_rows_is_reported_once() {
        // An ordering list has one row per element; it is one setting and gets
        // one warning.
        let rows = vec![
            row("json", "preprocessModules", "b", "a"),
            row("json", "preprocessModules", "c", "a"),
            row("json", "port", "9735", "9734"),
        ];
        let notes = super::engine_restart_notes(&rows);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key, "port");
    }

    /// A scalar editor row with a before/after value.
    fn row(source: &str, key: &str, value: &str, original: &str) -> super::EditorRow {
        super::EditorRow {
            source: source.to_string(),
            path: vec![super::Seg::Key(key.to_string())],
            value: value.to_string(),
            cursor: 0,
            is_secret: source == "env",
            kind: super::RowKind::Scalar,
            add_base: String::new(),
            original: original.to_string(),
        }
    }
}
