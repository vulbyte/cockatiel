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
    #[allow(dead_code)]
    pub entries: VecDeque<LogEntry>,
    pub filter: LogFilter,
    #[allow(dead_code)]
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

    #[allow(dead_code)]
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

        // Hotkey bar at bottom of window
        let hotkey_area = Rect { x: area.x + 1, y: area.y + area.height.saturating_sub(1), width: area.width.saturating_sub(2), height: 1 };
        let hotkeys = Line::from(vec![
            Span::styled("1-5", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":filter  ", Style::default().fg(Color::DarkGray)),
            Span::styled("j/k", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":scroll  ", Style::default().fg(Color::DarkGray)),
            Span::styled("w", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]);
        let hotkey_para = Paragraph::new(hotkeys);
        hotkey_para.render(hotkey_area, buf);
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
