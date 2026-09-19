use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::app::{Window, WindowId};
use crate::colors::ColorConfig;
use crate::db::GlobalStats;
use crate::hotkeys::Action;

const LOGO: &str = r#"
                         X
        XXXXXXXXX      XXX
      XXXXXXXXXXXXXXXXXXX
     XX    XXXXXXXXXXXXX
  XXXX      XXXXXXXXXXXXXX
 XXXXXX    XXXXXXXXX XX
   XXXXXXXXXXXXXXXXX
     XXXXXXXXXXXXXXX
     XXX XXXXXXX XXX
     XX   XXXX    XX

cockatiel
   -by vulbyte"#;

pub struct LogoWindow;

impl Window for LogoWindow {
    fn id(&self) -> WindowId {
        WindowId::Logo
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, is_active: bool, stats: &GlobalStats, colors: &ColorConfig, _hotkeys: &crate::hotkeys::HotkeyConfig, _prompts: &[crate::app::PendingPrompt]) {
        let border_color = if is_active {
            colors.active_border_color("logo")
        } else {
            colors.border_color("inactive")
        };

        let block = Block::default()
            .title(" cockatiel ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let mut lines: Vec<Line> = LOGO.lines()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::Gray))))
            .collect();

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  Total messages: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", stats.total_messages), Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Total users:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", stats.total_users), Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Total commands:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", stats.total_commands), Style::default().fg(Color::White)),
        ]));

        // Connection info
        if !stats.connection.ip.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  IP:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(&stats.connection.ip, Style::default().fg(Color::Cyan)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  Port:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{}", stats.connection.port), Style::default().fg(Color::Cyan)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  PIN:    ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{:06}", stats.connection.pin), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            ]));
        }

        let paragraph = Paragraph::new(lines).block(block);
        paragraph.render(area, buf);

        // Hotkey bar at bottom of window
        let hotkey_area = Rect { x: area.x + 1, y: area.y + area.height.saturating_sub(1), width: area.width.saturating_sub(2), height: 1 };
        let hotkeys = Line::from(vec![
            Span::styled("w", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]);
        let hotkey_para = Paragraph::new(hotkeys);
        hotkey_para.render(hotkey_area, buf);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent, _stats: &mut GlobalStats) -> Option<Action> {
        match key.code {
            crossterm::event::KeyCode::Char('w') => Some(Action::PopOut("logo".to_string())),
            _ => None,
        }
    }
}
