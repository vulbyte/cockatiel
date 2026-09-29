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

/// Row index of the ENGINE row in the grouped list. Row 0 is the `[ENGINE]`
/// group header; the engine row itself sits directly under it. Kept as a named
/// constant because the default selection lands here — the operator opens the
/// window onto the engine, not onto the header that merely names it.
pub const ENGINE_ROW: usize = 1;

/// The pipeline stage a group of rows belongs to, in pipeline order: input
/// (adapters) is the earliest, pre-process and in-process are the two middle
/// stages, post-process is the latest, and the engine is a thing apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Engine,
    Adapters,
    PreProcess,
    InProcess,
    PostProcess,
}

/// What one row in the grouped list IS: a group header, the engine, or a
/// module. Split from [`GroupedRow`] so the row's SHAPE is matchable on its
/// own — a header is selectable but names no module, and the engine is the one
/// row the engine-only actions may fire from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A group's `[NAME]` header. DISPLAY-ONLY: it highlights when selected but
    /// navigation never lands on it (see [`next_selectable`]), and it names
    /// neither a module nor the engine — so the hint bar narrows to the
    /// window-level keys and module-scoped actions are refused.
    Header,
    /// The engine row. Selectable, not a module, and the one row the two
    /// engine-only actions (restart / remove) can fire from.
    Engine,
    /// A module row, by index into `GlobalStats::module_entries`.
    Module(usize),
}

/// One selectable row in the modules window's grouped list.
///
/// The list is a VIEW over `GlobalStats::module_entries`, grouped by pipeline
/// stage: the `[ENGINE]` header + engine row, then one header per non-empty
/// group followed by its modules, in stage order. `self.selected` indexes THIS
/// space, and `EntryKind::Module(_)` is the only kind that can yield a module
/// name, so a module-scoped action cannot silently fire on a header or the
/// engine row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupedRow {
    pub group: Group,
    pub kind: EntryKind,
}

/// The pipeline stage a module's reported `position` belongs to.
///
/// Mirrors the supervisor's `config_list_key`: `input` is the adapter group,
/// `preprocess`/`inprocess`/`postprocess` are the three pipeline stages,
/// `output` is treated as post-process (it is the engine's name for the stage
/// the message is in when it is handed to a post-processor), and anything
/// unknown falls into pre-process — the same default the engine's ordering
/// uses for a capability it does not recognise.
pub fn group_for_position(position: &str) -> Group {
    match position {
        "input" => Group::Adapters,
        "preprocess" => Group::PreProcess,
        "inprocess" => Group::InProcess,
        "postprocess" | "output" => Group::PostProcess,
        _ => Group::PreProcess,
    }
}

/// The non-engine groups, in pipeline order.
const PIPELINE_GROUPS: [Group; 4] = [
    Group::Adapters,
    Group::PreProcess,
    Group::InProcess,
    Group::PostProcess,
];

impl GroupedRow {
    /// The index into `module_entries`, or `None` for a header or the engine.
    pub fn module_index(self) -> Option<usize> {
        match self.kind {
            EntryKind::Module(i) => Some(i),
            _ => None,
        }
    }

    /// The module name this row names, or `None` for a header or the engine.
    ///
    /// `None` is the point: a header names a stage and the engine is not a
    /// module, so a caller that needs a module has nothing to do here.
    /// Handing back a neighbouring row's name instead is how "stop" ends up
    /// killing a module nobody selected.
    pub fn module_name(self, stats: &GlobalStats) -> Option<String> {
        self.module_index()
            .and_then(|i| stats.module_entries.get(i))
            .map(|m| m.name.clone())
    }

    /// Whether this row is the engine row (not a header, and not a module).
    pub fn is_engine(self) -> bool {
        matches!(self.kind, EntryKind::Engine)
    }

    /// Whether this row is a group header.
    pub fn is_header(self) -> bool {
        matches!(self.kind, EntryKind::Header)
    }
}

/// The ordered selectable rows for the current stats: the `[ENGINE]` header
/// and the engine row, then — in pipeline order — every group's header (even
/// when the group is empty) followed by its modules.
///
/// Every group header is ALWAYS present, empty or not, so the operator sees
/// the full pipeline shape at a glance and knows a stage exists even when no
/// module sits in it yet. The engine header and row are unconditional, so the
/// list is never empty — there is always the engine to sit on and look at.
pub fn grouped_rows(stats: &GlobalStats) -> Vec<GroupedRow> {
    let mut rows = Vec::new();
    rows.push(GroupedRow {
        group: Group::Engine,
        kind: EntryKind::Header,
    });
    rows.push(GroupedRow {
        group: Group::Engine,
        kind: EntryKind::Engine,
    });
    for group in PIPELINE_GROUPS {
        let members: Vec<usize> = stats
            .module_entries
            .iter()
            .enumerate()
            .filter(|(_, m)| group_for_position(&m.position) == group)
            .map(|(i, _)| i)
            .collect();
        rows.push(GroupedRow {
            group,
            kind: EntryKind::Header,
        });
        for mi in members {
            rows.push(GroupedRow {
                group,
                kind: EntryKind::Module(mi),
            });
        }
    }
    rows
}

/// One display line of the grouped list: a real row, or the blank separator
/// line that separates groups.
///
/// Blank separators are SPACING, not rows: selection never lands on them and
/// they are absent from the row model entirely. They exist only here, in the
/// display model, because they still consume a screen line — and it is the
/// screen lines the scroll arithmetic must count or a row near a group
/// boundary silently falls off the bottom of the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupedLine {
    Blank,
    Row { index: usize },
}

/// The full ordered list of display lines for `rows`: every row, plus a blank
/// separator before each group header that is not the first line of the list.
///
/// Rows and lines are deliberately NOT 1:1 — the same model the config editor
/// uses for its tree, where a section header costs a line that is not an
/// editable row. `self.scroll` and the render loop live in this space;
/// `self.selected` stays in row space and `grouped_selected_line` bridges the
/// two.
fn grouped_lines(rows: &[GroupedRow]) -> Vec<GroupedLine> {
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if row.is_header() && i > 0 {
            out.push(GroupedLine::Blank);
        }
        out.push(GroupedLine::Row { index: i });
    }
    out
}

/// The display-line index of the selected row, or 0 when it is not in the list.
fn grouped_selected_line(lines: &[GroupedLine], selected: usize) -> usize {
    lines
        .iter()
        .position(|l| matches!(l, GroupedLine::Row { index } if *index == selected))
        .unwrap_or(0)
}

/// The display label for a group header. Every header sits at the SAME 2-space
/// base as the module rows beneath it — the mock's extra indent on the nested
/// pipeline stages was dropped once the status column aligned, so a header and
/// its modules read as one column, not two.
fn header_label(group: Group) -> String {
    let name = match group {
        Group::Engine => "ENGINE",
        Group::Adapters => "ADAPTERS",
        Group::PreProcess => "PRE-PROCESS",
        Group::InProcess => "IN-PROCESS",
        Group::PostProcess => "POST-PROCESS",
    };
    format!("  [{name}]")
}

/// Format a rolling average (ms) for the right-aligned ms column.
///
/// Sub-millisecond values read as `<1ms` (a blank tick means "under a
/// millisecond", which is the resolution the engine reports at); everything
/// else is one decimal with a unit, e.g. `15.7ms`. `None` (a module that has
/// not completed a message yet) renders as a blank.
fn format_ms(avg_ms: Option<f64>) -> String {
    match avg_ms {
        None => String::new(),
        Some(v) if v < 1.0 => "<1ms".to_string(),
        Some(v) => format!("{v:.1}ms"),
    }
}

/// How many messages/minute the engine could sustain at `total_ms` average
/// end-to-end latency before the queue starts filling (the inverse of the
/// per-message latency: `1000/ms` messages/sec, × 60 = `60000/ms`/min). Blank
/// when there's no latency data yet.
fn format_throughput(total_ms: f64) -> String {
    if total_ms <= 0.0 {
        return String::new();
    }
    let per_min = 60_000.0 / total_ms;
    format!("{:.0}/min", per_min)
}

/// The colour for a processing time, so the ms column doubles as a heat gauge.
/// A simple if/else tree: the higher the latency, the more alarming the
/// colour. The thresholds are deliberately coarse — they mark the round-trip
/// cost bands the operator cares about, not a subtle gradient.
fn ms_color(avg_ms: Option<f64>) -> Color {
    match avg_ms {
        None => Color::DarkGray,
        Some(v) if v < 6.0 => Color::Blue,
        Some(v) if v < 15.0 => Color::Green,
        Some(v) if v < 100.0 => Color::Yellow,
        Some(_) => Color::Red,
    }
}

/// The sum of the rolling averages of the modules in `group` — what the group
/// header reports as its "category average MS" and what the engine row reports
/// as the total pipeline time. A module with no timing yet contributes 0.
fn group_total_ms(stats: &GlobalStats, group: Group) -> f64 {
    stats
        .module_entries
        .iter()
        .filter(|m| group_for_position(&m.position) == group)
        .map(|m| m.avg_ms.unwrap_or(0.0))
        .sum()
}

