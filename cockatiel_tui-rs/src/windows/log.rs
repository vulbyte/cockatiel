use std::collections::VecDeque;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::app::{Window, WindowId};
use crate::colors::ColorConfig;
use crate::db::GlobalStats;
use crate::hotkeys::Action;

#[derive(Clone)]
pub struct LogEntry {
    #[allow(dead_code)]
    pub timestamp: String,
    pub source: String,
    pub message: String,
    pub event_type: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogFilter {
    All,
    Logs,
    Errors,
    System,
    Messages,
}

impl LogFilter {
    pub fn label(&self) -> &'static str {
        match self {
            LogFilter::All => "All",
            LogFilter::Logs => "Logs",
            LogFilter::Errors => "Errors",
            LogFilter::System => "System",
            LogFilter::Messages => "Messages",
        }
    }

    pub fn all() -> &'static [LogFilter] {
        &[LogFilter::All, LogFilter::Logs, LogFilter::Errors, LogFilter::System, LogFilter::Messages]
    }

    fn matches(&self, entry: &LogEntry) -> bool {
        match self {
            LogFilter::All => true,
            LogFilter::Logs => entry.event_type == 1,
            LogFilter::Errors => entry.event_type == 3,
            LogFilter::System => entry.event_type == 4,
            LogFilter::Messages => entry.event_type == 5,
        }
    }
}

pub struct LogWindow {
    pub entries: VecDeque<LogEntry>,
    pub filter: LogFilter,
    pub max_entries: usize,
    pub scroll: usize,
}

impl LogWindow {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            filter: LogFilter::All,
            max_entries: 200,
            scroll: 0,
        }
    }

    pub fn push(&mut self, entry: LogEntry) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    fn filtered_entries(&self) -> Vec<&LogEntry> {
        self.entries.iter().filter(|e| self.filter.matches(e)).collect()
    }

    fn tab_bar(&self) -> Line<'_> {
        let mut spans = vec![Span::styled("  ", Style::default())];
        for f in LogFilter::all().iter() {
            let is_active = *f == self.filter;
            let style = if is_active {
                Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(format!("[{}] ", f.label()), style));
        }
        Line::from(spans)
    }
}

impl Window for LogWindow {
    fn id(&self) -> WindowId {
        WindowId::Log
    }

    fn push_log(&mut self, entry: LogEntry) {
        self.push(entry);
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, is_active: bool, _stats: &GlobalStats, colors: &ColorConfig, _hotkeys: &crate::hotkeys::HotkeyConfig, _prompts: &[crate::app::PendingPrompt]) {
        let border_color = if is_active {
            colors.active_border_color("log")
        } else {
            colors.border_color("inactive")
        };

        let block = Block::default()
            .title(" log ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = area.inner(ratatui::layout::Margin { horizontal: 1, vertical: 1 });

        // Hotkey bar: wrapped to this window's width, so hints are no longer
        // clipped off the right edge. The two-tone `key:label` styling is kept by
        // passing the runs as separate chunks.
        let key_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        let desc_style = Style::default().fg(Color::DarkGray);
        let hotkey = crate::windows::hotkey_wrap::layout(
            &[
                ("1-5".to_string(), key_style),
                (":filter".to_string(), desc_style),
                ("j/k".to_string(), key_style),
                (":scroll".to_string(), desc_style),
                ("w".to_string(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            ],
            area,
            inner,
        );
        let inner = hotkey.content;

        // Tab bar
        let tab_area = Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 };
        let tab_para = Paragraph::new(self.tab_bar());
        tab_para.render(tab_area, buf);

        // Log entries
        let log_area = Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: inner.height.saturating_sub(1) };

        let filtered = self.filtered_entries();
        let mut lines: Vec<Line> = Vec::new();

        // Render from the end, honoring scroll offset (scroll=0 = newest at bottom).
        let visible = log_area.height as usize;
        let total = filtered.len();
        let scroll = self.scroll.min(total.saturating_sub(1));

        let start = total.saturating_sub(scroll + visible);
        for entry in filtered.iter().skip(start).take(visible) {
            let type_color = match entry.event_type {
                1 => Color::DarkGray,
                2 => Color::Yellow,
                3 => Color::Red,
                4 => Color::Cyan,
                5 => Color::Green,
                _ => Color::DarkGray,
            };

            lines.push(Line::from(vec![
                Span::styled(format!("[{}] ", entry.source), Style::default().fg(type_color)),
                Span::styled(&entry.message, Style::default().fg(Color::Gray)),
            ]));
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::styled("  (no entries)", Style::default().fg(Color::DarkGray))));
        }

