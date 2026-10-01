//! Width-aware layout for the hotkey bars along the bottom of each window.
//!
//! Every window drew its hints as a single-row `Paragraph` with wrapping off, so
//! ratatui hard-clipped the line at the right edge of the window: on a narrow
//! terminal the tail of the bar (`select`, `popout`, `quit`, …) simply vanished
//! and only reappeared when the terminal was fullscreened. This module wraps the
//! bar instead, and grows it upward into the window only when it needs more than
//! one line.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// One run of text sharing a style. A bar built from a single style is one
/// chunk; the log window passes several to keep its two-tone look.
pub type Chunk<'a> = (String, Style);

/// The wrapped bar, where to draw it, and the rect the window's content may use.
pub struct HotkeyBar {
    /// Wrapped lines, ready to render.
    pub lines: Vec<Line<'static>>,
    /// Where to draw them.
    pub area: Rect,
    /// The rect for the window's CONTENT: `inner`, shortened by any rows the bar
    /// claimed by growing upward. Unchanged when the bar fits on one line, so a
    /// wide terminal looks exactly as it did before.
    pub content: Rect,
    /// Rows this bar took from the window body: 0 when it fits on the bottom
    /// border row (the long-standing behaviour), otherwise the number of rows it
    /// grew upward. A window that centres a dialog over `area` should shorten
    /// `area` by this much so the dialog cannot land under the bar.
    pub claimed: u16,
}

