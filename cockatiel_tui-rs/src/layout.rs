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
    /// The vertical divider between the chart and the prompts window, so the
    /// prompts pane can be made wider or thinner by dragging it like the others.
    VerticalChartPrompts,
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
    pub modules: Rect,
    pub chart: Rect,
    pub prompts: Rect,
    /// The full-width bottom row BEFORE it is split into chart + prompts.
    /// `prompts_width_pct` is a fraction of this, so the chart/prompts drag
    /// needs it to convert a mouse x back into a percentage.
    pub bottom_row: Rect,
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
        let bottom_row = chart_area;
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
            modules: modules_area,
            chart: chart_area,
            prompts: prompts_area,
            bottom_row,
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

        // Chart/Prompts divider (vertical) — checked BEFORE the top/bottom rule
        // so the corner cell belongs to the vertical drag; a vertical resize
        // there is far more useful than nudging the top row by one row.
        let chart_prompts_x = areas.prompts.x;
        if x >= chart_prompts_x.saturating_sub(threshold)
            && x <= chart_prompts_x + threshold
            && y >= areas.chart.y
            && y <= areas.chart.y + areas.chart.height
        {
            return Some(DragEdge::VerticalChartPrompts);
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
            DragEdge::VerticalChartPrompts => {
                let areas = self.compute(terminal);
                // The prompts pane is RIGHT-anchored: it runs from the divider to
                // the right edge of the bottom row. So dragging the divider LEFT
                // widens it and dragging RIGHT narrows it, like any window edge.
                // `prompts_width_pct` is measured against the FULL bottom row, not
                // the already-narrowed chart, so convert against that rect.
                let row = areas.bottom_row;
                if row.width == 0 {
                    return;
                }
                let right = row.x + row.width;
                // Distance from the mouse to the row's right edge = prompts width.
                let local_x = right.saturating_sub(x).min(row.width);
                let pct = (local_x as f64 / row.width as f64 * 100.0) as u16;
                self.prompts_width_pct = pct.clamp(10, 80);
            }
        }
    }
}

#[cfg(test)]
mod prompts_drag_tests {
    use super::*;

    fn term() -> Rect {
        Rect { x: 0, y: 0, width: 120, height: 40 }
    }

    #[test]
    fn the_chart_prompts_divider_is_draggable() {
        let ls = LayoutState::default();
        let areas = ls.compute(term());
        // The divider sits at the prompts window's left edge, mid-height.
        let x = areas.prompts.x;
        let y = areas.prompts.y + areas.prompts.height / 2;
        assert_eq!(
            ls.hit_test_border(term(), x, y),
            Some(DragEdge::VerticalChartPrompts),
            "the chart/prompts divider must be grabbable"
        );
        // Either side of the 1-column threshold still hits it.
        assert_eq!(ls.hit_test_border(term(), x - 1, y), Some(DragEdge::VerticalChartPrompts));
        assert_eq!(ls.hit_test_border(term(), x + 1, y), Some(DragEdge::VerticalChartPrompts));
    }

    #[test]
    fn dragging_the_divider_left_widens_the_prompts_window() {
        // The prompts pane is right-anchored, so it behaves like a window's left
        // edge: drag the divider left and the pane grows, drag it right and it
        // shrinks. Dragging the wrong way is the "backwards drag" bug.
        let mut ls = LayoutState::default();
        let before = ls.compute(term()).prompts.width;

        // Drag the divider left of where it started -> prompts gets wider.
        let left_x = ls.compute(term()).prompts.x - 20;
        ls.update_from_drag(term(), DragEdge::VerticalChartPrompts, left_x, 30);
        let wider = ls.compute(term()).prompts.width;
        assert!(
            wider > before,
            "dragging the divider LEFT should widen prompts: {before} -> {wider}"
        );

        // ...and back to the right -> prompts gets thinner again.
        let mut ls = LayoutState::default();
        let start = ls.compute(term()).prompts.width;
        let right_x = ls.compute(term()).prompts.x + 30;
        ls.update_from_drag(term(), DragEdge::VerticalChartPrompts, right_x, 30);
        let thinner = ls.compute(term()).prompts.width;
        assert!(
            thinner < start,
            "dragging the divider RIGHT should narrow prompts: {start} -> {thinner}"
        );

        // Far right -> thin, but never disappears.
        let mut ls = LayoutState::default();
        ls.update_from_drag(term(), DragEdge::VerticalChartPrompts, 120, 30);
        let minimum = ls.compute(term()).prompts.width;
        assert!(minimum >= 10, "compute() floors prompts at 10 columns, got {minimum}");
    }

