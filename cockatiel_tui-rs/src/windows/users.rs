use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::app::{PendingPrompt, Window, WindowId};
use crate::colors::ColorConfig;
use crate::db::{GlobalStats, UserSummary, UserValue};
use crate::hotkeys::{Action, HotkeyConfig};

/// Which pane has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Detail,
}

/// A window-internal text-entry dialog (reason / duration / notes / delete
/// confirm). It consumes every key until Enter (submit) or Esc (cancel) —
/// deliberately NOT the engine prompt system, mirroring the config editor.
#[derive(Debug, Clone)]
struct Dialog {
    title: String,
    label: String,
    input: String,
    /// Target user for the action.
    uuid7: String,
    /// When set, the submitted text must equal this (delete confirmation).
    expected: Option<String>,
    mode: DialogMode,
}

#[derive(Debug, Clone)]
enum DialogMode {
    Commend,
    Reprimand,
    Ban,
    TimeoutDuration,
    TimeoutReason { duration_secs: i64 },
    Delete,
    Notes,
}

/// The detached pop-out user database view: a master/detail window over the
/// engine's user DB. The list is polled by the parent (`userdb_list_users`),
/// the detail/values/mutations travel as one-shot `Action::UserQuery` queries.
pub struct UsersWindow {
    /// Index into the filtered list.
    selected: usize,
    /// Scroll offset for the list pane.
    scroll: usize,
    /// List or detail focus.
    focus: Focus,
    /// Substring filter typed after `/`.
    filter: String,
    /// True while typing the filter (captures every key).
    filtering: bool,
    /// Active text-entry dialog.
    dialog: Option<Dialog>,
    /// Follow-up queries queued to fire on the next keypress (e.g. the values
    /// fetch after a selection, or the refresh after a mutation).
    pending_actions: VecDeque<Action>,
    /// True while a `userdb_get_user` for the selected user is in flight.
    detail_loading: bool,
    /// True while a `userdb_list_user_values` for the selected user is in flight.
    values_loading: bool,
    /// Last observed `user_detail_epoch` — a change means a fresh detail.
    detail_epoch_seen: u64,
    /// Last observed `user_values_epoch` — a change means fresh values.
    values_epoch_seen: u64,
}

impl UsersWindow {
    pub fn new() -> Self {
        Self {
            selected: 0,
            scroll: 0,
            focus: Focus::List,
            filter: String::new(),
            filtering: false,
            dialog: None,
            pending_actions: VecDeque::new(),
            detail_loading: false,
            values_loading: false,
            detail_epoch_seen: 0,
            values_epoch_seen: 0,
        }
    }

    // ── state helpers ───────────────────────────────────────────────