/// The direction a Shift+arrow moves a module through the pipeline stages.
///
/// Named for the pipeline, not the key: "earlier" (toward pre-process) is what
/// Shift+up asks for and "later" (toward post-process) is what Shift+down asks
/// for, but the function is really about stage order, so it is the order that
/// is named. Carried inside `Action::MoveModuleStage` as the ONLY thing the
/// window can know about the move; where the direction lands is decided by the
/// supervisor, which owns the engine's chain order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageDirection {
    /// Shift+up: toward an earlier stage (post → in → pre).
    Earlier,
    /// Shift+down: toward a later stage (pre → in → post).
    Later,
}

/// The fixed width of the name column in the list, so every module's status
/// starts on the same x. Wide enough for the longest real module name and for
/// `cockatiel-engine`; a name that overflows it simply pushes its own status
/// right rather than being truncated.
const STATUS_COL: usize = 22;

/// The fixed width of the status column (e.g. `connected`, `offline`,
/// `waiting for prompt`). Padded so the autostart marker and the ms column
/// start on the same x on every row.
const STATUS_TEXT_COL: usize = 12;

/// The fixed width of the autostart column. A module with autostart shows
/// `A`; one without shows blank. The width keeps the ms column fixed either
/// way.
const AUTOSTART_COL: usize = 2;

/// The fixed width of the ms column, right-aligned within it so `7.3ms` and
/// `157.3ms` share a right edge.
const MS_COL: usize = 9;

/// The fixed width of the engine row's throughput column ("messages/minute"
/// the engine could sustain without the queue filling). Engine-only: modules
/// don't have a throughput number.
const THROUGHPUT_COL: usize = 10;

/// Clamp a stored selection into a list of `total` rows.
pub fn clamp_selected(selected: usize, total: usize) -> usize {
    if total == 0 {
        0
    } else {
        selected.min(total - 1)
    }
}

