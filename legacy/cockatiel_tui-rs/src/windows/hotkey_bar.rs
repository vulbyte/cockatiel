use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::app::{Window, WindowId};
use crate::colors::ColorConfig;
use crate::db::GlobalStats;
use crate::hotkeys::Action;

pub struct HotkeyBarWindow;

impl Window for HotkeyBarWindow {
    fn id(&self) -> WindowId {
        WindowId::HotkeyBar
    }

    fn render(&self, area: Rect, buf: &mut Buffer, is_active: bool, _stats: &GlobalStats, colors: &ColorConfig) {
        let border_color = if is_active {
            colors.active_border_color("hotkey_bar")
        } else {
            colors.border_color("inactive")
        };

        let block = Block::default()
            .title(" hotkeys ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let hotkeys = Line::from(vec![
            Span::styled("hjkl", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":nav  ", Style::default().fg(Color::DarkGray)),
            Span::styled("s", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(":start/stop  ", Style::default().fg(Color::DarkGray)),
            Span::styled("x", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled(":disconnect  ", Style::default().fg(Color::DarkGray)),
            Span::styled("n", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(":note  ", Style::default().fg(Color::DarkGray)),
            Span::styled("i", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::styled(":info  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Tab", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(":next  ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled(":quit", Style::default().fg(Color::DarkGray)),
        ]);

        let paragraph = Paragraph::new(hotkeys).block(block);
        paragraph.render(area, buf);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent, _stats: &mut GlobalStats) -> Option<Action> {
        match key.code {
            crossterm::event::KeyCode::Char('w') => Some(Action::PopOut("hotkey_bar".to_string())),
            _ => None,
        }
    }
}
