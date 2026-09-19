use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::app::{Window, WindowId};
use crate::hotkeys::{Action, HotkeyConfig};
use crate::colors::ColorConfig;
use crate::db::GlobalStats;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TimeWindow {
    Minutes5,
    Hours1,
    Hours6,
    Hours24,
}

impl TimeWindow {
    pub fn label(&self) -> &'static str {
        match self {
            TimeWindow::Minutes5 => "5m",
            TimeWindow::Hours1 => "1h",
            TimeWindow::Hours6 => "6h",
            TimeWindow::Hours24 => "24h",
        }
    }
}

pub struct ChartWindow {
    pub visible_platforms: HashMap<String, bool>,
    pub hovered_platform: Option<usize>,
    pub time_window: TimeWindow,
}

impl ChartWindow {
    pub fn new() -> Self {
        Self {
            visible_platforms: HashMap::new(),
            hovered_platform: None,
            time_window: TimeWindow::Minutes5,
        }
    }

    fn toolbar_line(&self, stats: &GlobalStats, colors: &ColorConfig) -> Line<'_> {
        let mut spans = vec![
            Span::styled("  ", Style::default()),
        ];

        let mut platforms: Vec<&String> = stats.platform_counts.keys().collect();
        platforms.sort();

        for (i, platform) in platforms.iter().enumerate() {
            let visible = self.visible_platforms.get(*platform).copied().unwrap_or(true);
            let is_hovered = self.hovered_platform == Some(i);
            let platform_color = colors.platform_color(platform);

            let checkbox = if visible { "✓" } else { " " };
            let style = if is_hovered {
                Style::default().fg(platform_color).add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else if visible {
                Style::default().fg(platform_color)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            spans.push(Span::styled(format!("[{}] ", checkbox), style));
            spans.push(Span::styled(format!("{} ", platform), style));
        }

        // Time window selector
        spans.push(Span::styled("  ", Style::default()));
        for tw in &[TimeWindow::Minutes5, TimeWindow::Hours1, TimeWindow::Hours6, TimeWindow::Hours24] {
            let style = if *tw == self.time_window {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(format!("{} ", tw.label()), style));
        }

        Line::from(spans)
    }

    fn chart_lines(&self, area: Rect, stats: &GlobalStats, colors: &ColorConfig) -> Vec<Line<'_>> {
        let mut lines = Vec::new();

        if stats.chart_data.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("  No data yet", Style::default().fg(Color::DarkGray))));
            return lines;
        }

        // Find max value for normalization
        let mut max_val: u64 = 0;
        for bucket in &stats.chart_data {
            let total: u64 = bucket.counts.iter()
                .filter(|(p, _)| self.visible_platforms.get(*p).copied().unwrap_or(true))
                .map(|(_, c)| *c)
                .sum();
            if total > max_val {
                max_val = total;
            }
        }

        if max_val == 0 {
            max_val = 1;
        }

        let chart_height = area.height.saturating_sub(2) as usize;
        let chart_width = area.width.saturating_sub(4) as usize;

        if chart_width == 0 || chart_height == 0 {
            return lines;
        }

        // Sample data points to fit width
        let step = if stats.chart_data.len() > chart_width {
            (stats.chart_data.len() as f64 / chart_width as f64).ceil() as usize
        } else {
            1
        };

        // Build rows from top to bottom
        for row in 0..chart_height {
            let threshold = max_val as f64 * (1.0 - row as f64 / chart_height as f64);
            let mut spans = vec![Span::styled("  ", Style::default())];

            for col in 0..chart_width {
                let data_idx = (col * step).min(stats.chart_data.len().saturating_sub(1));
                let bucket = &stats.chart_data[data_idx];

                let mut y_val: u64 = 0;
                let mut platforms: Vec<(&String, &u64)> = bucket.counts.iter()
                    .filter(|(p, _)| self.visible_platforms.get(*p).copied().unwrap_or(true))
                    .collect();
                platforms.sort_by_key(|(_, c)| std::cmp::Reverse(**c));

                let mut found = false;
                for (platform, count) in &platforms {
                    y_val += *count;
                    if y_val as f64 >= threshold {
                        let color = colors.platform_color(platform);
                        spans.push(Span::styled("█", Style::default().fg(color)));
                        found = true;
                        break;
                    }
                }

                if !found {
                    spans.push(Span::styled(" ", Style::default()));
                }
            }

            lines.push(Line::from(spans));
        }

        // X-axis
        let mut axis_spans = vec![Span::styled("  └", Style::default().fg(Color::DarkGray))];
        axis_spans.push(Span::styled("─".repeat(chart_width.saturating_sub(1)), Style::default().fg(Color::DarkGray)));
        lines.push(Line::from(axis_spans));

        lines
    }
}

