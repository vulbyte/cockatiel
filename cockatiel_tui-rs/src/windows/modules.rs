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


/// One step in a config path (a map key or an array index).
/// The modules window: engine status, module list, databases, platforms.
#[derive(Debug, Clone)]
pub struct ModulesWindow {

    pub scroll: usize,

    pub selected: usize,

    /// Clickable region of the pending prompt's link, set during render.

    pub link_rect: Option<Rect>,

    pub link_url: Option<String>,

    /// Active config editor (takes over the window until Esc).

    editing: Option<ConfigEditor>,

    /// Module whose config was most recently saved by the editor (cleared on
    /// read) — lets the app warn that the module must be restarted.

    last_saved_module: Option<String>,

}

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
}

/// The config editor: a tree of the module's `.env` + `config.json` flattened
/// into editable rows, with "+" rows to add keys (maps) / items (arrays).
#[derive(Debug, Clone)]
struct ConfigEditor {
    module_name: String,
    dir: PathBuf,
    rows: Vec<EditorRow>,
    selected: usize,
    scroll: usize,
    /// Set after Esc is pressed: await y/n before saving (or discarding).
    confirm_save: bool,
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
        let module_name = ed.module_name.clone();

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
        if std::fs::write(&env_path, env_content).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
            }
        }

        let json_path = ed.dir.join("config.json");
        if let Ok(pretty) = serde_json::to_string_pretty(&json_root) {
            let _ = std::fs::write(&json_path, pretty);
        }
        eprintln!("[supervisor] saved config for {} (.env + config.json)", module_name);
        self.last_saved_module = Some(module_name);
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
            .title(format!(" config editor · {} ", ed.module_name))
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

        let available = (inner.y + inner.height).saturating_sub(y);
        if ed.selected < ed.scroll {
            ed.scroll = ed.selected;
        } else if ed.selected >= ed.scroll + available.max(1) as usize {
            ed.scroll = ed.selected.saturating_add(1).saturating_sub(available.max(1) as usize);
        }

        let mut prev_source = String::new();
        for i in ed.scroll..ed.rows.len() {
            if y >= inner.y + inner.height {
                break;
            }
            if ed.rows[i].source != prev_source {
                let header = if ed.rows[i].source == "env" {
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
                prev_source = ed.rows[i].source.clone();
                if y >= inner.y + inner.height {
                    break;
                }
            }

let row = &ed.rows[i];
            let is_selected = i == ed.selected && is_active;
            let is_add = matches!(row.kind, RowKind::AddMap | RowKind::AddList);
            let is_group = row.kind == RowKind::Group;
            // "+" rows hang one level under their parent; groups/scalars sit at
            // their own depth.
            let depth = if is_add { row.path.len() + 1 } else { row.path.len() };
            let prefix = Self::tree_prefix(&ed.rows, i, depth);
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

            let mut spans = vec![Span::styled(prefix.clone(), Style::default().fg(Color::DarkGray))];

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
                        spans.push(Span::styled("\u{2588}", Style::default().fg(Color::White).bg(Color::Red)));
                        spans.push(Span::styled(a, row_style));
                    } else {
                        spans.push(Span::styled(format!("{}{}", b, a), Style::default().fg(Color::Green)));
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
                // `config.json`: visible value with a cursor block on the selected row.
                if !label.is_empty() {
                    spans.push(Span::styled(format!("{} : ", label), Style::default().fg(Color::White)));
                }
                let chars: Vec<char> = row.value.chars().collect();
                let pos = row.cursor.min(chars.len());
                let b: String = chars[..pos].iter().collect();
                let a: String = chars[pos..].iter().collect();
                if is_selected {
                    spans.push(Span::styled(b, row_style));
                    spans.push(Span::styled("\u{2588}", Style::default().fg(Color::White).bg(Color::Red)));
                    spans.push(Span::styled(a, row_style));
                } else {
                    spans.push(Span::styled(format!("{}{}", b, a), row_style));
                }
            }

            let line = Line::from(spans);
            line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            y += 1;

            // Blank continuation line after each row (matches the tree look).
            if y < inner.y + inner.height && i + 1 < ed.rows.len() {
                let sep = Line::from(Span::styled(prefix.clone(), Style::default().fg(Color::DarkGray)));
                sep.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
                y += 1;
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
            rows.insert(idx + offset, row);
        }
    }

}
impl Window for ModulesWindow {
    fn id(&self) -> WindowId {
        WindowId::Modules
    }