    #[test]
    fn the_drag_percentage_is_measured_against_the_whole_bottom_row() {
        // prompts_width_pct is a fraction of the FULL bottom row, not of the
        // already-narrowed chart, so the drag must divide by bottom_row.width.
        let mut ls = LayoutState::default();
        ls.update_from_drag(term(), DragEdge::VerticalChartPrompts, 60, 30);
        assert_eq!(
            ls.prompts_width_pct, 50,
            "x=60 of a 120-wide bottom row is 50%"
        );
        let areas = ls.compute(term());
        assert_eq!(areas.bottom_row.width, 120);
    }

    #[test]
    fn the_chart_keeps_a_usable_minimum_width() {
        for x in (0..=120u16).step_by(4) {
            let mut ls = LayoutState::default();
            ls.update_from_drag(term(), DragEdge::VerticalChartPrompts, x, 30);
            let areas = ls.compute(term());
            assert!(
                areas.chart.width >= 20,
                "at x={x} the chart collapsed to {} columns",
                areas.chart.width
            );
            assert!(areas.prompts.width >= 10, "at x={x} prompts collapsed");
        }
    }

    #[test]
    fn the_existing_drag_edges_still_work() {
        let ls = LayoutState::default();
        let areas = ls.compute(term());
        // Left vertical divider, mid height of the top row.
        let y = areas.left.y + areas.left.height / 2;
        assert_eq!(
            ls.hit_test_border(term(), areas.left.x + areas.left.width, y),
            Some(DragEdge::LeftVertical)
        );
        // Logo/log divider.
        assert_eq!(
            ls.hit_test_border(term(), areas.left.x + 2, areas.log.y),
            Some(DragEdge::HorizontalLogoLog)
        );
        // Top/bottom divider, on the left half so it cannot be confused with
        // the chart/prompts one.
        assert_eq!(
            ls.hit_test_border(term(), 5, areas.chart.y),
            Some(DragEdge::HorizontalTopBottom)
        );
    }

    #[test]
    fn no_two_panes_ever_overlap_after_a_drag() {
        // The invariant behind "the log overflows into modules": every pane must
        // be disjoint from every other for any legal set of percentages.
        for lx in (10..=90u16).step_by(8) {
            for tx in (10..=90u16).step_by(8) {
                for lh in (10..=90u16).step_by(8) {
                    for pw in (10..=80u16).step_by(8) {
                        let mut ls = LayoutState {
                            left_width_pct: lx,
                            top_height_pct: tx,
                            log_height_pct: lh,
                            prompts_width_pct: pw,
                            ..Default::default()
                        };
                        ls.dragging = None;
                        let a = ls.compute(term());
                        let panes = [
                            ("logo", a.logo),
                            ("log", a.log),
                            ("modules", a.modules),
                            ("chart", a.chart),
                            ("prompts", a.prompts),
                        ];
                        for (i, (ni, ri)) in panes.iter().enumerate() {
                            for (nj, rj) in panes.iter().skip(i + 1) {
                                let overlap_x =
                                    ri.x < rj.x + rj.width && rj.x < ri.x + ri.width;
                                let overlap_y =
                                    ri.y < rj.y + rj.height && rj.y < ri.y + ri.height;
                                assert!(
                                    !(overlap_x && overlap_y),
                                    "panes {ni}{ri:?} and {nj}{rj:?} overlap at lx={lx} tx={tx} lh={lh} pw={pw}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
