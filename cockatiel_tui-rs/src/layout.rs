use ratatui::layout::Rect;

#[derive(Debug, Clone)]
pub struct LayoutState {
    /// Left column width percentage (10-90)
    pub left_width_pct: u16,
    /// Top section height percentage (10-90)
    pub top_height_pct: u16,
    /// Log panel height percentage within left column (below logo)
    pub log_height_pct: u16,
    /// Prompts window width percentage within the bottom (chart) row
    pub prompts_width_pct: u16,

    /// Drag state
    pub dragging: Option<DragEdge>,
    pub drag_start: Option<(u16, u16)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragEdge {
    LeftVertical,
    HorizontalTopBottom,
    HorizontalLogoLog,
}

impl Default for LayoutState {
    fn default() -> Self {
        Self {
            left_width_pct: 30,
            top_height_pct: 70,
            log_height_pct: 50,
            prompts_width_pct: 35,
            dragging: None,
            drag_start: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LayoutAreas {
    pub left: Rect,
    pub logo: Rect,
    pub log: Rect,
    #[allow(dead_code)]
    pub right: Rect,
    pub modules: Rect,
    pub chart: Rect,
    pub prompts: Rect,
    pub status_bar: Rect,
}

impl LayoutState {
    pub fn compute(&self, terminal: Rect) -> LayoutAreas {
        let status_height = 1;
        let main_height = terminal.height.saturating_sub(status_height);
        let main = Rect {
            x: terminal.x,
            y: terminal.y,
            width: terminal.width,
            height: main_height,
        };

        // Vertical split: top vs chart. Guard the clamps so a tiny terminal
        // (or a resize mid-frame) can never produce max < min and panic.
        let top_height = (main.height as f64 * self.top_height_pct as f64 / 100.0) as u16;
        let top_max = main.height.saturating_sub(3).max(5);
        let top_height = top_height.clamp(5, top_max).min(main.height);
        let chart_height = main.height.saturating_sub(top_height);

        let chart_area = Rect { x: main.x, y: main.y + top_height, width: main.width, height: chart_height };

        // Horizontal split of top: left vs right
        let left_width = (main.width as f64 * self.left_width_pct as f64 / 100.0) as u16;
        let left_max = main.width.saturating_sub(10).max(5);
        let left_width = left_width.clamp(5, left_max).min(main.width);
        let right_width = main.width.saturating_sub(left_width);

        let left_area = Rect { x: main.x, y: main.y, width: left_width, height: top_height };
        let right_area = Rect { x: main.x + left_width, y: main.y, width: right_width, height: top_height };

        // Left column: logo (top) + log (bottom)
        let log_height = (top_height as f64 * self.log_height_pct as f64 / 100.0) as u16;
        let log_max = top_height.saturating_sub(3).max(3);
        let log_height = log_height.clamp(3, log_max).min(top_height);
        let logo_height = top_height.saturating_sub(log_height);

        let logo_area = Rect { x: left_area.x, y: left_area.y, width: left_width, height: logo_height };
        let log_area = Rect { x: left_area.x, y: left_area.y + logo_height, width: left_width, height: log_height };

        // Right column: modules (full width)
        let modules_area = right_area;

        // Bottom row: chart (left) + prompts (right of the graph)
        let prompts_width = (chart_area.width as f64 * self.prompts_width_pct as f64 / 100.0) as u16;
        let prompts_max = chart_area.width.saturating_sub(20).max(10);
        let prompts_width = prompts_width.clamp(10, prompts_max).min(chart_area.width);
        let chart_width = chart_area.width.saturating_sub(prompts_width);
        let chart_area = Rect { x: chart_area.x, y: chart_area.y, width: chart_width, height: chart_height };
        let prompts_area = Rect { x: chart_area.x + chart_width, y: chart_area.y, width: prompts_width, height: chart_height };

        // Status bar
        let status_area = Rect { x: terminal.x, y: terminal.y + main_height, width: terminal.width, height: status_height };

        LayoutAreas {
            left: left_area,
            logo: logo_area,
            log: log_area,
            right: right_area,
            modules: modules_area,
            chart: chart_area,
            prompts: prompts_area,
            status_bar: status_area,
        }
    }

    /// Hit test for border dragging. Returns the drag edge if the mouse is near a border.
    pub fn hit_test_border(&self, terminal: Rect, x: u16, y: u16) -> Option<DragEdge> {
        let areas = self.compute(terminal);
        let threshold = 1;

        // Left column right border (vertical)
        let left_border_x = areas.left.x + areas.left.width;
        if x >= left_border_x.saturating_sub(threshold) && x <= left_border_x + threshold {
            if y >= areas.left.y && y <= areas.left.y + areas.left.height {
                return Some(DragEdge::LeftVertical);
            }
        }

        // Logo/Log border (horizontal)
        let logo_log_border_y = areas.log.y;
        if y >= logo_log_border_y.saturating_sub(threshold) && y <= logo_log_border_y + threshold {
            if x >= areas.left.x && x <= areas.left.x + areas.left.width {
                return Some(DragEdge::HorizontalLogoLog);
            }
        }

        // Top/Chart border (horizontal)
        let top_border_y = areas.chart.y;
        if y >= top_border_y.saturating_sub(threshold) && y <= top_border_y + threshold {
            if x >= terminal.x && x <= terminal.x + terminal.width {
                return Some(DragEdge::HorizontalTopBottom);
            }
        }

        None
    }

    /// Update layout percentages based on drag
    pub fn update_from_drag(&mut self, terminal: Rect, edge: DragEdge, x: u16, y: u16) {
        match edge {
            DragEdge::LeftVertical => {
                let pct = (x as f64 / terminal.width as f64 * 100.0) as u16;
                self.left_width_pct = pct.clamp(10, 90);
            }
            DragEdge::HorizontalTopBottom => {
                let pct = (y as f64 / terminal.height as f64 * 100.0) as u16;
                self.top_height_pct = pct.clamp(10, 90);
            }
            DragEdge::HorizontalLogoLog => {
                let areas = self.compute(terminal);
                let local_y = y.saturating_sub(areas.left.y);
                let pct = (local_y as f64 / areas.left.height as f64 * 100.0) as u16;
                self.log_height_pct = (100 - pct).clamp(10, 90);
            }
        }
    }
}