        let log_para = Paragraph::new(lines);
        log_para.render(log_area, buf);

        // Render border on top
        block.render(area, buf);

        // Hotkey bar (already wrapped above).
        Paragraph::new(hotkey.lines).render(hotkey.area, buf);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent, _stats: &mut GlobalStats) -> Option<Action> {
        match key.code {
            crossterm::event::KeyCode::Char('1') => { self.filter = LogFilter::All; Some(Action::Noop) }
            crossterm::event::KeyCode::Char('2') => { self.filter = LogFilter::Logs; Some(Action::Noop) }
            crossterm::event::KeyCode::Char('3') => { self.filter = LogFilter::Errors; Some(Action::Noop) }
            crossterm::event::KeyCode::Char('4') => { self.filter = LogFilter::System; Some(Action::Noop) }
            crossterm::event::KeyCode::Char('5') => { self.filter = LogFilter::Messages; Some(Action::Noop) }
            crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('w') => Some(Action::PopOut("log".to_string())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod log_bounds_tests {
    use super::*;
    use crate::hotkeys::default_hotkeys;
    use crate::layout::LayoutState;
    use ratatui::buffer::Buffer;

    fn painted(buf: &Buffer) -> Vec<(u16, u16)> {
        let area = buf.area;
        let mut out = Vec::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if buf[(x, y)].symbol() != " " {
                    out.push((x, y));
                }
            }
        }
        out
    }

    fn probe(term_w: u16, term_h: u16, pct: u16) {
        let terminal = Rect { x: 0, y: 0, width: term_w, height: term_h };
        let ls = LayoutState {
            left_width_pct: pct,
            ..LayoutState::default()
        };
        let areas = ls.compute(terminal);

        let mut w = LogWindow::new();
        for i in 0..40 {
            w.push_log(LogEntry {
                timestamp: String::new(),
                source: format!("discord-adapter-{}", i),
                message: format!("a fairly long log message number {} that goes on a bit", i),
                event_type: 1 + (i % 5),
            });
        }

        let mut buf = Buffer::empty(terminal);
        let colors = crate::colors::load_colors(&std::path::PathBuf::from(""));
        w.render(areas.log, &mut buf, true, &crate::db::GlobalStats::default(), &colors, &default_hotkeys(), &[]);

        let outside: Vec<(u16, u16)> = painted(&buf).into_iter()
            .filter(|(x, y)| {
                !(*x >= areas.log.x && *x < areas.log.x + areas.log.width
                  && *y >= areas.log.y && *y < areas.log.y + areas.log.height)
            })
            .collect();
        assert!(
            outside.is_empty(),
            "log wrote {} cell(s) outside its area {:?} (modules is {:?}) -- first: {:?}",
            outside.len(),
            areas.log,
            areas.modules,
            outside.iter().take(6).collect::<Vec<_>>()
        );
    }

    /// The reported "log overflows into modules" symptom. The log window is
    /// provably confined to its own rect for every legal layout, so anything
    /// seen bleeding across is a stale-cell artifact of the diff-based
    /// renderer rather than a bounds bug -- which is what the full-repaint on
    /// a shape change fixes.
    #[test]
    fn log_never_paints_outside_its_own_area() {
        for (w, h) in [(120u16, 40u16), (200, 60), (80, 24), (60, 20)] {
            for pct in [10u16, 30, 50, 90] {
                probe(w, h, pct);
            }
        }
    }
}