/// Split styled text into words, dropping the runs of whitespace between them.
fn flatten_words(chunks: &[Chunk<'_>]) -> Vec<(String, Style)> {
    let mut words = Vec::new();
    for (text, style) in chunks {
        for word in text.split_whitespace() {
            if !word.is_empty() {
                words.push((word.to_string(), *style));
            }
        }
    }
    words
}

/// Greedily wrap styled words to `width` columns.
///
/// A word wider than the line is hard-broken across lines instead of
/// overflowing, and because the break is mid-word it inserts no space — a
/// wrapped `command:[ctrl+shift+x|alt+x]` rejoins exactly. Word styles are
/// preserved across the break.
fn wrap_words(words: Vec<(String, Style)>, width: usize) -> Vec<Vec<(String, Style)>> {
    if width == 0 {
        return vec![words];
    }
    let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
    let mut current: Vec<(String, Style)> = Vec::new();
    let mut used = 0usize;

    for (word, style) in words {
        let mut rest: Vec<char> = word.chars().collect();
        let width_of_word = rest.len();

        if width_of_word <= width {
            // The whole word fits on a line of its own, so wrap it intact rather
            // than splitting `del:[d]` into `del` + `:[d]`.
            if used > 0 && used + 1 + width_of_word > width {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            if used > 0 {
                current.push((" ".to_string(), style));
                used += 1;
            }
            used += width_of_word;
            current.push((word, style));
            if used >= width {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            continue;
        }

        // Wider than an entire line: hard-break it, and because a continuation
        // is mid-word, never insert a space between the pieces.
        let mut want_space = used > 0;
        while !rest.is_empty() {
            let overhead = usize::from(want_space);
            let available = width.saturating_sub(used + overhead);
            if available == 0 {
                lines.push(std::mem::take(&mut current));
                used = 0;
                want_space = false;
                continue;
            }
            let take = available.min(rest.len());
            let piece: String = rest.drain(..take).collect();
            if want_space {
                current.push((" ".to_string(), style));
                used += 1;
            }
            used += piece.chars().count();
            current.push((piece, style));
            if used >= width {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            want_space = false;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Merge adjacent same-style pieces so the rendered line stays compact.
fn to_line(pieces: Vec<(String, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (text, style) in pieces {
        match spans.last_mut() {
            Some(last) if last.style == style => {
                let merged = format!("{}{}", last.content, text);
                *last = Span::styled(merged, style);
            }
            _ => spans.push(Span::styled(text, style)),
        }
    }
    Line::from(spans)
}

/// Place a hotkey bar in `area`, wrapping it to the window's inner width.
///
/// * Text that fits on one line is drawn on the window's bottom border row,
///   exactly as before, and costs no content rows — a wide terminal is
///   pixel-identical to before.
/// * Text that does not fit is wrapped and drawn in the bottom rows of `inner`,
///   and `content` is shortened by the rows claimed so content is never
///   overdrawn.
pub fn layout(chunks: &[Chunk<'_>], area: Rect, inner: Rect) -> HotkeyBar {
    let lines: Vec<Line<'static>> = wrap_words(flatten_words(chunks), inner.width as usize)
        .into_iter()
        .map(to_line)
        .collect();

    let needed = lines.len().max(1) as u16;
    if needed <= 1 {
        return HotkeyBar {
            lines,
            area: Rect {
                x: area.x + 1,
                y: area.y + area.height.saturating_sub(1),
                width: area.width.saturating_sub(2),
                height: 1,
            },
            content: inner,
            claimed: 0,
        };
    }

    let claimed = needed.min(inner.height);
    HotkeyBar {
        lines,
        area: Rect {
            x: inner.x,
            y: inner.y + inner.height - claimed,
            width: inner.width,
            height: claimed,
        },
        content: Rect {
            height: inner.height.saturating_sub(claimed),
            ..inner
        },
        claimed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn plain(text: &str) -> Vec<Chunk<'_>> {
        vec![(text.to_string(), Style::default().fg(Color::DarkGray))]
    }

    fn rendered(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_bar_that_fits_stays_on_the_border_row_and_costs_no_content() {
        let area = Rect { x: 0, y: 0, width: 80, height: 20 };
        let inner = Rect { x: 1, y: 1, width: 78, height: 18 };
        let bar = layout(&plain("nav:[j|k]  start:[s]"), area, inner);
        assert_eq!(bar.lines.len(), 1);
        assert_eq!(
            bar.area,
            Rect { x: 1, y: 19, width: 78, height: 1 },
            "a one-line bar keeps the original border-row placement"
        );
        assert_eq!(bar.content, inner, "content must not shrink when nothing wrapped");
    }

    #[test]
    fn a_bar_too_wide_wraps_onto_extra_rows_and_reserves_them() {
        let area = Rect { x: 0, y: 0, width: 30, height: 12 };
        let inner = Rect { x: 1, y: 1, width: 28, height: 10 };
        let text = "nav:[j|k|arrows]  start:[s]  stop:[x]  del:[d]  auto:[a]  copy:[c]  creds:[C]";
        let bar = layout(&plain(text), area, inner);
        assert!(bar.lines.len() > 1, "expected wrapping, got {} line(s)", bar.lines.len());
        for line in &bar.lines {
            assert!(
                line.spans.iter().map(|s| s.content.chars().count()).sum::<usize>() <= inner.width as usize,
                "line exceeds the window width: {:?}",
                rendered(line)
            );
        }
        // The bar is bottom-aligned inside `inner` and claims exactly its rows.
        assert_eq!(bar.area.height as usize, bar.lines.len());
        assert_eq!(bar.area.y + bar.area.height, inner.y + inner.height);
        assert_eq!(bar.content.height, inner.height - bar.lines.len() as u16);
    }

    #[test]
    fn wrapping_preserves_every_word_and_none_are_dropped() {
        let area = Rect { x: 0, y: 0, width: 24, height: 10 };
        let inner = Rect { x: 1, y: 1, width: 22, height: 8 };
        let text = "start:[s] stop:[x] del:[d] auto:[a] copy:[c] creds:[C] edit:[e] select:[v]";
        let bar = layout(&plain(text), area, inner);
        let joined: String = bar.lines.iter().map(rendered).collect::<Vec<_>>().join(" ");
        for word in text.split_whitespace() {
            assert!(
                joined.contains(word),
                "word {:?} vanished after wrapping; got {:?}",
                word,
                joined
            );
        }
    }

    #[test]
    fn an_overlong_single_word_is_broken_rather_than_clipped() {
        // The failure the user reported: a long `command:[a|b|c]` label must not
        // disappear off the right edge.
        let area = Rect { x: 0, y: 0, width: 20, height: 10 };
        let inner = Rect { x: 1, y: 1, width: 18, height: 8 };
        let word = "select:[ctrl+shift+v|shift+v|v]";
        let bar = layout(&plain(word), area, inner);
        assert!(bar.lines.len() > 1, "an overlong word must break, got {:?}", bar.lines.len());
        let rejoined: String = bar.lines.iter().map(rendered).collect::<Vec<_>>().join("");
        assert_eq!(rejoined, word, "a hard break must rejoin into the original word");
        for line in &bar.lines {
            assert!(line.spans.iter().map(|s| s.content.chars().count()).sum::<usize>() <= inner.width as usize);
        }
    }

    #[test]
    fn multi_style_chunks_keep_their_style_across_a_wrap() {
        let key = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        let desc = Style::default().fg(Color::DarkGray);
        let chunks = vec![
            ("1-5".to_string(), key),
            (":filter".to_string(), desc),
            ("j/k".to_string(), key),
            (":scroll".to_string(), desc),
            ("w".to_string(), key),
        ];
        let area = Rect { x: 0, y: 0, width: 16, height: 10 };
        let inner = Rect { x: 1, y: 1, width: 14, height: 8 };
        let bar = layout(&chunks, area, inner);
        assert!(bar.lines.len() > 1, "expected a wrap, got {:?}", bar.lines.len());
        let key_style = bar.lines[0].spans[0].style;
        assert_eq!(key_style, key, "the key run must keep its style");
        assert!(bar.lines[0].spans.iter().any(|s| s.style == desc), "the label style must survive");
    }

    #[test]
    fn a_zero_width_window_cannot_panic_or_loop() {
        let area = Rect { x: 0, y: 0, width: 0, height: 0 };
        let inner = Rect { x: 0, y: 0, width: 0, height: 0 };
        let bar = layout(&plain("start:[s] stop:[x]"), area, inner);
        assert_eq!(bar.content.height, 0);
    }

    #[test]
    fn a_bar_longer_than_the_window_never_claims_more_rows_than_exist() {
        let area = Rect { x: 0, y: 0, width: 12, height: 4 };
        let inner = Rect { x: 1, y: 1, width: 10, height: 2 };
        let text = (0..40)
            .map(|i| format!("k{}:[{}]", i, i))
            .collect::<Vec<_>>()
            .join(" ");
        let bar = layout(&plain(&text), area, inner);
        assert!(bar.area.height <= inner.height, "claimed {} rows of a {} row window", bar.area.height, inner.height);
        assert_eq!(bar.content.height, inner.height - bar.area.height);
    }

    #[test]
    fn empty_text_still_renders_one_blank_row() {
        let area = Rect { x: 0, y: 0, width: 20, height: 6 };
        let inner = Rect { x: 1, y: 1, width: 18, height: 4 };
        let bar = layout(&plain(""), area, inner);
        assert_eq!(bar.lines.len(), 1);
        assert_eq!(bar.area.height, 1);
    }
}