impl Window for ChartWindow {
    fn id(&self) -> WindowId {
        WindowId::Chart
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, is_active: bool, stats: &GlobalStats, colors: &ColorConfig, hotkeys: &HotkeyConfig, _prompts: &[crate::app::PendingPrompt]) {
        let border_color = if is_active {
            colors.active_border_color("chart")
        } else {
            colors.border_color("inactive")
        };

        let block = Block::default()
            .title(format!(" message chart [{}] ", self.time_window.label()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = area.inner(ratatui::layout::Margin { horizontal: 1, vertical: 1 });

        // Toolbar (top 1 line)
        let toolbar_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };

        let toolbar = self.toolbar_line(stats, colors);
        let toolbar_para = Paragraph::new(toolbar);
        toolbar_para.render(toolbar_area, buf);

        // Chart area (below toolbar)
        let chart_area = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        };

        let chart_lines = self.chart_lines(chart_area, stats, colors);
        let chart_para = Paragraph::new(chart_lines);
        chart_para.render(chart_area, buf);

        // Render border
        block.render(area, buf);

        // Hotkey bar at bottom of window
        let hotkey_area = Rect { x: area.x + 1, y: area.y + area.height.saturating_sub(1), width: area.width.saturating_sub(2), height: 1 };
        let mut hotkey_text = "nav:[h|l|arrows]  ".to_string();
        hotkey_text.push_str(&hotkeys.format_window(
            "chart",
            &["5m", "1h", "6h", "24h", "toggle", "zoom-in", "zoom-out", "popout"],
        ));
        let hotkeys_line = Line::from(vec![
            Span::styled(hotkey_text, Style::default().fg(Color::DarkGray)),
        ]);
        let hotkey_para = Paragraph::new(hotkeys_line);
        hotkey_para.render(hotkey_area, buf);
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent, area: Rect) -> Option<Action> {
        match mouse.kind {
            crossterm::event::MouseEventKind::Moved => {
                if mouse.row == area.y + 1 && mouse.column >= area.x + 2 {
                    self.hovered_platform = Some(((mouse.column - area.x - 2) / 12) as usize);
                } else {
                    self.hovered_platform = None;
                }
                Some(Action::Noop)
            }
            _ => None,
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent, stats: &mut GlobalStats) -> Option<Action> {
        match key.code {
            crossterm::event::KeyCode::Char('1') => {
                self.time_window = TimeWindow::Minutes5;
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('2') => {
                self.time_window = TimeWindow::Hours1;
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('3') => {
                self.time_window = TimeWindow::Hours6;
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('4') => {
                self.time_window = TimeWindow::Hours24;
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('h') | crossterm::event::KeyCode::Left => {
                if let Some(idx) = self.hovered_platform {
                    self.hovered_platform = Some(idx.saturating_sub(1));
                } else if !stats.platform_counts.is_empty() {
                    self.hovered_platform = Some(0);
                }
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('l') | crossterm::event::KeyCode::Right => {
                let count = stats.platform_counts.keys().len();
                if count == 0 {
                    return Some(Action::Noop);
                }
                let next = match self.hovered_platform {
                    Some(idx) => (idx + 1).min(count.saturating_sub(1)),
                    None => 0,
                };
                self.hovered_platform = Some(next);
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Enter | crossterm::event::KeyCode::Char(' ') => {
                if let Some(idx) = self.hovered_platform {
                    let mut platforms: Vec<String> = stats.platform_counts.keys().cloned().collect();
                    platforms.sort();
                    if let Some(platform) = platforms.get(idx) {
                        let visible = self.visible_platforms.get(platform).copied().unwrap_or(true);
                        self.visible_platforms.insert(platform.clone(), !visible);
                    }
                }
                Some(Action::Noop)
            }
            crossterm::event::KeyCode::Char('w') => Some(Action::PopOut("chart".to_string())),
            _ => None,
        }
    }
}