    fn filtered<'a>(&self, stats: &'a GlobalStats) -> Vec<&'a UserSummary> {
        if self.filter.is_empty() {
            stats.users.iter().collect()
        } else {
            let f = self.filter.to_lowercase();
            stats
                .users
                .iter()
                .filter(|u| u.username.to_lowercase().contains(&f))
                .collect()
        }
    }

    fn selected_user<'a>(&self, stats: &'a GlobalStats) -> Option<&'a UserSummary> {
        self.filtered(stats).get(self.selected).copied()
    }

    fn clamp_selection(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
            self.scroll = 0;
        } else {
            self.selected = self.selected.min(len - 1);
            self.scroll = self.scroll.min(len.saturating_sub(1));
        }
    }

    fn rank_color(&self, tier: &str) -> Color {
        match tier {
            "opal" => Color::LightCyan,
            "gold" => Color::Yellow,
            "silver" => Color::Gray,
            "coal" => Color::DarkGray,
            "trash" => Color::Red,
            _ => Color::White,
        }
    }

    fn role_badges(&self, u: &UserSummary) -> String {
        let mut s = String::new();
        if u.is_owner {
            s.push_str("OWN ");
        }
        if u.is_admin {
            s.push_str("ADM ");
        }
        if u.is_moderator {
            s.push_str("MOD ");
        }
        if u.is_sponsor {
            s.push_str("SPN ");
        }
        s
    }

    fn notes_value<'a>(&self, stats: &'a GlobalStats) -> Option<&'a UserValue> {
        stats.user_values.iter().find(|v| v.key == "notes")
    }

    // ── query builders ──────────────────────────────────────────────

    fn detail_action(&mut self, uuid7: &str, stats: &mut GlobalStats) -> Action {
        self.detail_loading = true;
        self.detail_epoch_seen = stats.user_detail_epoch;
        self.focus = Focus::Detail;
        let payload = serde_json::json!({ "uuid7": uuid7 });
        Action::UserQuery("userdb_get_user".to_string(), payload.to_string())
    }

    fn values_action(&mut self, uuid7: &str, stats: &mut GlobalStats) -> Action {
        self.values_loading = true;
        self.values_epoch_seen = stats.user_values_epoch;
        let payload = serde_json::json!({ "uuid7": uuid7 });
        Action::UserQuery("userdb_list_user_values".to_string(), payload.to_string())
    }

    /// Open a text-entry dialog targeting `user`.
    fn open_dialog(&mut self, mode: DialogMode, user: &UserSummary, label: String) {
        self.dialog = Some(Dialog {
            title: dialog_title(&mode).to_string(),
            label,
            input: String::new(),
            uuid7: user.uuid7.clone(),
            expected: match &mode {
                DialogMode::Delete => Some(user.username.clone()),
                _ => None,
            },
            mode,
        });
    }

    // ── key handling ────────────────────────────────────────────────

    fn filter_key(&mut self, key: KeyEvent, _stats: &mut GlobalStats) -> Option<Action> {
        match key.code {
            KeyCode::Enter => {
                // Commit the filter: keep it, exit typing mode.
                self.filtering = false;
                self.selected = 0;
                Some(Action::Noop)
            }
            KeyCode::Esc => {
                // Clear the filter entirely.
                self.filtering = false;
                self.filter.clear();
                self.selected = 0;
                Some(Action::Noop)
            }
            KeyCode::Char(c)
                if key.kind != KeyEventKind::Release
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                self.selected = 0;
                Some(Action::Noop)
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
                Some(Action::Noop)
            }
            _ => Some(Action::Noop),
        }
    }

    fn dialog_key(&mut self, key: KeyEvent, dialog: Dialog, stats: &mut GlobalStats) -> Option<Action> {
        let mut d = dialog;
        match key.code {
            KeyCode::Esc => {
                self.dialog = None;
                Some(Action::Noop)
            }
            KeyCode::Char(c)
                if key.kind != KeyEventKind::Release
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                match d.mode {
                    DialogMode::TimeoutDuration => {
                        if c.is_ascii_digit() {
                            d.input.push(c);
                        }
                    }
                    _ => d.input.push(c),
                }
                self.dialog = Some(d);
                Some(Action::Noop)
            }
            KeyCode::Backspace => {
                d.input.pop();
                self.dialog = Some(d);
                Some(Action::Noop)
            }
            KeyCode::Enter => {
                let uuid7 = d.uuid7.clone();
                let input = d.input.clone();
                // Delete requires typing the username to confirm.
                if matches!(d.mode, DialogMode::Delete)
                    && d.expected.as_deref() != Some(input.trim())
                {
                    self.dialog = Some(d);
                    return Some(Action::Noop);
                }
                let action = match d.mode {
                    DialogMode::Commend => Action::UserQuery(
                        "userdb_commendation".into(),
                        serde_json::json!({ "uuid7": uuid7, "reason": input }).to_string(),
                    ),
                    DialogMode::Reprimand => Action::UserQuery(
                        "userdb_reprimand".into(),
                        serde_json::json!({ "uuid7": uuid7, "reason": input }).to_string(),
                    ),
                    DialogMode::Ban => Action::UserQuery(
                        "userdb_ban".into(),
                        serde_json::json!({ "uuid7": uuid7, "reason": input }).to_string(),
                    ),
                    DialogMode::TimeoutDuration => {
                        let duration: i64 = input.trim().parse().unwrap_or(300).max(1);
                        // Phase 2: reason.
                        self.dialog = Some(Dialog {
                            title: "timeout".into(),
                            label: format!("reason ({}s timeout):", duration),
                            input: String::new(),
                            uuid7,
                            expected: None,
                            mode: DialogMode::TimeoutReason { duration_secs: duration },
                        });
                        return Some(Action::Noop);
                    }
                    DialogMode::TimeoutReason { duration_secs } => Action::UserQuery(
                        "userdb_timeout".into(),
                        serde_json::json!({
                            "uuid7": uuid7,
                            "duration_secs": duration_secs,
                            "reason": input,
                        })
                        .to_string(),
                    ),
                    DialogMode::Delete => Action::UserQuery(
                        "userdb_delete_user".into(),
                        serde_json::json!({
                            "uuid7": uuid7,
                            "actor_uuid7": "",
                            "actor_role": "owner",
                        })
                        .to_string(),
                    ),
                    DialogMode::Notes => Action::UserQuery(
                        "userdb_write_user_value".into(),
                        serde_json::json!({ "uuid7": uuid7, "key": "notes", "value": input }).to_string(),
                    ),
                };
                self.dialog = None;
                // Queue a refresh so the pane reflects the mutation.
                if !matches!(d.mode, DialogMode::Delete) {
                    self.queue_refresh(&uuid7, stats);
                }
                Some(action)
            }
            _ => {
                self.dialog = Some(d);
                Some(Action::Noop)
            }
        }
    }

    /// One queued refresh (detail + values) after a mutation: returns the
    /// get_user action to dispatch immediately and queues the values fetch.
    fn refresh_action(&mut self, uuid7: &str, stats: &mut GlobalStats) -> Action {
        self.detail_loading = true;
        self.detail_epoch_seen = stats.user_detail_epoch;
        self.values_loading = true;
        self.values_epoch_seen = stats.user_values_epoch;
        self.focus = Focus::Detail;
        Action::UserQuery("userdb_get_user".into(), serde_json::json!({ "uuid7": uuid7 }).to_string())
    }

    /// Queue a full refresh (get_user, then values) for the next keypresses.
    fn queue_refresh(&mut self, uuid7: &str, stats: &mut GlobalStats) {
        let detail = self.refresh_action(uuid7, stats);
        let values = self.values_action(uuid7, stats);
        self.pending_actions.push_back(detail);
        self.pending_actions.push_back(values);
    }

    // ── rendering ───────────────────────────────────────────────────

    fn render_list(&mut self, area: Rect, buf: &mut Buffer, stats: &GlobalStats, _colors: &ColorConfig) {
        let list = self.filtered(stats);
        let mut y = area.y;

        let header = Line::from(Span::styled(
            format!(" users:{} ", list.len()),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        header.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
        y += 1;

        if list.is_empty() {
            let line = Line::from(Span::styled(
                if stats.users.is_empty() {
                    "  (no users — waiting for user DB)"
                } else {
                    "  (no users match filter)"
                },
                Style::default().fg(Color::DarkGray),
            ));
            line.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
            return;
        }

        let visible = area.height.saturating_sub(1) as usize;
        let max_scroll = list.len().saturating_sub(visible);
        let scroll = self.scroll.min(max_scroll);
        let scroll = if self.selected >= scroll && self.selected < scroll + visible {
            scroll
        } else if self.selected < scroll {
            self.selected
        } else {
            self.selected.saturating_add(1).saturating_sub(visible)
        };
        self.scroll = scroll;

        for (i, user) in list.iter().skip(scroll).take(visible).enumerate() {
            if y >= area.y + area.height {
                break;
            }
            let idx = scroll + i;
            let is_selected = idx == self.selected && self.focus == Focus::List;
            let tier = user.rank_tier();
            let row_style = if is_selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::styled(
                    format!(" {:<20}", truncate(&user.username, 20)),
                    row_style.fg(if is_selected { Color::Black } else { self.rank_color(tier) }),
                ),
                Span::styled(
                    format!("{:>4}", user.score),
                    row_style.fg(if is_selected { Color::Black } else { Color::White }),
                ),
                Span::styled(
                    format!(" {:<5}", tier),
                    row_style.fg(if is_selected { Color::Black } else { Color::DarkGray }),
                ),
                Span::styled(
                    self.role_badges(user),
                    row_style.fg(if is_selected { Color::Black } else { Color::Yellow }),
                ),
            ]);
            line.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
            y += 1;
        }
    }

    fn render_detail(&self, area: Rect, buf: &mut Buffer, stats: &GlobalStats, _colors: &ColorConfig) {
        let mut y = area.y;
        let Some(selected) = self.selected_user(stats) else {
            let line = Line::from(Span::styled(
                "  (select a user)",
                Style::default().fg(Color::DarkGray),
            ));
            line.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
            return;
        };

        let tier = selected.rank_tier();
        let header = Line::from(vec![
            Span::styled(
                format!(" {}", selected.username),
                Style::default()
                    .fg(self.rank_color(tier))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  [{}]", tier),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("  {}", self.role_badges(selected)),
                Style::default().fg(Color::Yellow),
            ),
        ]);
        header.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
        y += 1;

        // Sync/loading indicator for the detail fetch.
        let detail_fresh = stats.user_detail.as_ref().map(|d| d.uuid7 == selected.uuid7).unwrap_or(false);
        let sync = if self.detail_loading || !detail_fresh {
            "detail loading…"
        } else {
            "detail synced"
        };
        let sync_color = if self.detail_loading || !detail_fresh {
            Color::Yellow
        } else {
            Color::Green
        };
        let line = Line::from(vec![
            Span::styled(format!("  {}  ", sync), Style::default().fg(sync_color)),
            Span::styled(
                truncate(&selected.uuid7, 36),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        line.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
        y += 1;

        let score_line = Line::from(vec![
            Span::styled(
                format!("  score {}", selected.score),
                Style::default().fg(self.rank_color(tier)).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "   +{} commended  -{} reprimanded",
                    selected.commendations, selected.reprimands
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        score_line.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
        y += 1;

        let roles = [
            ("1", "owner", selected.is_owner),
            ("2", "admin", selected.is_admin),
            ("3", "moderator", selected.is_moderator),
            ("4", "sponsor", selected.is_sponsor),
        ];
        let role_line = Line::from(
            roles
                .iter()
                .map(|(k, name, on)| {
                    let boxed = format!("[{}]", if *on { "x" } else { " " });
                    Span::styled(
                        format!(" {} {} {}", k, boxed, name),
                        if *on {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        },
                    )
                })
                .collect::<Vec<_>>(),
        );
        role_line.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
        y += 1;

        // Channels
        let chans: Vec<String> = selected.channels.clone();
        let chan_label = Line::from(Span::styled(
            if chans.is_empty() {
                "  channels: (none)".to_string()
            } else {
                format!("  channels: {}", chans.join(", "))
            },
            Style::default().fg(Color::DarkGray),
        ));
        chan_label.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
        y += 1;

        // Flags
        let flags_line = Line::from(vec![
            Span::styled("  flags: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate(&selected.flags, (area.width as usize).saturating_sub(10)),
                Style::default().fg(Color::White),
            ),
        ]);
        flags_line.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
        y += 1;

        // Values
        let values_header = Line::from(Span::styled(
            if self.values_loading {
                "  values loading…".to_string()
            } else {
                "  values".to_string()
            },
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        values_header.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
        y += 1;
        if stats.user_values.is_empty() && !self.values_loading {
            let line = Line::from(Span::styled(
                "    (none — Enter/v to load values)",
                Style::default().fg(Color::DarkGray),
            ));
            line.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
            y += 1;
        }
        for v in &stats.user_values {
            if y >= area.y + area.height {
                break;
            }
            self.render_value_line(v, Rect { x: area.x, y, width: area.width, height: 1 }, buf);
            y += 1;
        }

        // Notes (from the "notes" value).
        if let Some(notes) = self.notes_value(stats) {
            if y < area.y + area.height {
                let note_line = Line::from(vec![
                    Span::styled("  notes: ", Style::default().fg(Color::Cyan)),
                    Span::styled(
                        truncate(&notes.value, (area.width as usize).saturating_sub(10)),
                        Style::default().fg(Color::White),
                    ),
                ]);
                note_line.render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
            }
        }
    }

    fn render_value_line(&self, v: &UserValue, area: Rect, buf: &mut Buffer) {
        // Render known JSON-shaped values (ban/timeout) as key fields.
        let text = match v.key.as_str() {
            "ban" | "timeout" => {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&v.value) {
                    let mut parts = Vec::new();
                    if let Some(b) = val.get("banned") {
                        parts.push(format!("banned={}", b));
                    }
                    if let Some(ms) = val.get("expires_at_ms").and_then(|x| x.as_i64()) {
                        parts.push(format!("expires={}s", ms.saturating_sub(now_ms()) / 1000));
                    }
                    if let Some(secs) = val.get("duration_secs").and_then(|x| x.as_i64()) {
                        parts.push(format!("{}s", secs));
                    }
                    if let Some(r) = val.get("reason").and_then(|x| x.as_str()) {
                        parts.push(format!("reason={}", r));
                    }
                    if parts.is_empty() {
                        v.value.clone()
                    } else {
                        parts.join(", ")
                    }
                } else {
                    v.value.clone()
                }
            }
            _ => v.value.clone(),
        };
        let line = Line::from(vec![
            Span::styled(format!("    {:<12}", v.key), Style::default().fg(Color::Yellow)),
            Span::styled(text, Style::default().fg(Color::White)),
        ]);
        line.render(area, buf);
    }

    fn render_dialog(&self, area: Rect, buf: &mut Buffer, colors: &ColorConfig) {
        let Some(dialog) = &self.dialog else { return };
        let w = 60u16.min(area.width.saturating_sub(4));
        let h = 6u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let rect = Rect { x, y, width: w, height: h };

        let block = Block::default()
            .title(format!(" {} ", dialog.title))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(colors.active_border_color("users")));
        let inner = rect.inner(ratatui::layout::Margin { horizontal: 1, vertical: 1 });

        let label = Line::from(Span::styled(
            &dialog.label,
            Style::default().fg(Color::Cyan),
        ));
        label.render(Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 }, buf);

        let input = Line::from(Span::styled(
            format!("> {}", dialog.input),
            Style::default().fg(Color::White),
        ));
        input.render(Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: 1 }, buf);

        let hint = if dialog.expected.is_some() {
            "type the username to confirm, Esc to cancel"
        } else {
            "Enter to submit, Esc to cancel"
        };
        let hint = Line::from(Span::styled(
            format!("  {}", hint),
            Style::default().fg(Color::DarkGray),
        ));
        hint.render(Rect { x: inner.x, y: inner.y + 2, width: inner.width, height: 1 }, buf);

        block.render(rect, buf);
    }
}

fn dialog_title(mode: &DialogMode) -> &'static str {
    match mode {
        DialogMode::Commend => "commend",
        DialogMode::Reprimand => "reprimand",
        DialogMode::Ban => "ban",
        DialogMode::TimeoutDuration => "timeout",
        DialogMode::TimeoutReason { .. } => "timeout",
        DialogMode::Delete => "delete user",
        DialogMode::Notes => "notes",
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

impl Window for UsersWindow {
    fn id(&self) -> WindowId {
        WindowId::Users
    }

    fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        is_active: bool,
        stats: &GlobalStats,
        colors: &ColorConfig,
        _hotkeys: &HotkeyConfig,
        _prompts: &[PendingPrompt],
    ) {
        // Refresh loading flags from the epoch counters in the stats.
        if self.detail_epoch_seen != stats.user_detail_epoch {
            self.detail_epoch_seen = stats.user_detail_epoch;
            self.detail_loading = false;
        }
        if self.values_epoch_seen != stats.user_values_epoch {
            self.values_epoch_seen = stats.user_values_epoch;
            self.values_loading = false;
        }

        let list = self.filtered(stats);
        self.clamp_selection(list.len());

        let border_color = if is_active {
            colors.active_border_color("users")
        } else {
            colors.border_color("inactive")
        };
        let block = Block::default()
            .title(" users ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = area.inner(ratatui::layout::Margin { horizontal: 1, vertical: 1 });
        let content_height = inner.height.saturating_sub(2); // hotkey + filter/error rows

        let list_width = (((inner.width as f64) * 0.45) as u16)
            .max(24)
            .min(inner.width.saturating_sub(10));
        let list_area = Rect { x: inner.x, y: inner.y, width: list_width, height: content_height };
        let detail_area = Rect {
            x: inner.x + list_width,
            y: inner.y,
            width: inner.width - list_width,
            height: content_height,
        };

        self.render_list(list_area, buf, stats, colors);
        self.render_detail(detail_area, buf, stats, colors);

        // Filter line / error line (last two rows inside the border).
        if self.filtering {
            let filter_line = Line::from(vec![
                Span::styled(" filter: ", Style::default().fg(Color::Cyan)),
                Span::styled(&self.filter, Style::default().fg(Color::White)),
            ]);
            filter_line.render(
                Rect { x: inner.x, y: inner.y + inner.height.saturating_sub(2), width: inner.width, height: 1 },
                buf,
            );
        }
        if let Some(err) = &stats.user_last_error {
            let err_line = Line::from(Span::styled(
                format!("  userdb: {}", truncate(err, inner.width as usize)),
                Style::default().fg(Color::LightRed),
            ));
            err_line.render(
                Rect { x: inner.x, y: inner.y + inner.height.saturating_sub(1), width: inner.width, height: 1 },
                buf,
            );
        }

        block.render(area, buf);

        // Hotkey bar on the bottom border row (interior), like modules.rs.
        let hotkey_area = Rect {
            x: area.x + 1,
            y: area.y + area.height.saturating_sub(1),
            width: area.width.saturating_sub(2),
            height: 1,
        };
        let mut hotkey_text = "nav:[j|k|g|G]  filter:[/]  open:[Enter]  pane:[Tab]  ".to_string();
        if self.focus == Focus::Detail {
            hotkey_text.push_str("commend:[c]  reprimand:[r]  ban:[b]  timeout:[t]  roles:[1-4]  notes:[n]  del:[d]  values:[v]  ");
        }
        hotkey_text.push_str("quit:[q|esc]");
        let hotkeys_line = Line::from(vec![Span::styled(hotkey_text, Style::default().fg(Color::DarkGray))]);
        Paragraph::new(hotkeys_line).render(hotkey_area, buf);

        self.render_dialog(area, buf, colors);
    }

    fn handle_key(&mut self, key: KeyEvent, stats: &mut GlobalStats) -> Option<Action> {
        // Drain queued follow-up queries first (refresh after a selection or
        // mutation). The next keypress after such an action carries it out.
        if let Some(action) = self.pending_actions.pop_front() {
            return Some(action);
        }
        // An active dialog captures every key.
        if let Some(dialog) = self.dialog.take() {
            return self.dialog_key(key, dialog, stats);
        }
        // An active filter captures every key.
        if self.filtering {
            return self.filter_key(key, stats);
        }

        let list = self.filtered(stats);
        self.clamp_selection(list.len());
        let selected_uuid = self.selected_user(stats).map(|u| u.uuid7.clone());

        match key.code {
            KeyCode::Esc => Some(Action::Quit),
            KeyCode::Char('j') | KeyCode::Down => {
                if !list.is_empty() {
                    self.selected = (self.selected + 1).min(list.len() - 1);
                }
                Some(Action::Noop)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                Some(Action::Noop)
            }
            KeyCode::Char('g') => {
                self.selected = 0;
                Some(Action::Noop)
            }
            KeyCode::Char('G') => {
                self.selected = list.len().saturating_sub(1);
                Some(Action::Noop)
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + 10).min(list.len().saturating_sub(1));
                Some(Action::Noop)
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(10);
                Some(Action::Noop)
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
                Some(Action::Noop)
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let Some(uuid) = selected_uuid else {
                    return Some(Action::Noop);
                };
                if self.focus == Focus::List {
                    self.focus = Focus::Detail;
                    let fresh = stats
                        .user_detail
                        .as_ref()
                        .map(|d| d.uuid7 == uuid)
                        .unwrap_or(false);
                    if fresh {
                        Some(self.values_action(&uuid, stats))
                    } else {
                        Some(self.detail_action(&uuid, stats))
                    }
                } else {
                    self.focus = Focus::List;
                    Some(Action::Noop)
                }
            }
            KeyCode::Enter => {
                let Some(uuid) = selected_uuid else {
                    return Some(Action::Noop);
                };
                if self.focus == Focus::List {
                    // Open the detail; the queued values fetch follows.
                    let detail = self.detail_action(&uuid, stats);
                    let values = self.values_action(&uuid, stats);
                    self.pending_actions.push_back(values);
                    Some(detail)
                } else {
                    // In the detail: (re)load values + detail.
                    Some(self.refresh_action(&uuid, stats))
                }
            }
            KeyCode::Char('v') if self.focus == Focus::Detail => {
                selected_uuid.as_ref().map(|uuid| self.values_action(uuid, stats))
            }
            KeyCode::Char('1') | KeyCode::Char('2') | KeyCode::Char('3') | KeyCode::Char('4')
                if self.focus == Focus::Detail =>
            {
                let Some(uuid) = selected_uuid else { return Some(Action::Noop) };
                Some(self.toggle_role_action(key.code, &uuid, stats))
            }
            KeyCode::Char('c') if self.focus == Focus::Detail => {
                self.selected_user(stats).map(|u| {
                    self.open_dialog(
                        DialogMode::Commend,
                        u,
                        "commendation reason (optional):".to_string(),
                    );
                    Action::Noop
                })
            }
            KeyCode::Char('r') if self.focus == Focus::Detail => {
                self.selected_user(stats).map(|u| {
                    self.open_dialog(
                        DialogMode::Reprimand,
                        u,
                        "reprimand reason (optional):".to_string(),
                    );
                    Action::Noop
                })
            }
            KeyCode::Char('b') if self.focus == Focus::Detail => {
                self.selected_user(stats).map(|u| {
                    self.open_dialog(DialogMode::Ban, u, "ban reason (optional):".to_string());
                    Action::Noop
                })
            }
            KeyCode::Char('t') if self.focus == Focus::Detail => {
                self.selected_user(stats).map(|u| {
                    self.open_dialog(DialogMode::TimeoutDuration, u, "timeout duration in seconds:".to_string());
                    Action::Noop
                })
            }
            KeyCode::Char('n') if self.focus == Focus::Detail => {
                let (user, notes) = {
                    let user = self.selected_user(stats)?.clone();
                    let notes = self.notes_value(stats).map(|v| v.value.clone()).unwrap_or_default();
                    (user, notes)
                };
                self.dialog = Some(Dialog {
                    title: "notes".to_string(),
                    label: format!("notes for {}:", user.username),
                    input: notes,
                    uuid7: user.uuid7.clone(),
                    expected: None,
                    mode: DialogMode::Notes,
                });
                Some(Action::Noop)
            }
            KeyCode::Char('d') if self.focus == Focus::Detail => {
                self.selected_user(stats).map(|u| {
                    self.open_dialog(
                        DialogMode::Delete,
                        u,
                        format!("type '{}' to permanently delete:", u.username),
                    );
                    Action::Noop
                })
            }
            _ => None,
        }
    }

    fn selected_module_name(&self, _stats: &GlobalStats) -> Option<String> {
        None
    }
}

impl UsersWindow {
    fn toggle_role_action(&self, code: KeyCode, uuid7: &str, stats: &GlobalStats) -> Action {
        let cur = stats
            .user_detail
            .as_ref()
            .filter(|d| d.uuid7 == uuid7)
            .or_else(|| stats.users.iter().find(|u| u.uuid7 == uuid7));
        let (sp, mo, ad, ow) = match cur {
            Some(u) => (u.is_sponsor, u.is_moderator, u.is_admin, u.is_owner),
            None => (false, false, false, false),
        };
        let (sp, mo, ad, ow) = match code {
            KeyCode::Char('1') => (!sp, mo, ad, ow),
            KeyCode::Char('2') => (sp, !mo, ad, ow),
            KeyCode::Char('3') => (sp, mo, !ad, ow),
            KeyCode::Char('4') => (sp, mo, ad, !ow),
            _ => (sp, mo, ad, ow),
        };
        let payload = serde_json::json!({
            "uuid7": uuid7,
            "actor_uuid7": "",
            "actor_role": "owner",
            "is_sponsor": sp,
            "is_moderator": mo,
            "is_admin": ad,
            "is_owner": ow,
        });
        Action::UserQuery("userdb_set_roles".to_string(), payload.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Window;
    use crate::colors::default_colors;
    use crate::db::rank_tier;
    use crate::hotkeys::default_hotkeys;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }

    fn user(uuid: &str, name: &str, score: i64) -> UserSummary {
        UserSummary {
            uuid7: uuid.to_string(),
            username: name.to_string(),
            is_sponsor: false,
            is_moderator: false,
            is_admin: false,
            is_owner: false,
            score,
            commendations: 0,
            reprimands: 0,
            channels: Vec::new(),
            flags: "{}".to_string(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn stats_with_users() -> GlobalStats {
        let mut s = GlobalStats::default();
        s.users = vec![
            user("00000000-0000-7000-0000-000000000001", "alice", 60),
            user("00000000-0000-7000-0000-000000000002", "bob", 25),
            user("00000000-0000-7000-0000-000000000003", "carol", -8),
            user("00000000-0000-7000-0000-000000000004", "dave", 3),
        ];
        s
    }

    #[test]
    fn rank_tier_boundaries() {
        assert_eq!(rank_tier(60), "opal");
        assert_eq!(rank_tier(50), "opal");
        assert_eq!(rank_tier(49), "gold");
        assert_eq!(rank_tier(20), "gold");
        assert_eq!(rank_tier(19), "silver");
        assert_eq!(rank_tier(5), "silver");
        assert_eq!(rank_tier(4), "neutral");
        assert_eq!(rank_tier(-4), "neutral");
        assert_eq!(rank_tier(-5), "coal");
        assert_eq!(rank_tier(-19), "coal");
        assert_eq!(rank_tier(-20), "trash");
        assert_eq!(rank_tier(-100), "trash");
    }

    #[test]
    fn navigation_and_enter_issue_detail_query() {
        let mut w = UsersWindow::new();
        let mut stats = stats_with_users();

        // j moves down; selection starts at 0.
        assert_eq!(w.handle_key(key('j'), &mut stats), Some(Action::Noop));
        assert_eq!(w.selected, 1);

        // k moves up.
        assert_eq!(w.handle_key(key('k'), &mut stats), Some(Action::Noop));
        assert_eq!(w.selected, 0);

        // G jumps to the last row.
        assert_eq!(w.handle_key(key('G'), &mut stats), Some(Action::Noop));
        assert_eq!(w.selected, 3);

        // Enter on the last row opens the detail and issues a get_user query.
        let action = w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), &mut stats);
        match action {
            Some(Action::UserQuery(qid, sql)) => {
                assert_eq!(qid, "userdb_get_user");
                assert!(sql.contains("00000000-0000-7000-0000-000000000004"));
            }
            other => panic!("expected get_user, got {:?}", other),
        }
        assert_eq!(w.focus, Focus::Detail);

        // The next keypress carries the queued values fetch.
        let action = w.handle_key(key('j'), &mut stats);
        match action {
            Some(Action::UserQuery(qid, _)) => assert_eq!(qid, "userdb_list_user_values"),
            other => panic!("expected list_user_values, got {:?}", other),
        }
    }

    #[test]
    fn filter_restricts_list() {
        let mut w = UsersWindow::new();
        let mut stats = stats_with_users();

        // Start a filter, type "bo".
        assert_eq!(w.handle_key(key('/'), &mut stats), Some(Action::Noop));
        assert!(w.filtering);
        assert_eq!(w.handle_key(key('b'), &mut stats), Some(Action::Noop));
        assert_eq!(w.handle_key(key('o'), &mut stats), Some(Action::Noop));

        let filtered = w.filtered(&stats);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].username, "bob");

        // Enter commits the filter.
        assert_eq!(w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), &mut stats), Some(Action::Noop));
        assert!(!w.filtering);
        assert_eq!(w.selected_user(&stats).map(|u| u.username.as_str()), Some("bob"));

        // Esc clears the filter.
        assert_eq!(w.handle_key(key('/'), &mut stats), Some(Action::Noop));
        assert_eq!(w.handle_key(key('q'), &mut stats), Some(Action::Noop));
        assert_eq!(w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()), &mut stats), Some(Action::Noop));
        assert!(w.filter.is_empty());
        assert!(!w.filtering);
        assert_eq!(w.filtered(&stats).len(), 4);
    }

    #[test]
    fn dialog_submits_ban_query_and_refresh() {
        let mut w = UsersWindow::new();
        let mut stats = stats_with_users();
        w.focus = Focus::Detail;

        // Open the ban dialog.
        assert_eq!(w.handle_key(key('b'), &mut stats), Some(Action::Noop));
        assert!(w.dialog.is_some());

        // Type a reason and submit.
        for c in "spam".chars() {
            assert_eq!(w.handle_key(key(c), &mut stats), Some(Action::Noop));
        }
        let action = w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), &mut stats);
        match action {
            Some(Action::UserQuery(qid, sql)) => {
                assert_eq!(qid, "userdb_ban");
                let v: serde_json::Value = serde_json::from_str(&sql).unwrap();
                assert_eq!(v["reason"], "spam");
                assert_eq!(v["uuid7"], "00000000-0000-7000-0000-000000000001");
            }
            other => panic!("expected ban, got {:?}", other),
        }
        assert!(w.dialog.is_none());

        // The next keypress carries the queued refresh (get_user first).
        let action = w.handle_key(key('j'), &mut stats);
        match action {
            Some(Action::UserQuery(qid, _)) => assert_eq!(qid, "userdb_get_user"),
            other => panic!("expected refresh get_user, got {:?}", other),
        }
    }

    #[test]
    fn delete_requires_username_confirm() {
        let mut w = UsersWindow::new();
        let mut stats = stats_with_users();
        w.focus = Focus::Detail;

        assert_eq!(w.handle_key(key('d'), &mut stats), Some(Action::Noop));
        // Wrong name keeps the dialog open.
        for c in "bob".chars() {
            w.handle_key(key(c), &mut stats);
        }
        let action = w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), &mut stats);
        assert_eq!(action, Some(Action::Noop));
        assert!(w.dialog.is_some(), "mismatched delete confirm should keep dialog");

        // Clear + type the right name.
        while !w.dialog.as_ref().unwrap().input.is_empty() {
            w.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()), &mut stats);
        }
        for c in "alice".chars() {
            w.handle_key(key(c), &mut stats);
        }
        let action = w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), &mut stats);
        match action {
            Some(Action::UserQuery(qid, _)) => assert_eq!(qid, "userdb_delete_user"),
            other => panic!("expected delete_user, got {:?}", other),
        }
        assert!(w.dialog.is_none());
    }

    #[test]
    fn render_draws_list_and_detail() {
        let mut w = UsersWindow::new();
        let mut stats = stats_with_users();
        let colors = default_colors();
        let hotkeys = default_hotkeys();

        let mut buf = Buffer::empty(Rect::new(0, 0, 140, 40));
        w.render(Rect::new(0, 0, 140, 40), &mut buf, true, &stats, &colors, &hotkeys, &[]);

        let mut all = String::new();
        for y in 0..40u16 {
            for x in 0..140u16 {
                all.push_str(buf[(x, y)].symbol());
            }
            all.push('\n');
        }
        assert!(all.contains("alice"), "list missing alice:\n{}", all);
        assert!(all.contains("users:"), "header missing:\n{}", all);
        assert!(all.contains("score 60"), "detail score missing:\n{}", all);

        // Open a detail and render again.
        w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), &mut stats);
        w.pending_actions.clear();
        stats.user_detail = Some(user("00000000-0000-7000-0000-000000000001", "alice", 60));
        stats.user_detail_epoch = stats.user_detail_epoch.wrapping_add(1);
        stats.user_values = vec![crate::db::UserValue {
            key: "notes".to_string(),
            value: "follow up next week".to_string(),
        }];
        w.render(Rect::new(0, 0, 140, 40), &mut buf, true, &stats, &colors, &hotkeys, &[]);
        let mut all = String::new();
        for y in 0..40u16 {
            for x in 0..140u16 {
                all.push_str(buf[(x, y)].symbol());
            }
            all.push('\n');
        }
        assert!(all.contains("score 60"), "detail score missing:\n{}", all);
        assert!(all.contains("notes"), "values missing:\n{}", all);
        assert!(all.contains("follow up next week"), "notes text missing:\n{}", all);
    }
}