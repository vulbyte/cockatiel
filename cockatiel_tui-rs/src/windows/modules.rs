use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::app::{PendingPrompt, Window, WindowId};
use crate::colors::ColorConfig;
use crate::db::GlobalStats;
use crate::hotkeys::{Action, HotkeyConfig};

pub struct ModulesWindow {
    pub scroll: usize,
    pub selected: usize,
    /// Clickable region of the pending prompt's link, set during render.
    pub link_rect: Option<Rect>,
    pub link_url: Option<String>,
}

impl ModulesWindow {
    pub fn new() -> Self {
        Self {
            scroll: 0,
            selected: 0,
            link_rect: None,
            link_url: None,
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
            let line = Line::from(vec![
                Span::styled("    backup:   ", Style::default().fg(Color::DarkGray)),
                Span::styled("unknown", Style::default().fg(Color::DarkGray)),
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
            &["start", "stop", "del", "auto", "creds", "test", "select", "popout"],
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
}