/// The next SELECTABLE row index from `selected`, walking `dir` (1 = down,
/// -1 = up) through `rows` while skipping group headers.
///
/// Headers are display-only: navigation must never LAND on one, so the walk
/// stops at the first non-header. When the walk runs off the end of the list
/// (the trailing `[IN-PROCESS]`/`[POST-PROCESS]` headers are always present,
/// empty or not, so the boundary row IS a header), it settles on the last
/// selectable row in the walked direction rather than on the header. Clamped at
/// both ends — a press at the top or bottom holds the current row instead of
/// wrapping or landing on a header.
pub fn next_selectable(selected: usize, total: usize, rows: &[GroupedRow], dir: i32) -> usize {
    if total == 0 {
        return 0;
    }
    // Walk in the direction, skipping headers...
    let mut i = selected as i64 + dir as i64;
    while i >= 0 && i < total as i64 {
        if !rows[i as usize].is_header() {
            return i as usize;
        }
        i += dir as i64;
    }
    // ...and off the end: the boundary is a header, so fall back to the nearest
    // selectable row in the walked direction (the last module, or the engine).
    let mut j = if dir > 0 { total as i64 - 1 } else { 0 };
    while j >= 0 && j < total as i64 {
        if !rows[j as usize].is_header() {
            return j as usize;
        }
        j -= dir as i64;
    }
    // No selectable row at all — cannot happen while the engine row exists.
    selected
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
fn hint_labels(row: GroupedRow, engine_removed: bool) -> &'static [&'static str] {
    match row.kind {
        // A group header names a stage, not a module and not the engine, so
        // the only keys that mean anything here are the window-level ones
        // (`select`/`popout`/`users`). The pause toggle is global and is
        // appended by the caller. Offering start/stop/del on a header would
        // promise an action the press refuses (see `is_module_scoped`).
        EntryKind::Header => &["select", "popout", "users"],
        // The engine: open its own config, start/stop it, and — while there IS
        // one — restart or remove it. `select`/`popout`/`users` act on the
        // WINDOW, so they mean the same thing on every row. `start`/`stop` are
        // the SAME keys the modules use (s/x): on the engine row the dispatcher
        // turns them into launch/kill of the engine process rather than of a
        // module. The rest — del/auto/copy/creds/clear/test — is module-only
        // and is refused on the engine row (see `is_module_scoped`), so
        // advertising it would promise an action the key press refuses.
        EntryKind::Engine if engine_removed => &["edit", "select", "popout", "users"],
        EntryKind::Engine => &[
            "edit", "start", "stop", "restart", "detach", "select", "popout", "users",
        ],
        EntryKind::Module(_) => &[
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

    /// First VISIBLE display line of the grouped list, in the same line space
    /// as the render loop (see [`GroupedLine`]).
    pub scroll: usize,

    /// The selected row of the grouped list: 0 is the `[ENGINE]` header,
    /// [`ENGINE_ROW`] is the engine, the rest are group headers and module
    /// rows. Always an index into that view, never into `module_entries`.
    /// Navigation keeps the selection on the engine row or a module row —
    /// headers are display-only (see [`next_selectable`]).
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
    UserDb,
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
            // Land on the ENGINE row, not the `[ENGINE]` header above it: the
            // header merely names the group, while the engine row is where the
            // engine's own actions (and its config editor) live.
            selected: ENGINE_ROW,
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
            EditorTarget::UserDb => {
                // The user-db re-reads its config.json on a short ticker, so a
                // save applies live without a restart.
                self.last_saved_engine = Some(vec![
                    crate::windows::modules::EngineRestartNote {
                        key: "user-db config".to_string(),
                        why: "the user-db re-reads config.json on its ticker",
                    },
                ]);
            }
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
    /// The row `self.selected` names, clamped into the current grouped view.
    ///
    /// Never fails: the engine row always exists (it is not conditional on any
    /// module), so a selection left pointing past the end of a shrunken module
    /// list resolves to the ENGINE rather than to some module the operator is
    /// not looking at. Every selection consumer goes through here, which is
    /// what keeps one index space for `selected` instead of two.
    fn selected_row(&self, stats: &GlobalStats) -> GroupedRow {
        grouped_rows(stats).get(self.selected).copied().unwrap_or(GroupedRow {
            group: Group::Engine,
            kind: EntryKind::Engine,
        })
    }

    /// The `Action` for a Shift+arrow stage move on the selected row: the
    /// module's name and the DIRECTION the operator asked for.
    ///
    /// The window answers only "which module" — the direction is all it can
    /// know, because the actual move (a jump into another stage, or a reorder
    /// within the in-process chain) depends on the engine's `config.json`
    /// ordering, which the window has no access to. The dispatch, which owns
    /// the supervisor, resolves the direction against the chain order and
    /// refuses the no-ops (input adapters, stage edges). A row that names no
    /// module — a header, the engine — is a no-op here, and the key is still
    /// consumed so it does not fall through to navigation or another binding.
    fn stage_action(&self, stats: &GlobalStats, direction: StageDirection) -> Option<Action> {
        let Some(name) = self.selected_row(stats).module_name(stats) else {
            return Some(Action::Noop);
        };
        Some(Action::MoveModuleStage(name, direction))
    }
}

impl Window for ModulesWindow {
    fn id(&self) -> WindowId {
        WindowId::Modules
    }

    fn selected_module_name(&self, stats: &GlobalStats) -> Option<String> {
        self.selected_row(stats).module_name(stats)
    }

    fn select_module_name(&mut self, name: &str, stats: &GlobalStats) {
        // The cursor follows a module that was moved (Shift+up/down): re-anchor
        // the selection onto its NEW row so the operator is not left staring at
        // whatever row the old index now names (another module, or a header).
        if let Some(idx) = grouped_rows(stats)
            .iter()
            .position(|r| r.module_name(stats).as_deref() == Some(name))
        {
            self.selected = idx;
            let rows = grouped_rows(stats);
            let lines = grouped_lines(&rows);
            self.scroll = grouped_selected_line(&lines, idx);
        }
    }

    fn selection_is_module(&self, stats: &GlobalStats) -> bool {
        // The engine row AND every group header are real selections that are
        // deliberately not a module, and the app's fallback (act on the first
        // known module) is exactly the wrong thing to do with them. Navigation
        // never lands on a header, but a stale selection after a shrunken list
        // can still resolve to one, so the refusal must hold there too.
        self.selected_row(stats).module_index().is_some()
    }

    fn selection_is_engine(&self, stats: &GlobalStats) -> bool {
        // Only the ENGINE row is the engine. The `[ENGINE]` header above it
        // names no module AND no engine, so the two engine-only actions stay
        // guarded on the row itself rather than on the group.
        // Stays `true` for a removed engine: the row is still selected, and
        // answering "no" there would hand the press to that same fallback.
        // Whether there is anything TO restart is a separate question, answered
        // by the action itself (`RestartOutcome::Removed`).
        self.selected_row(stats).is_engine()
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
        let rows = grouped_rows(stats);
        let total_rows = rows.len();
        let selected = clamp_selected(self.selected, total_rows);
        // Cannot be out of range: the clamp keeps `selected` inside the list
        // and the list always contains the engine header + engine row.
        let selected_row = rows[selected];
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

        // ── the list: grouped sections, ENGINE first then each stage ──
        //
        // One list, one selection, one scroll. The modules are grouped by
        // pipeline stage ([ENGINE], then input adapters, then the pre/in/post
        // stages), each group headed by a display-only `[NAME]` line. The engine
        // is row [`ENGINE_ROW`] under the `[ENGINE]` header — a real, selectable
        // row, not a status line.
        //
        // Scrolling runs over DISPLAY LINES (`grouped_lines`): each group's
        // header is preceded by a blank separator line that is NOT a row, and
        // those separators still consume screen rows, so a scroll that counted
        // only rows would let the selected row fall off the bottom of the
        // viewport at a group boundary. Selection stays in row space; the two
        // meet at `grouped_selected_line`.
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

        let lines = grouped_lines(&rows);
        let selected_line = grouped_selected_line(&lines, selected);
        let available_lines = (inner.y + inner.height).saturating_sub(y) as usize;
        let scroll = scroll_for(selected_line, self.scroll, available_lines, lines.len());

        // Modules that currently have an unanswered prompt waiting.
        let prompts_waiting: std::collections::HashSet<&str> =
            prompts.iter().map(|p| p.prompt.origin.as_str()).collect();

        let mut rendered_rows: HashMap<String, u16> = HashMap::new();
        for line in &lines[scroll..] {
            if y >= inner.y + inner.height {
                break;
            }
            match line {
                // A group separator: spacing, not a row. Consumes a screen line
                // and nothing else.
                GroupedLine::Blank => {
                    y += 1;
                }
                GroupedLine::Row { index } => {
                    let row = rows[*index];
                    let is_selected = *index == selected && is_active;
                    let (line, name) = match row.kind {
                        EntryKind::Header => {
                            let row_style = if is_selected {
                                Style::default().fg(Color::Black).bg(Color::Cyan)
                            } else {
                                Style::default()
                            };
                            // The group header's right column shows the ms sum
                            // for the stage (its category average), in the same
                            // fixed ms column as the rows below it, so the
                            // header and its modules line up. No label — the ms
                            // column is self-explanatory next to the row
                            // values.
                            //
                            // The `[ENGINE]` header shows NOTHING there: the
                            // engine's end-to-end total (and now the messages/
                            // minute throughput) live on the ENGINE ROW, the
                            // one place the engine actually displays its own
                            // numbers. Showing the total twice (header + row)
                            // is redundant.
                            let header_name = header_label(row.group);
                            let sum_ms = match row.group {
                                Group::Engine => 0.0,
                                _ => group_total_ms(stats, row.group),
                            };
                            // The engine header stays blank in the ms column
                            // (the engine row shows its own total + throughput).
                            let ms_text = if row.group == Group::Engine {
                                String::new()
                            } else {
                                format_ms(Some(sum_ms))
                            };
                            // Blank fill to the ms column, whose start includes
                            // the same 2-space row indent a module row has (the
                            // header label carries that indent too).
                            let ms_start = 2 + STATUS_COL + STATUS_TEXT_COL + AUTOSTART_COL;
                            let pad = ms_start.saturating_sub(header_name.len());
                            let header_style = if is_selected {
                                row_style
                            } else {
                                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                            };
                            (
                                Line::from(vec![
                                    Span::styled(
                                        format!("{header_name}{}", " ".repeat(pad)),
                                        header_style,
                                    ),
                                    Span::styled(
                                        format!("{:>width$}", ms_text, width = MS_COL),
                                        if is_selected {
                                            row_style
                                        } else {
                                            Style::default().fg(ms_color(Some(sum_ms)))
                                        },
                                    ),
                                ]),
                                None,
                            )
                        }
                        EntryKind::Engine => {
                            // The engine is a different KIND of thing from a
                            // module, but it is not named differently on
                            // screen: it is padded to the SAME name column as a
                            // module so its status lines up with every other
                            // status in the window. What separates it is the
                            // name itself (`cockatiel-engine`, no module has
                            // that), its own status colour, the PAUSED badge,
                            // and the engine-only actions the row can fire.
                            let row_style = if is_selected {
                                Style::default().fg(Color::Black).bg(Color::Cyan)
                            } else {
                                Style::default()
                            };
                            // The row is STILL the engine's row after a removal —
                            // it is the record of "this TUI has no engine", and
                            // it is where the operator comes back to (`E`) to
                            // read `shutdown_on_request` and relaunch. What it
                            // must not show is a connection that no longer
                            // exists, so the status becomes a flat "no engine"
                            // instead of the last known connection, and the
                            // PAUSED badge (a fact about a live gate) goes with
                            // it.
                            let status_text = if stats.engine_removed {
                                ENGINE_REMOVED_STATUS
                            } else {
                                stats.engine_status.as_str()
                            };
                            let mut spans = vec![
                                Span::styled(
                                    format!("  {:<width$}", "cockatiel-engine", width = STATUS_COL),
                                    row_style
                                        .fg(if is_selected { Color::Black } else { Color::White }),
                                ),
                                Span::styled(
                                    format!("{:<width$}", status_text, width = STATUS_TEXT_COL),
                                    row_style
                                        .fg(if is_selected { Color::Black } else { engine_status_color }),
                                ),
                            ];
                            // Flashing PAUSED, placed in the space where the
                            // autostart marker would sit on a module row (the
                            // engine has no autostart column of its own), so
                            // the ms column stays fixed. Uses the same
                            // warning-but-not-broken colour as the NEAR-LIMIT
                            // row below rather than a hard red: a held pipeline
                            // is the engine working as designed, not a fault.
                            let paused_text = if paused_indicator_visible(
                                engine_live,
                                stats.pipeline_paused,
                                crate::app::AppState::pause_flash_on(stats.pause_flash_tick),
                            ) {
                                Some("PAUSED")
                            } else {
                                None
                            };
                            // The engine's total: the sum of every module's
                            // rolling average, right-aligned in the same fixed
                            // ms column (absolute x = the module rows' ms
                            // column) so the value lines up under the group
                            // headers' totals.
                            let total_ms = group_total_ms(stats, Group::Adapters)
                                + group_total_ms(stats, Group::PreProcess)
                                + group_total_ms(stats, Group::InProcess)
                                + group_total_ms(stats, Group::PostProcess);
                            let total_text = format_ms(Some(total_ms));
                            let indent = 2;
                            let ms_start = indent + STATUS_COL + STATUS_TEXT_COL + AUTOSTART_COL;
                            let ms_right = ms_start + MS_COL;
                            // Everything rendered so far: name + status. The
                            // badge (if any) then the ms must end at `ms_right`.
                            let rendered: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                            let mut gap = ms_start.saturating_sub(rendered);
                            // The badge consumes gap space; if it overflows the
                            // autostart column the ms still right-aligns to the
                            // fixed right edge.
                            if let Some(badge) = paused_text {
                                spans.push(Span::styled(
                                    format!("{badge:>width$}", width = gap.saturating_add(badge.len())),
                                    row_style
                                        .fg(if is_selected { Color::Black } else { colors.status_color("stopped") })
                                        .add_modifier(Modifier::BOLD),
                                ));
                                gap = 0;
                            }
                            let rendered: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                            let before_ms = ms_right
                                .saturating_sub(rendered)
                                .saturating_sub(total_text.len())
                                .max(1);
                            spans.push(Span::styled(
                                format!("{}{}", " ".repeat(before_ms), total_text),
                                row_style.fg(if is_selected { Color::Black } else { ms_color(Some(total_ms)) }),
                            ));
                            // Messages/minute the engine could sustain at this
                            // latency before the queue fills — the inverse of
                            // the total ms, in its own fixed column beside it.
                            let throughput = format_throughput(total_ms);
                            if !throughput.is_empty() {
                                spans.push(Span::styled(
                                    format!("{:>width$}", throughput, width = THROUGHPUT_COL),
                                    row_style.fg(if is_selected {
                                        Color::Black
                                    } else {
                                        Color::Cyan
                                    }),
                                ));
                            }
                            (Line::from(spans), None)
                        }
                        EntryKind::Module(mi) => {
                            let module = &stats.module_entries[mi];
                            let waiting = prompts_waiting.contains(module.name.as_str());
                            let status_text = if waiting {
                                "waiting for prompt"
                            } else {
                                module.status.as_str()
                            };
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
                            // Left: name + status, both fixed-width so the autostart marker and the
                            // rolling average ms each land in their own column
                            // on every row.
                            let mut spans = vec![
                                Span::styled(
                                    format!("  {:<width$}", module.name, width = STATUS_COL),
                                    row_style
                                        .fg(if is_selected { Color::Black } else { Color::White }),
                                ),
                                Span::styled(
                                    format!("{:<width$}", status_text, width = STATUS_TEXT_COL),
                                    row_style.fg(status_color),
                                ),
                            ];
                            // Autostart marker: `A` for a module set to start
                            // automatically, blank otherwise — a fixed column
                            // so the ms never shifts.
                            let autostart_text = if module.autostart { "A" } else { "" };
                            spans.push(Span::styled(
                                format!("{:<width$}", autostart_text, width = AUTOSTART_COL),
                                row_style.fg(if is_selected {
                                    Color::Black
                                } else {
                                    Color::DarkGray
                                }),
                            ));
                            // The rolling average ms, right-aligned in its fixed column, coloured by
                            // latency band.
                            let ms_text = format_ms(module.avg_ms);
                            spans.push(Span::styled(
                                format!("{:>width$}", ms_text, width = MS_COL),
                                row_style.fg(if is_selected {
                                    Color::Black
                                } else {
                                    ms_color(module.avg_ms)
                                }),
                            ));
                            (
                                Line::from(spans),
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
            }
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
        // The engine header + engine row always exist, so the row count is
        // never zero: with no modules registered at all there are still those
        // two rows to sit on and look at, which is why the old "nothing to
        // navigate" early return is gone. Every clamp is against the grouped
        // view, so the up/down bounds can never land on a row that does not
        // exist (or, worse, on `module_entries[selected]` for a `selected` that
        // now means something else).
        let rows = grouped_rows(stats);
        let lines = grouped_lines(&rows);
        let total = rows.len();
        self.selected = clamp_selected(self.selected, total);
        self.scroll = clamp_selected(self.scroll, lines.len());

        use crossterm::event::{KeyCode, KeyModifiers};
        // The Shift guards matter: crossterm reports Shift+arrow as the plain
        // key with SHIFT set, and without the guard the navigation arms below
        // would swallow the very keys Shift+arrow is supposed to mean.
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                // Group headers are DISPLAY-ONLY: selection lands on the engine
                // row or a module row, so down/up skip the `[NAME]` lines.
                self.selected = next_selectable(self.selected, total, &rows, 1);
                self.scroll = grouped_selected_line(&lines, self.selected);
                Some(Action::Noop)
            }
            KeyCode::Char('k') | KeyCode::Up if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.selected = next_selectable(self.selected, total, &rows, -1);
                self.scroll = grouped_selected_line(&lines, self.selected);
                Some(Action::Noop)
            }
            // Shift+down / Shift+up ask the dispatcher to move the selected
            // module earlier or later through the pipeline. The window answers
            // only "which module, and which direction"; the dispatch reads the
            // engine's config.json ordering and resolves the move (a stage
            // jump, an in-process reorder, or a no-op for an input adapter or a
            // stage edge). A header or the engine names no module, so those are
            // a no-op here — the key is still consumed, so it does not fall
            // through to some other binding.
            //
            // crossterm reports Shift+letter as the UPPERCASE character with
            // the SHIFT modifier (`Shift+J` arrives as `Char('J') + SHIFT`),
            // the same model the `E`/`R`/`X` bindings use. So the keybind
            // moves match the uppercase forms — a lowercase `Char('j') +
            // SHIFT` never arrives on a terminal. The arrow keys with Shift
            // stay bound too. (The navigation arms above already exclude
            // SHIFT, so the two never overlap.)
            KeyCode::Char('J') | KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.stage_action(stats, StageDirection::Later)
            }
            KeyCode::Char('K') | KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.stage_action(stats, StageDirection::Earlier)
            }
            // `p` is NOT handled here. It is a global (nav) binding, matched
            // before the focused window's own key handling, so the pause toggle
            // works from every window and keeps working with the engine row
            // selected — reaching for it in here would shadow the global.
            KeyCode::Char('w') => Some(Action::PopOut("modules".to_string())),
            _ => None,
        }
    }

    fn start_config_editor(&mut self, target: crate::app::ConfigTarget, label: &str, dir: PathBuf) {
        let rows = Self::load_rows(label, &dir);
        self.editing = Some(ConfigEditor {
            target: match target {
                crate::app::ConfigTarget::Engine => EditorTarget::Engine,
                crate::app::ConfigTarget::Module => EditorTarget::Module,
                crate::app::ConfigTarget::UserDb => EditorTarget::UserDb,
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
        // where that knowledge lives. A header row returns None too — a stage
        // name has no config to edit.
        match self.selected_row(stats).kind {
            EntryKind::Engine => Some((
                crate::app::ConfigTarget::Engine,
                ENGINE_ROW_LABEL.to_string(),
                crate::supervisor::engine_dir(),
            )),
            _ => None,
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
            // splice inserts the WHOLE string at pos, in order. Inserting one
            // char at the same `pos` per iteration reverses the paste (each
            // char lands before the previous one): "https://" came out as
            // "//:sptth". A paste is one logical edit at the cursor.
            chars.splice(pos..pos, text.chars());
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
            avg_ms: None,
            autostart: false,
        }
    }

    /// Stats with `n` connected modules, all pre-process, so the grouped view
    /// has `n + 6` rows: the [ENGINE] header + engine row, the [ADAPTERS]
    /// header, the [PRE-PROCESS] header + `n` modules, and the [IN-PROCESS]
    /// and [POST-PROCESS] headers (always present, even when empty).
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
            // The first module row: index 4 in the grouped view
            // ([ENGINE] header, engine, [ADAPTERS], [PRE-PROCESS], m0), so the
            // per-module hint set is on show.
            win.selected = 4;
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
        assert!(
            row.contains("cockatiel-engine") && row.contains("connected"),
            "indicator not beside the engine status: {:?}",
            row
        );

        assert!(
            !render("connected", true, dark_tick).contains("PAUSED"),
            "the flash's dark phase must clear the indicator"
        );
        let running = render("connected", false, visible_tick);
        assert!(running.contains("cockatiel-engine"), "baseline render: {}", running);
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
    fn editor_paste_keeps_the_text_in_order_at_the_cursor() {
        // Regression: pasting inserted each char at the SAME cursor index, so
        // every char landed before the previous one and the paste came out
        // reversed -- "https://" became "//:sptth". A paste is ONE logical
        // edit at the cursor, in the order the text arrives.
        let tmp = std::env::temp_dir().join(format!("cockatiel-paste-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("config.json"), r#"{"model":"mms","model_source":""}"#).unwrap();

        let mut w = ModulesWindow::new();
        w.start_config_editor(ConfigTarget::Module, "test-mod", tmp.clone());

        // Select the `model_source` value row directly (its path is the key
        // "model_source"), so the test does not depend on the flatten order.
        {
            use super::Seg;
            let idx = w
                .editing
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .position(|r| {
                    matches!(r.path.first(), Some(Seg::Key(k)) if k == "model_source")
                })
                .expect("model_source row must exist");
            let mut ed = w.editing.take().unwrap();
            ed.selected = idx;
            w.editing = Some(ed);
        }

        let url = "https://huggingface.co/facebook/mms-1b-all";
        assert!(w.editor_paste(url), "paste should be consumed by the editor");

        let row = w.editing.as_ref().unwrap().rows[w.editing.as_ref().unwrap().selected].clone();
        assert_eq!(row.value, url, "paste must arrive in order, not reversed");
        assert_eq!(
            row.cursor,
            row.value.chars().count(),
            "cursor must sit after the pasted text"
        );

        // Pasting in the middle (cursor moved back two chars) inserts there.
        let mut ed = w.editing.take().unwrap();
        ed.rows[ed.selected].cursor = ed.rows[ed.selected].value.chars().count() - 2;
        w.editing = Some(ed);
        assert!(w.editor_paste("XX"));
        let row = w.editing.as_ref().unwrap().rows[w.editing.as_ref().unwrap().selected].clone();
        assert_eq!(
            row.value,
            "https://huggingface.co/facebook/mms-1b-aXXll",
            "mid-string paste must splice at the cursor"
        );

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
    //! The engine is a selectable row under a `[ENGINE]` header, ahead of the
    //! grouped module sections. `self.selected` therefore indexes a VIEW
    //! (`grouped_rows`) rather than `stats.module_entries`, and every site that
    //! used to index `module_entries` directly had to be re-derived. These
    //! tests pin the grouped index space, the display-line scroll, and the
    //! Shift+arrow stage moves, because the failure mode of getting any of them
    //! wrong is silent: the wrong row highlights, the wrong module gets
    //! stopped, or the list scrolls wrong.
    use super::{
        clamp_selected, scroll_for, next_selectable, grouped_rows, grouped_lines,
        grouped_selected_line, GroupedLine, GroupedRow, Group, EntryKind, header_label,
        StageDirection, ENGINE_REMOVED_STATUS, ENGINE_ROW, ENGINE_ROW_LABEL, STATUS_COL, Reload,
        ENGINE_CONFIG_KEYS, ms_color, format_throughput,
    };
    use crate::app::{ConfigTarget, Window};
    use crate::db::GlobalStats;
    use crate::hotkeys::{default_hotkeys, Action};
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

    fn shift_down() -> KeyEvent {
        KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)
    }

    fn shift_up() -> KeyEvent {
        KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)
    }

    /// The row index of the first row matching `group` + `kind`, so a test
    /// can name the row it means instead of hardcoding where a header sits.
    /// `kind` matches exactly, so a module row is located by its
    /// `module_entries` index (`EntryKind::Module(0)` for `m0`).
    fn row_index(rows: &[GroupedRow], group: Group, kind: EntryKind) -> Option<usize> {
        rows.iter().position(|r| r.group == group && r.kind == kind)
    }

    /// Stats whose modules carry explicit positions, so a test can build a
    /// group layout other than the all-pre-process one `stats_with_modules`
    /// makes.
    fn stats_with(pos: &[(&str, &str)]) -> GlobalStats {
        crate::db::GlobalStats {
            module_entries: pos
                .iter()
                .map(|(name, position)| {
                    let mut m = super::tests::module(name);
                    m.position = position.to_string();
                    m
                })
                .collect(),
            ..Default::default()
        }
    }

    // ── the row model ────────────────────────────────────────────────────

    #[test]
    fn grouped_rows_orders_engine_then_stage_groups() {
        let stats = stats_with(&[
            ("adapter", "input"),
            ("clip", "preprocess"),
            ("score", "inprocess"),
            ("term", "postprocess"),
        ]);
        let describe = |r: GroupedRow| format!("{:?}:{:?}", r.group, r.kind);
        let kinds: Vec<String> = grouped_rows(&stats).iter().map(|r| describe(*r)).collect();
        assert_eq!(
            kinds,
            vec![
                "Engine:Header",
                "Engine:Engine",
                "Adapters:Header",
                "Adapters:Module(0)",
                "PreProcess:Header",
                "PreProcess:Module(1)",
                "InProcess:Header",
                "InProcess:Module(2)",
                "PostProcess:Header",
                "PostProcess:Module(3)",
            ],
            "the groups must appear in pipeline order, each headed by its own \
             header and holding its own modules"
        );
    }

    #[test]
    fn a_module_without_a_known_position_falls_into_preprocess() {
        // `output` is the engine's alias for the post-process stage; anything
        // unrecognised falls into pre-process, matching the engine's own
        // ordering default for an unknown capability.
        let stats = stats_with(&[("term", "output"), ("odd", "not-a-stage")]);
        let rows = grouped_rows(&stats);
        assert!(
            rows.iter().any(|r| r.group == Group::PostProcess && r.kind == EntryKind::Module(0)),
            "output must be treated as post-process: {:?}",
            rows
        );
        assert!(
            rows.iter().any(|r| r.group == Group::PreProcess && r.kind == EntryKind::Module(1)),
            "an unknown position must fall into pre-process: {:?}",
            rows
        );
    }

    #[test]
    fn every_group_header_is_always_present_even_when_empty() {
        // All four stage headers must show regardless of whether any module
        // sits in them — the operator sees the full pipeline shape at a glance
        // and knows a stage exists even before any module is added to it.
        let stats = stats_with(&[("clip", "preprocess"), ("term", "postprocess")]);
        let rows = grouped_rows(&stats);
        for group in [Group::Adapters, Group::PreProcess, Group::InProcess, Group::PostProcess] {
            assert!(
                rows.iter().any(|r| r.group == group && r.kind == EntryKind::Header),
                "{group:?} header must be present even with no modules in it"
            );
        }
        // The empty IN-PROCESS group has its header but no module rows.
        assert!(!rows.iter().any(|r| r.group == Group::InProcess && matches!(r.kind, EntryKind::Module(_))));
    }

    #[test]
    fn all_group_headers_share_the_same_indent() {
        // The status column is aligned, so headers sit at the same 2-space base
        // as the module rows — no extra indent on the pipeline stages.
        for group in [Group::Engine, Group::Adapters, Group::PreProcess, Group::InProcess, Group::PostProcess] {
            let name = match group {
                Group::Engine => "ENGINE",
                Group::Adapters => "ADAPTERS",
                Group::PreProcess => "PRE-PROCESS",
                Group::InProcess => "IN-PROCESS",
                Group::PostProcess => "POST-PROCESS",
            };
            assert_eq!(header_label(group), format!("  [{name}]"));
        }
    }

    #[test]
    fn the_engine_row_and_headers_have_no_module() {
        let stats = stats_with_modules(2);
        let rows = grouped_rows(&stats);
        // The accessor must refuse rather than hand back a neighbouring row's
        // module: `x` with a header or the engine selected would otherwise stop
        // module 0.
        let engine = rows[ENGINE_ROW];
        assert_eq!(engine.module_index(), None);
        assert_eq!(engine.module_name(&stats), None);
        assert_eq!(rows[0].module_index(), None, "the [ENGINE] header is not a module");
        assert_eq!(rows[0].module_name(&stats), None);
        // The [PRE-PROCESS] header — after [ENGINE], the engine row and the
        // [ADAPTERS] header (row 3) — found by lookup, not hardcoded.
        let pre = row_index(&rows, Group::PreProcess, EntryKind::Header).expect("[PRE-PROCESS] header");
        assert_eq!(rows[pre].module_index(), None);
        assert_eq!(rows[pre].module_name(&stats), None);
        // The row right after it is the first module, m0.
        let m0 = row_index(&rows, Group::PreProcess, EntryKind::Module(0)).expect("m0");
        assert_eq!(rows[m0].module_name(&stats), Some("m0".to_string()));
    }

    #[test]
    fn the_row_count_is_never_zero() {
        // The [ENGINE] header + engine row exist even with no modules, and
        // every stage header is present too, so an empty list still has
        // something selectable (and the old "nothing to navigate" early return
        // is gone).
        let rows = grouped_rows(&GlobalStats::default());
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0], GroupedRow { group: Group::Engine, kind: EntryKind::Header });
        assert_eq!(rows[1], GroupedRow { group: Group::Engine, kind: EntryKind::Engine });
        // The four stage headers show the full pipeline shape even with no
        // modules in them.
        for (i, group) in [Group::Adapters, Group::PreProcess, Group::InProcess, Group::PostProcess]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                rows[2 + i],
                GroupedRow { group, kind: EntryKind::Header },
                "an empty group must still get a header"
            );
        }
    }

    #[test]
    fn the_window_reports_the_engine_row_as_not_a_module() {
        let mut w = super::ModulesWindow::new();
        let stats = stats_with_modules(2);
        // Default selection is the engine row.
        assert_eq!(w.selected, ENGINE_ROW);
        assert!(!w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats), None);

        // The first module is found by lookup, not a magic number: after [ENGINE],
        // the engine row, [ADAPTERS] and [PRE-PROCESS] it sits at row 4 with
        // two pre-process modules.
        let m0 = row_index(&grouped_rows(&stats), Group::PreProcess, EntryKind::Module(0)).expect("m0");
        w.selected = m0;
        assert!(w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats), Some("m0".to_string()));

        // A group header is selectable but names no module.
        let adapters = row_index(&grouped_rows(&stats), Group::Adapters, EntryKind::Header).expect("[ADAPTERS]");
        w.selected = adapters;
        assert!(!w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats), None);
    }

    // ── selection + scroll arithmetic ─────────────────────────────────────

    #[test]
    fn navigation_skips_group_headers_and_clamps_at_both_ends() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(2);
        // rows = [ENGINE], engine, [ADAPTERS], [PRE-PROCESS], m0, m1,
        // [IN-PROCESS], [POST-PROCESS] → 8 rows, of which 3 are selectable.
        let rows = grouped_rows(&stats);
        assert_eq!(rows.len(), 8);
        assert_eq!(w.selected, ENGINE_ROW);

        // Up from the engine row: the [ENGINE] header is display-only, so the
        // selection stays on the engine row.
        w.handle_key(up(), &mut stats);
        assert_eq!(w.selected, ENGINE_ROW);
        w.handle_key(up(), &mut stats);
        assert_eq!(w.selected, ENGINE_ROW);

        // Down: the [ADAPTERS]/[PRE-PROCESS] headers are skipped, so one press
        // lands on the first module, and the walk never stops on the trailing
        // [IN-PROCESS]/[POST-PROCESS] headers.
        let m0 = row_index(&rows, Group::PreProcess, EntryKind::Module(0)).expect("m0");
        let m1 = row_index(&rows, Group::PreProcess, EntryKind::Module(1)).expect("m1");
        w.handle_key(down(), &mut stats);
        assert_eq!(w.selected, m0, "one down from the engine must land on the first module");
        assert_eq!(w.selected_module_name(&stats).as_deref(), Some("m0"));
        w.handle_key(down(), &mut stats);
        assert_eq!(w.selected, m1);
        for _ in 0..10 {
            w.handle_key(down(), &mut stats);
        }
        assert_eq!(w.selected, m1, "down must stop at the last selectable row, never on a header");
        assert!(!rows[w.selected].is_header(), "a header must never become the selected row");

        // ...and back up: straight across the headers to the engine row.
        w.handle_key(up(), &mut stats);
        assert_eq!(w.selected, m0);
        w.handle_key(up(), &mut stats);
        assert_eq!(w.selected, ENGINE_ROW);
        assert_eq!(w.selected_module_name(&stats), None);
    }

    #[test]
    fn a_shrunken_module_list_pulls_the_selection_onto_a_selectable_row() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(3);
        // Walk to the last MODULE (m2), not just the last row — the shrink
        // test is about a stale selection, so land somewhere meaningful.
        let m2 = row_index(&grouped_rows(&stats), Group::PreProcess, EntryKind::Module(2)).expect("m2");
        while w.selected < m2 {
            w.handle_key(down(), &mut stats);
        }
        assert!(w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats).as_deref(), Some("m2"));

        // The engine drops two of its modules (a module removed itself). The
        // stale selection clamps onto a HEADER, and the next press must walk
        // back to the last SELECTABLE row — the walk never settles on a header.
        stats.module_entries.truncate(1);
        w.handle_key(key('j'), &mut stats);
        let rows = grouped_rows(&stats);
        let m0 = row_index(&rows, Group::PreProcess, EntryKind::Module(0)).expect("m0");
        assert_eq!(
            w.selected, m0,
            "a shrunken list must land on the last selectable row, not a header"
        );
        w.handle_key(down(), &mut stats);
        assert_eq!(w.selected, m0, "down past the end must hold on the last selectable row");

        // With NO modules at all the only selectable row is the engine row —
        // a stale selection pulled back onto the trailing headers walks home.
        stats.module_entries.clear();
        w.handle_key(down(), &mut stats);
        assert_eq!(w.selected, ENGINE_ROW, "with no modules, the engine row is the only landing spot");
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
    fn next_selectable_skips_headers_and_clamps_at_both_ends() {
        // rows = [ENGINE](0), engine(1), [ADAPTERS](2), [PRE-PROCESS](3), m0(4),
        // m1(5), [IN-PROCESS](6), [POST-PROCESS](7) — 8 rows, 3 selectable.
        let stats = stats_with_modules(2);
        let rows = grouped_rows(&stats);
        let total = rows.len();
        let m0 = row_index(&rows, Group::PreProcess, EntryKind::Module(0)).expect("m0");
        let m1 = row_index(&rows, Group::PreProcess, EntryKind::Module(1)).expect("m1");

        // Down: engine → m0 → m1, then clamps on m1 (the trailing headers are
        // skipped, and a press at the bottom holds).
        assert_eq!(next_selectable(ENGINE_ROW, total, &rows, 1), m0);
        assert_eq!(next_selectable(m0, total, &rows, 1), m1);
        assert_eq!(next_selectable(m1, total, &rows, 1), m1);
        assert_eq!(
            next_selectable(total - 1, total, &rows, 1),
            m1,
            "down past the end must clamp on the last selectable row, not the boundary header"
        );

        // Up: m1 → m0 → engine, then clamps on the engine (the leading header
        // is skipped, and a press at the top holds).
        assert_eq!(next_selectable(m1, total, &rows, -1), m0);
        assert_eq!(next_selectable(m0, total, &rows, -1), ENGINE_ROW);
        assert_eq!(next_selectable(ENGINE_ROW, total, &rows, -1), ENGINE_ROW);
        assert_eq!(
            next_selectable(0, total, &rows, -1),
            ENGINE_ROW,
            "up from the top header must clamp on the engine row"
        );
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
    fn the_scroll_counts_display_lines_not_rows() {
        // One module in each of the four groups: 2 engine rows + 4 group
        // headers + 4 modules, plus a blank separator before every header
        // after the first — the separators consume screen lines without being
        // rows, and the scroll must count them or a row near a group boundary
        // falls off the bottom of the viewport.
        let stats = stats_with(&[
            ("a", "input"),
            ("p", "preprocess"),
            ("i", "inprocess"),
            ("t", "postprocess"),
        ]);
        let rows = grouped_rows(&stats);
        let lines = grouped_lines(&rows);
        assert_eq!(lines.len(), rows.len() + 4, "one blank per non-engine group");
        // Every header after the first is preceded by its blank separator.
        for (i, line) in lines.iter().enumerate() {
            if let GroupedLine::Row { index } = line {
                if rows[*index].is_header() && *index > 0 {
                    assert_eq!(lines[i - 1], GroupedLine::Blank, "header at line {}", i);
                }
            }
        }
        // The last row maps to the last display line, and a scroll that keeps
        // it on screen is valid for the view.
        let last_row = rows.len() - 1;
        let last_line = grouped_selected_line(&lines, last_row);
        assert_eq!(last_line, lines.len() - 1);
        let visible = 6usize;
        let scroll = scroll_for(last_line, 0, visible, lines.len());
        assert!(
            last_line >= scroll && last_line < scroll + visible,
            "the selected row must stay inside the viewport"
        );
    }

    #[test]
    fn the_engine_row_comes_back_into_view_when_it_is_selected_again() {
        // The engine is row 1, so scrolled down it is legitimately off screen.
        // The property that matters is the round trip: drive the real window to
        // the bottom, then walk the selection back to the engine and assert the
        // list scrolled home and the engine row is actually drawn again. A
        // keep-visible that forgot the index shift would leave it stranded.
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(8);
        let (width, height) = (60u16, 12u16);

        for _ in 0..grouped_rows(&stats).len() {
            w.handle_key(down(), &mut stats);
        }
        let bottom = super::tests::render(&mut w, &stats, width, height);
        assert!(!bottom.contains("cockatiel"), "expected the list to be scrolled away from the engine:\n{}", bottom);

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
        // is the selected one — for EVERY row of the grouped view, headers
        // included.
        let mut w = super::ModulesWindow::new();
        let stats = stats_with_modules(8);
        let rows = grouped_rows(&stats);
        let (width, height) = (60u16, 12u16);
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        for (selected, grouped) in rows.iter().enumerate() {
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
            let expected = match grouped.kind {
                EntryKind::Header => header_label(grouped.group).trim().to_string(),
                EntryKind::Engine => "cockatiel-engine".to_string(),
                EntryKind::Module(i) => format!("m{}", i),
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
    /// would put "is there an engine" into `grouped_rows` and into every clamp,
    /// and a selection stored in the old index space would silently start
    /// naming a MODULE. With the row kept, `selected` means exactly what it
    /// meant before the removal.
    #[test]
    fn removing_the_engine_does_not_move_the_row_arithmetic() {
        let before = stats_with_modules(3);
        let mut after = before.clone();
        after.forget_engine();
        // `forget_engine` clears the module list, so the view shrinks to the
        // headers + the engine row — the engine is still its row, not gone,
        // and not demoted to a module.
        assert_eq!(grouped_rows(&before).len(), 9);
        assert_eq!(grouped_rows(&after).len(), 6);
        assert_eq!(grouped_rows(&after)[0], GroupedRow { group: Group::Engine, kind: EntryKind::Header });
        assert_eq!(grouped_rows(&after)[1], GroupedRow { group: Group::Engine, kind: EntryKind::Engine });
        assert_eq!(
            clamp_selected(8, grouped_rows(&after).len()),
            5,
            "a stale selection is pulled into range, and it lands on a header, not a module"
        );
        assert_eq!(scroll_for(0, 3, 2, grouped_lines(&grouped_rows(&after)).len()), 0);

        // And the window agrees: a selection left pointing past the end of the
        // shrunken list resolves to the ENGINE row — the one row that always
        // exists — not to a module, so the per-module actions stay refused and
        // the engine-only ones are refused by the ACTION, not by the row
        // vanishing under it.
        let mut w = super::ModulesWindow::new();
        w.selected = 8;
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
        assert!(w.selection_is_engine(&stats), "row ENGINE_ROW is the engine");
        assert!(!w.selection_is_module(&stats), "…and it is not a module");
        // One down lands on the FIRST MODULE — the [ADAPTERS] and [PRE-PROCESS]
        // headers are skipped by navigation.
        w.handle_key(down(), &mut stats);
        assert!(!w.selection_is_engine(&stats));
        assert!(w.selection_is_module(&stats), "one down from the engine must land on a module");
        // A group header — reachable only by direct assignment now — is still
        // not a module, and a stale selection pulled back onto one stays that
        // way.
        let adapters = row_index(&grouped_rows(&stats), Group::Adapters, EntryKind::Header).expect("[ADAPTERS]");
        w.selected = adapters;
        assert!(!w.selection_is_engine(&stats));
        assert!(!w.selection_is_module(&stats), "a group header is not a module");
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
            .find(|l| l.contains("cockatiel-engine"))
            .expect("the engine row");
        // Its own status text, not a module name...
        assert!(engine.contains("connected"), "engine row: {:?}", engine);
        // ...a name no module has, and no stage tag, because it is in no stage.
        assert!(!engine.contains('['), "the engine is not in a stage: {:?}", engine);
        // The [ENGINE] section header leads the list again — the engine is now
        // one row under the group it heads.
        assert!(screen.contains("[ENGINE]"), "the [ENGINE] header is missing:\n{}", screen);
        // The modules are still there, still under their own group header.
        assert!(screen.contains("[PRE-PROCESS]"), "module group header missing:\n{}", screen);
        assert!(screen.contains("m0"), "module rows missing:\n{}", screen);
        assert!(screen.contains("m1"), "module rows missing:\n{}", screen);
    }

    // ── the action rule for the engine row ───────────────────────────────

    /// Per-module actions are REFUSED on the engine row AND on every group header,
    /// loudly, rather than falling through to a neighbouring module. The app
    /// side of this is `is_module_scoped` + `focused_selection_is_module` in
    /// main.rs; what the window owes them is a selection that says "not a
    /// module".
    #[test]
    fn a_module_action_has_no_target_on_the_engine_row() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(3);
        // The engine row names no module, so the "first known module" fallback
        // must not fire there.
        assert!(!w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats), None);

        // One down lands on the FIRST module — the [ADAPTERS] and [PRE-PROCESS]
        // headers are skipped — so the module fallback is correct there.
        w.handle_key(down(), &mut stats);
        assert!(w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats).as_deref(), Some("m0"));

        // A group header — reachable only by direct assignment, since
        // navigation skips it — still names no module.
        let adapters = row_index(&grouped_rows(&stats), Group::Adapters, EntryKind::Header).expect("[ADAPTERS]");
        w.selected = adapters;
        assert!(!w.selection_is_module(&stats));
        assert_eq!(w.selected_module_name(&stats), None);

        // Up from the header crosses back to the engine row, still no module.
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
        // One down from the engine lands on the first module — the [ADAPTERS]
        // and [PRE-PROCESS] headers are skipped.
        w.handle_key(down(), &mut stats);
        assert!(w.selection_is_module(&stats));
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

    /// A group header names a stage, so its bar is the window-level keys only —
    /// no module actions (there is no module), and no engine actions (there is
    /// no engine on a header row). Navigation never lands on a header, so the
    /// bar is reached by direct assignment — the renderer still draws whatever
    /// `selected` names.
    #[test]
    fn the_hint_bar_offers_only_window_actions_on_a_group_header() {
        let mut w = super::ModulesWindow::new();
        let stats = stats_with_modules(3);
        let adapters = row_index(&grouped_rows(&stats), Group::Adapters, EntryKind::Header).expect("[ADAPTERS]");
        w.selected = adapters;
        assert!(w.selected_row(&stats).is_header());
        let bar = super::tests::render_with(&mut w, &stats, 220, 20, &keymap());
        assert_eq!(
            bar_labels(&bar),
            vec!["nav", "select", "users", "pause"],
            "a header names no module and no engine:\n{}",
            bar
        );
        for gone in ["start:[s]", "del:[d]", "restart:[R]", "detach:[X]", "edit:[E]"] {
            assert!(!bar.contains(gone), "a header must not advertise {:?}:\n{}", gone, bar);
        }
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

    // ── Shift+arrows: moving a module through the pipeline ──────────────

    /// The window answers only "which module, and which direction" — the
    /// actual move (a stage jump vs an in-process reorder) is resolved by the
    /// dispatcher against the engine's config.json ordering, which the window
    /// has no access to. So a module ALWAYS emits a direction, even at a stage
    /// edge or on an adapter; the no-op lives in the resolver.
    #[test]
    fn shift_on_a_module_emits_the_direction_not_a_target_stage() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(2); // all pre-process
        w.handle_key(down(), &mut stats); // m0 (headers are skipped)
        assert!(w.selection_is_module(&stats));
        assert_eq!(
            w.handle_key(shift_down(), &mut stats),
            Some(Action::MoveModuleStage("m0".to_string(), StageDirection::Later))
        );
        assert_eq!(
            w.handle_key(shift_up(), &mut stats),
            Some(Action::MoveModuleStage("m0".to_string(), StageDirection::Earlier)),
            "even a pre module at the earlier edge emits the direction; the resolver refuses it"
        );
    }

    /// The window has NOT changed the view (the dispatch owns that), so a
    /// Shift+down on a pre module still emits `Later`; the resolver turns pre +
    /// Later into a jump into in-process.
    #[test]
    fn shift_down_on_a_pre_module_emits_later_until_the_dispatch_moves_it() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(3); // all pre-process
        w.handle_key(down(), &mut stats); // m0 (headers are skipped)
        assert_eq!(
            w.handle_key(shift_down(), &mut stats),
            Some(Action::MoveModuleStage("m0".to_string(), StageDirection::Later))
        );
        // Simulate the dispatch having applied the move: the module now sits in
        // the in-process group, and Shift+up asks for the earlier direction
        // (which the resolver turns into a reorder, or a fall-out to pre).
        stats.module_entries[0].position = "inprocess".to_string();
        w.selected = grouped_rows(&stats)
            .iter()
            .position(|r| r.kind == EntryKind::Module(0))
            .expect("m0 must be in the grouped view");
        assert_eq!(
            w.handle_key(shift_up(), &mut stats),
            Some(Action::MoveModuleStage("m0".to_string(), StageDirection::Earlier))
        );
    }

    /// A module at the requested edge, or an input adapter, is refused by the
    /// RESOLVER (which sees the config order), not by the window — the window
    /// cannot know a post module is already last, or that an adapter feeds the
    /// pipeline rather than running inside it, so it keeps emitting the
    /// direction and the dispatcher no-ops.
    #[test]
    fn shift_on_an_adapter_or_a_stage_edge_still_emits_the_direction() {
        // A post-process module shifted down is already last.
        let mut stats = stats_with(&[("term", "postprocess")]);
        let mut w = super::ModulesWindow::new();
        w.handle_key(down(), &mut stats); // term (headers are skipped)
        assert_eq!(
            w.handle_key(shift_down(), &mut stats),
            Some(Action::MoveModuleStage("term".to_string(), StageDirection::Later))
        );
        // ...and an earlier shift-up DOES move it into in-process.
        assert_eq!(
            w.handle_key(shift_up(), &mut stats),
            Some(Action::MoveModuleStage("term".to_string(), StageDirection::Earlier))
        );

        // An input adapter is emitted too; the resolver refuses it.
        let mut stats2 = stats_with(&[("discord", "input")]);
        let mut w2 = super::ModulesWindow::new();
        w2.handle_key(down(), &mut stats2); // discord (headers are skipped)
        assert_eq!(
            w2.handle_key(shift_down(), &mut stats2),
            Some(Action::MoveModuleStage("discord".to_string(), StageDirection::Later))
        );
        assert_eq!(
            w2.handle_key(shift_up(), &mut stats2),
            Some(Action::MoveModuleStage("discord".to_string(), StageDirection::Earlier))
        );
    }

    /// The Shift-move works with the j/k KEYBINDS, not only the arrow keys —
    /// the same pair that navigates without Shift. Shift+j moves later (down),
    /// Shift+k earlier (up).
    #[test]
    fn shift_j_and_shift_k_move_through_the_pipeline() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(2); // all pre-process
        w.handle_key(down(), &mut stats); // m0

        let shift_j = KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT);
        assert_eq!(
            w.handle_key(shift_j, &mut stats),
            Some(Action::MoveModuleStage("m0".to_string(), StageDirection::Later)),
            "Shift+j must move a module later, like Shift+down"
        );

        let shift_k = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT);
        assert_eq!(
            w.handle_key(shift_k, &mut stats),
            Some(Action::MoveModuleStage("m0".to_string(), StageDirection::Earlier)),
            "Shift+k must move a module earlier, like Shift+up"
        );
    }

    /// After a stage move the cursor re-anchors onto the moved module's NEW row,
    /// so the operator is not left looking at whatever the old index now names.
    #[test]
    fn select_module_name_reanchors_the_cursor_onto_the_moved_module() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(2); // m0, m1 both pre-process
        // Select m0 (row 4: [ENGINE], engine, [ADAPTERS], [PRE-PROCESS], m0).
        w.select_module_name("m0", &stats);
        assert_eq!(w.selected_module_name(&stats).as_deref(), Some("m0"));

        // Move m0 to in-process (the dispatch rewrites its position), then the
        // cursor must follow it to the in-process group.
        stats.module_entries[0].position = "inprocess".to_string();
        w.select_module_name("m0", &stats);
        assert_eq!(w.selected_module_name(&stats).as_deref(), Some("m0"));
        // Its row now sits inside the [IN-PROCESS] group, not pre-process.
        let row = w.selected_row(&stats);
        assert_eq!(row.group, Group::InProcess);

        // An unknown name leaves the selection alone.
        w.select_module_name("does-not-exist", &stats);
        assert_eq!(w.selected_module_name(&stats).as_deref(), Some("m0"));
    }

    /// Headers and the engine row name no module, so a Shift+arrow there is a
    /// no-op — but the key is still consumed, so it does not fall through to
    /// navigation or any other binding.
    #[test]
    fn shift_on_a_header_or_the_engine_row_is_a_noop() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with_modules(2);
        // Engine row.
        assert_eq!(w.handle_key(shift_down(), &mut stats), Some(Action::Noop));
        assert_eq!(w.handle_key(shift_up(), &mut stats), Some(Action::Noop));
        // A group header — reached by direct assignment, since navigation skips
        // it — is a no-op too.
        let adapters = row_index(&grouped_rows(&stats), Group::Adapters, EntryKind::Header).expect("[ADAPTERS]");
        w.selected = adapters;
        assert_eq!(w.handle_key(shift_down(), &mut stats), Some(Action::Noop));
    }

    // ── the status column ────────────────────────────────────────────────

    /// Every module's status (and the engine's) starts at the same x, so the
    /// status column reads as one vertical line instead of ragged text. The
    /// name column width is `STATUS_COL`; the engine row is padded to the same
    /// width so its status lines up with the modules below it.
    #[test]
    fn the_status_column_is_aligned_across_all_rows() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with(&[
            ("discord-adapter", "input"),
            ("clip", "preprocess"),
            ("banned-words-manager", "inprocess"),
        ]);
        stats.engine_status = "connected".to_string();
        let screen = super::tests::render(&mut w, &stats, 120, 24);
        // The status column: the window's left border (`│`, 3 UTF-8 bytes) +
        // the 2-space row indent + the fixed-width name column.
        let status_at = 3 + 2 + STATUS_COL;
        for needle in ["cockatiel-engine", "discord-adapter", "clip", "banned-words-manager"] {
            let line = screen
                .lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{} is missing:\n{}", needle, screen));
            let col = line
                .find("connected")
                .unwrap_or_else(|| panic!("{} has no status on its line: {:?}", needle, line));
            assert_eq!(
                col, status_at,
                "the {} status must start at the status column (x={})",
                needle, status_at
            );
        }
    }

    #[test]
    fn the_ms_column_shows_rolling_averages_category_sums_and_autostart() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with(&[
            ("banned-words", "preprocess"),
            ("language-constrainer", "inprocess"),
            ("reprimand", "inprocess"),
            ("twitch-adapter", "input"),
        ]);
        // Rolling averages (ms). banned-words is sub-ms; the two in-process
        // modules sum to the category total; the adapter has none yet.
        stats.module_entries.iter_mut().for_each(|m| match m.name.as_str() {
            "banned-words" => m.avg_ms = Some(0.4),
            "language-constrainer" => m.avg_ms = Some(7.3),
            "reprimand" => m.avg_ms = Some(5.2),
            "twitch-adapter" => m.avg_ms = None,
            _ => {}
        });
        // `A` is the autostart marker.
        stats
            .module_entries
            .iter_mut()
            .find(|m| m.name == "language-constrainer")
            .unwrap()
            .autostart = true;
        let screen = super::tests::render(&mut w, &stats, 120, 30);

        let line = |needle: &str| {
            screen
                .lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing:\n{screen}"))
        };
        assert!(line("banned-words").contains("<1ms"), "sub-ms must render as <1ms");
        assert!(line("language-constrainer").contains("7.3ms"), "avg must render with one decimal");
        assert!(line("language-constrainer").contains(" A "), "autostart marker must show after status");
        assert!(!line("reprimand").contains(" A"), "non-autostart module must not show A");
        assert!(!line("twitch-adapter").contains("ms"), "no timing yet renders no ms");
        // Category sum: 7.3 + 5.2 = 12.5 on the in-process header.
        assert!(
            line("[IN-PROCESS]").contains("12.5ms"),
            "in-process header must show the category sum: {screen}"
        );
        // The ms is a FIXED COLUMN: every value ends at the same x (the right edge
        // of the ms column), and there is no label text next to it. Position is
        // measured from a module row (the ground truth for the column layout)
        // and the header's category sum must land at exactly the same spot.
        let lang = line("7.3ms");
        let ms_end = lang.find("7.3ms").unwrap() + "7.3ms".len();
        for needle in ["<1ms", "12.5ms"] {
            let l = line(needle);
            let end = l.find(needle).unwrap() + needle.len();
            assert_eq!(
                end, ms_end,
                "the {} value must end at the fixed ms column (x={}): {l:?}",
                needle, ms_end
            );
        }
        assert!(
            !screen.contains("category average"),
            "the header label must be removed: {screen}"
        );
    }

    /// The engine's end-to-end latency is shown in ONE place (the engine row,
    /// with the throughput it sustains), not also on the `[ENGINE]` header.
    #[test]
    fn the_engine_latency_and_throughput_live_only_on_the_engine_row() {
        let mut w = super::ModulesWindow::new();
        let mut stats = stats_with(&[
            ("banned-words", "preprocess"),
            ("language-constrainer", "inprocess"),
            ("reprimand", "inprocess"),
            ("tts-service", "postprocess"),
        ]);
        stats.engine_status = "connected".to_string();
        // 15.7 + 7.3 + 5.2 + 4.5 = 32.7ms total -> 60000/32.7 = 1834.8 -> 1835/min.
        stats.module_entries.iter_mut().for_each(|m| match m.name.as_str() {
            "banned-words" => m.avg_ms = Some(15.7),
            "language-constrainer" => m.avg_ms = Some(7.3),
            "reprimand" => m.avg_ms = Some(5.2),
            "tts-service" => m.avg_ms = Some(4.5),
            _ => {}
        });
        let screen = super::tests::render(&mut w, &stats, 120, 30);
        let line = |needle: &str| {
            screen
                .lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing:\n{screen}"))
        };

        // The engine row shows the total ms AND the messages/minute it could
        // sustain without the queue filling.
        let engine = line("cockatiel-engine");
        assert!(engine.contains("32.7ms"), "engine row must show total latency: {engine}");
        assert!(engine.contains("1835/min"), "engine row must show throughput: {engine}");

        // The [ENGINE] header shows NOTHING in the ms column (no duplicate).
        let header = line("[ENGINE]");
        assert!(
            !header.contains("ms"),
            "the engine header must not duplicate the total latency: {header}"
        );
    }

    #[test]
    fn throughput_is_the_inverse_of_total_latency() {
        assert_eq!(format_throughput(0.0), "", "no latency data -> no throughput");
        assert_eq!(format_throughput(1000.0), "60/min", "1s/message -> 60/min");
        assert_eq!(format_throughput(100.0), "600/min");
        assert_eq!(format_throughput(50.0), "1200/min");
        assert_eq!(format_throughput(32.7), "1835/min");
    }

    /// The ms column doubles as a heat gauge: an if/else tree colours the value
    /// by latency band, so a slowing module turns visibly alarming at the
    /// thresholds before the operator has to read the number.
    #[test]
    fn the_ms_color_bands_are_a_simple_if_else_tree() {
        use ratatui::style::Color;
        assert_eq!(ms_color(None), Color::DarkGray, "no timing is neutral");
        assert_eq!(ms_color(Some(0.0)), Color::Blue, "<6ms is dark blue");
        assert_eq!(ms_color(Some(5.9)), Color::Blue, "just under 6ms is still dark blue");
        assert_eq!(ms_color(Some(6.0)), Color::Green, ">=6ms flips to dark green");
        assert_eq!(ms_color(Some(14.9)), Color::Green, "just under 15ms is dark green");
        assert_eq!(ms_color(Some(15.0)), Color::Yellow, ">=15ms flips to dark yellow");
        assert_eq!(ms_color(Some(99.9)), Color::Yellow, "just under 100ms is dark yellow");
        assert_eq!(ms_color(Some(100.0)), Color::Red, ">=100ms is dark red");
        assert_eq!(ms_color(Some(1000.0)), Color::Red, "well over 100ms stays dark red");
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
        w.selected = 4; // rows[4] is the first module (m0): [ENGINE], engine, [ADAPTERS], [PRE-PROCESS], m0.
        assert_eq!(w.config_editor_target(&stats2), None);
        // A header row names a stage, so it has no config either.
        w.selected = 2;
        assert_eq!(w.config_editor_target(&stats2), None);
        // ...and it is the engine row that is the special one, not "no modules".
        stats2.module_entries.clear();
        w.selected = 0;
        assert_eq!(w.config_editor_target(&stats2), None, "the [ENGINE] header is not the engine");
        w.selected = ENGINE_ROW;
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
