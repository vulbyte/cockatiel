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

/// Dedicated prompt queue window: shows how many prompts are waiting and the
/// dialog for the currently-selected one. Left/right arrows cycle the queue;
/// y/n (or typed text + Enter) answers it. When no prompt is focused the rest
/// of the TUI stays fully navigable.
pub struct PromptsWindow {
    /// Index into the pending-prompt queue that this window highlights.
    pub selected: usize,
    /// Clickable region of the active prompt's link, set during render.
    pub link_rect: Option<Rect>,
    pub link_url: Option<String>,
}

impl PromptsWindow {
    pub fn new() -> Self {
        Self {
            selected: 0,
            link_rect: None,
            link_url: None,
        }
    }

    /// Draw the active prompt as a dialog filling the window body.
    fn draw_prompt_dialog(&mut self, area: Rect, buf: &mut Buffer, pending: &PendingPrompt) {
        let p = &pending.prompt;
        let yes = if p.yes_dialog.is_empty() {
            "Allow".to_string()
        } else {
            p.yes_dialog.clone()
        };
        let no = if p.no_dialog.is_empty() {
            "Deny".to_string()
        } else {
            p.no_dialog.clone()
        };
        let secs = pending
            .deadline
            .saturating_duration_since(Instant::now())
            .as_secs();

        let inner = area.inner(ratatui::layout::Margin { horizontal: 1, vertical: 1 });
        let max_text_w = (inner.width.saturating_sub(2) as usize).max(12);
        let mut lines: Vec<String> = Vec::new();
        if !p.origin.is_empty() {
            lines.push(format!("  from: {}", p.origin));
            lines.push(String::new());
        }
        if !p.details.is_empty() {
            for l in p.details.lines() {
                lines.extend(wrap_text(l, max_text_w));
            }
            lines.push(String::new());
        }
        if !p.instructions.is_empty() {
            for l in p.instructions.lines() {
                for wrapped in wrap_text(l, max_text_w) {
                    lines.push(format!("  {}", wrapped));
                }
            }
            lines.push(String::new());
        }
        // Free-text / credential prompts: show the input field (masked for
        // credentials so the secret never leaks on screen). Boolean prompts
        // show the y/n choices instead.
        let kind = p.kind();
        if kind == cockatiel_client::PromptKind::Boolean {
            lines.push(format!(" {} (y) / {} (n)", yes, no));
        } else {
            let label = if p.input_label.is_empty() {
                "Input".to_string()
            } else {
                p.input_label.clone()
            };
            let shown = if kind == cockatiel_client::PromptKind::Credential {
                "•".repeat(pending.text_input.chars().count())
            } else {
                pending.text_input.clone()
            };
            lines.push(format!(" {}: {}", label, shown));
            lines.push(String::new());
            if kind == cockatiel_client::PromptKind::Credential {
                lines.push(" type or paste (masked), enter: submit   esc: cancel".to_string());
            } else {
                lines.push(" type text or number, paste to fill, enter: submit   esc: cancel".to_string());
            }
        }
        lines.push(format!(" seconds remaining: {}", secs));
        lines.push(String::new());
        lines.push(" < / > : cycle prompts".to_string());

        let mut y_cursor = inner.y;
        for line in &lines {
            if y_cursor >= inner.y + inner.height {
                break;
            }
            let text = Line::from(Span::styled(line.as_str(), Style::default().fg(Color::White)));
            text.render(Rect { x: inner.x, y: y_cursor, width: inner.width, height: 1 }, buf);
            y_cursor += 1;
        }

        // Clickable link row (if the prompt carried one).
        if !p.link.is_empty() && y_cursor < inner.y + inner.height {
            let spans = vec![
                Span::styled(
                    "[open]",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ),
                Span::styled(
                    format!("  {}", p.link),
                    Style::default()
                        .fg(Color::LightBlue)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ];
            let line = Line::from(spans);
            line.render(Rect { x: inner.x, y: y_cursor, width: inner.width, height: 1 }, buf);
            self.link_rect = Some(Rect { x: inner.x, y: y_cursor, width: inner.width, height: 1 });
            self.link_url = Some(p.link.clone());
        } else {
            self.link_rect = None;
            self.link_url = None;
        }
    }
}

/// Wrap `text` to at most `max_w` characters per line (word-aware-ish).
fn wrap_text(text: &str, max_w: usize) -> Vec<String> {
    let max_w = max_w.max(8);
    let mut out = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{} {}", current, word)
        };
        if candidate.chars().count() > max_w && !current.is_empty() {
            out.push(current);
            current = word.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

impl Window for PromptsWindow {
    fn id(&self) -> WindowId {
        WindowId::Prompts
    }

    fn set_prompt_selected(&mut self, idx: usize) {
        self.selected = idx;
    }

    fn pending_link(&self) -> Option<(Rect, String)> {
        match (&self.link_rect, &self.link_url) {
            (Some(rect), Some(url)) => Some((*rect, url.clone())),
            _ => None,
        }
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, is_active: bool, _stats: &GlobalStats, colors: &ColorConfig, hotkeys: &HotkeyConfig, prompts: &[PendingPrompt]) {
        let pending_count = prompts.len();
        // Border states: empty+focused = green, empty+unfocused = gray,
        // queue+focused = orange, queue+unfocused = orange-red (blinking ~1/s).
        let mut border_style = match (pending_count, is_active) {
            (0, true) => Style::default().fg(Color::Green),
            (0, false) => Style::default().fg(Color::DarkGray),
            (_, true) => Style::default().fg(Color::Indexed(208)), // orange
            (_, false) => Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::SLOW_BLINK), // orange-red blink
        };
        // If a custom border color for "prompts" is configured, prefer it for
        // the non-blink states; the blink/attention states stay as above.
        if pending_count == 0 && !is_active {
            border_style = Style::default().fg(colors.border_color("inactive"));
        }

        let title = if pending_count > 0 {
            format!(" prompts [{}/{}] ", self.selected.min(pending_count.saturating_sub(1)) + 1, pending_count)
        } else {
            " prompts ".to_string()
        };

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(border_style);
        let inner = area.inner(ratatui::layout::Margin { horizontal: 1, vertical: 1 });

        if pending_count == 0 {
            let text = Paragraph::new(Line::from(Span::styled(
                "No pending prompts",
                Style::default().fg(Color::DarkGray),
            )));
            text.render(inner, buf);
        } else {
            let idx = self.selected.min(pending_count.saturating_sub(1));
            self.draw_prompt_dialog(area, buf, &prompts[idx]);
        }

        // Hotkey bar at bottom of window.
        let hotkey_area = Rect { x: area.x + 1, y: area.y + area.height.saturating_sub(1), width: area.width.saturating_sub(2), height: 1 };
        let mut hotkey_text = "nav:[<|>] ".to_string();
        hotkey_text.push_str(&hotkeys.format_window("prompts", &[]));
        hotkey_text.push_str("type/paste answer  enter submit  esc cancel  double-esc: quit");
        let hotkeys_line = Line::from(vec![
            Span::styled(hotkey_text, Style::default().fg(Color::DarkGray)),
        ]);
        let hotkey_para = Paragraph::new(hotkeys_line);
        hotkey_para.render(hotkey_area, buf);

        block.render(area, buf);
    }

    fn handle_key(&mut self, _key: crossterm::event::KeyEvent, _stats: &mut GlobalStats) -> Option<Action> {
        // Prompt answering/cycling is handled by the app layer (it owns the
        // prompt queue), so the window itself has no key actions.
        None
    }
}