    fn selected_module_name(&self, stats: &GlobalStats) -> Option<String> {
        stats.module_entries.get(self.selected).map(|m| m.name.clone())
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
        let mut y = inner.y;

        // ── [ENGINE] section ──
        if y < inner.y + inner.height {
            let header = Line::from(Span::styled("  [ENGINE]:", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
            header.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            y += 1;
        }

        let engine_status_color = if stats.engine_status == "connected" {
            colors.status_color("online")
        } else {
            colors.status_color("offline")
        };

        if y < inner.y + inner.height {
            let line = Line::from(vec![
                Span::styled("    cockatiel: ", Style::default().fg(Color::DarkGray)),
                Span::styled(&stats.engine_status, Style::default().fg(engine_status_color)),
            ]);
            line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            y += 1;
        }

        // Modules list, honoring scroll offset
        let available_lines = (inner.y + inner.height).saturating_sub(y);
        let module_rows: Vec<&crate::db::ModuleStatus> = stats.module_entries.iter().collect();
        let max_scroll = module_rows.len().saturating_sub(available_lines as usize);
        let scroll = self.scroll.min(max_scroll);

        // Keep the selected row visible within the scroll window
        let scroll = if self.selected >= scroll && self.selected < scroll + available_lines as usize {
            scroll
        } else if self.selected < scroll {
            self.selected
        } else {
            self.selected.saturating_add(1).saturating_sub(available_lines as usize)
        };

        // Modules that currently have an unanswered prompt waiting.
        let prompts_waiting: std::collections::HashSet<&str> =
            prompts.iter().map(|p| p.prompt.origin.as_str()).collect();

        let mut rendered_rows: HashMap<String, u16> = HashMap::new();
        for (i, module) in module_rows.iter().skip(scroll).take(available_lines as usize).enumerate() {
            if y >= inner.y + inner.height {
                break;
            }
            let idx = scroll + i;
            let is_selected = idx == self.selected && is_active;
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
            let line = Line::from(vec![
                Span::styled(format!("  {:<20}", module.name), row_style.fg(if is_selected { Color::Black } else { Color::White })),
                Span::styled(status_text, row_style.fg(status_color)),
                Span::styled(format!("  [{}]", module.position), Style::default().fg(Color::DarkGray)),
            ]);
            line.render(Rect { x: inner.x, y, width: inner.width, height: 1 }, buf);
            rendered_rows.insert(module.name.clone(), y);
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

        // Hotkey bar at bottom of window
        let hotkey_area = Rect { x: area.x + 1, y: area.y + area.height.saturating_sub(1), width: area.width.saturating_sub(2), height: 1 };
        let mut hotkey_text = "nav:[j|k|arrows]  ".to_string();
        hotkey_text.push_str(&hotkeys.format_window(
            "modules",
            &["start", "stop", "del", "auto", "copy", "creds", "edit", "clear", "test", "select", "popout"],
        ));
        let hotkeys_line = Line::from(vec![
            Span::styled(hotkey_text, Style::default().fg(Color::DarkGray)),
        ]);
        let hotkey_para = Paragraph::new(hotkeys_line);
        hotkey_para.render(hotkey_area, buf);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent, stats: &mut GlobalStats) -> Option<Action> {
        let module_count = stats.module_entries.len();
        if module_count == 0 {
            return None;
        }
        // Clamp selection if the module list shrank
        self.selected = self.selected.min(module_count.saturating_sub(1));
        self.scroll = self.scroll.min(module_count.saturating_sub(1));

        match key.code {
            crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => {
                self.selected = (self.selected + 1).min(module_count.saturating_sub(1));
                self.scroll = self.selected;
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.scroll = self.selected;
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('w') => Some(Action::PopOut("modules".to_string())),
            _ => None,
        }
    }

    fn start_config_editor(&mut self, module_name: &str, dir: PathBuf) {
        let rows = Self::load_rows(module_name, &dir);
        self.editing = Some(ConfigEditor {
            module_name: module_name.to_string(),
            dir,
            rows,
            selected: 0,
            scroll: 0,
            confirm_save: false,
        });
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
}
#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::ModulesWindow;
    use crate::app::Window;
    use crate::hotkeys::default_hotkeys;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
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
        w.start_config_editor("test-mod", tmp.clone());
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
        w.start_config_editor("test-mod", tmp.clone());

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
        w.start_config_editor("test-mod", tmp.clone());
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
        w.start_config_editor("test-mod", tmp.clone());
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
