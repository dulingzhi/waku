//! What the rope-backed editor paints: which lines a viewport shows, and which
//! spans of each line take which colour.
//!
//! This is the layer between the buffer (`winc_buffer`) and a GPUI canvas. It is
//! deliberately free of drawing calls so it can be tested without a window: the
//! bugs that matter here — an off-by-one at the viewport edge, a span computed
//! against the wrong line, a colour picked for an unclassified run — are all
//! arithmetic, and arithmetic is testable.
//!
//! ## Row heights are an input, not a measurement
//!
//! The caller supplies the row height it will actually paint at. This module does
//! not measure text: GPUI's shaping needs a window, and doing it here would make
//! every test require one. Keeping the guess outside also keeps a wrong value
//! local — it misplaces rows, but it cannot desynchronise the line numbers from
//! the text, because both derive from the same index.

use std::ops::Range;

use gpui::Hsla;

use super::winc_buffer::WincBuffer;
use super::winc_highlight::LineHighlighter;
use crate::md::highlight::TokenClass;

/// One visible row: its line number, text, and coloured spans as byte ranges
/// **within that line's text**.
#[derive(Clone, Debug, PartialEq)]
pub struct PaintedLine {
    pub line: usize,
    pub text: String,
    pub spans: Vec<ColoredSpan>,
}

/// A span with the colour already resolved, so the canvas does no lookups.
#[derive(Clone, Debug, PartialEq)]
pub struct ColoredSpan {
    pub range: Range<usize>,
    pub color: Hsla,
}

/// Resolves a token class to a colour, provided by the caller so this module
/// holds no palette of its own.
pub trait Palette {
    fn color_of(&self, class: TokenClass) -> Hsla;
}

/// The rows a viewport shows, ready to paint.
///
/// `scroll_top` is the vertical scroll offset in pixels, `viewport_height` the
/// visible height. Rows are `row_height` tall and never soft-wrapped: wrapping
/// would make "which line is at this pixel" depend on measured text, which is the
/// previous editor's trade and the reason it re-laid out on every keystroke.
pub fn visible_lines(
    buffer: &WincBuffer,
    highlighter: Option<&mut LineHighlighter>,
    scroll_top: f32,
    viewport_height: f32,
    row_height: f32,
    palette: &impl Palette,
) -> Vec<PaintedLine> {
    let total = buffer.line_count();
    if total == 0 || row_height <= 0.0 || viewport_height <= 0.0 {
        return Vec::new();
    }

    // The rows the viewport shows, plus whatever the state chain needs above
    // them. Read once, up front: a rope line copied per painted row would make
    // scrolling cost proportional to the scroll position.
    let first = (scroll_top / row_height).floor().max(0.0) as usize;
    let visible = (viewport_height / row_height).ceil() as usize + 1;
    let last = (first + visible).min(total);

    let mut painted = Vec::with_capacity(last.saturating_sub(first));
    // Take the highlighter out once: `Option<&mut _>` is not `Copy`, so matching
    // on it inside the loop would move it on the first iteration.
    let mut highlighter = highlighter;
    for line in first..last {
        let text = buffer.line(line);
        // The lexer trims nothing, but a CRLF file leaves a `\r` that would paint
        // as a stray cell and shift the caret one column.
        let text = text.trim_end_matches(['\r', '\n']).to_owned();

        let spans = match highlighter.as_deref_mut() {
            // Walking the chain as it draws is what keeps a block comment
            // coloured on rows the viewport jumps straight into.
            Some(hl) => {
                // The chain may need lines above `first`, so it is walked from
                // the rope directly before this row is highlighted.
                let above: Vec<String> = if hl.known_state(line).is_none() {
                    (0..line)
                        .map(|index| buffer.line(index).trim_end_matches(['\r', '\n']).to_owned())
                        .collect()
                } else {
                    Vec::new()
                };
                hl.ensure_through(line, |index| above.get(index).cloned());
                hl.highlight(line, &text)
                    .into_iter()
                    .map(|span| ColoredSpan {
                        range: span.range,
                        color: palette.color_of(span.class),
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        painted.push(PaintedLine { line, text, spans });
    }
    painted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::winc_highlight::LineHighlighter;

    struct Fixed;

    impl Palette for Fixed {
        fn color_of(&self, class: TokenClass) -> Hsla {
            // Distinct per class so a test can tell which class produced a span.
            gpui::rgb(match class {
                TokenClass::Keyword => 0xFF0000,
                TokenClass::Comment => 0x00FF00,
                _ => 0x0000FF,
            })
            .into()
        }
    }

    fn text_of(lines: &[PaintedLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn the_top_of_the_file_paints_from_line_zero() {
        let mut buffer = WincBuffer::from_text("a.rs", "a\nb\nc\nd");
        let painted = visible_lines(&buffer, None, 0.0, 40.0, 20.0, &Fixed);
        assert_eq!(text_of(&painted), ["a", "b", "c"]);
        assert_eq!(painted[0].line, 0);
    }

    #[test]
    fn scrolling_picks_the_rows_the_offset_reaches() {
        let mut buffer = WincBuffer::from_text("a.rs", "a\nb\nc\nd\ne");
        // Four rows down at 20px each is line 2, and two visible rows.
        let painted = visible_lines(&buffer, None, 40.0, 40.0, 20.0, &Fixed);
        assert_eq!(painted[0].line, 2);
        assert_eq!(text_of(&painted), ["c", "d", "e"]);
    }

    #[test]
    fn the_viewport_stops_at_the_last_line() {
        let mut buffer = WincBuffer::from_text("a.rs", "a\nb");
        let painted = visible_lines(&buffer, None, 0.0, 1000.0, 20.0, &Fixed);
        assert_eq!(text_of(&painted), ["a", "b"]);
    }

    #[test]
    fn scrolling_past_the_end_paints_nothing() {
        let mut buffer = WincBuffer::from_text("a.rs", "a\nb");
        let painted = visible_lines(&buffer, None, 1000.0, 40.0, 20.0, &Fixed);
        assert!(painted.is_empty(), "an offset below the file has no rows");
    }

    #[test]
    fn a_degenerate_row_height_paints_nothing_rather_than_dividing_by_zero() {
        let mut buffer = WincBuffer::from_text("a.rs", "a\nb");
        assert!(visible_lines(&buffer, None, 0.0, 40.0, 0.0, &Fixed).is_empty());
    }

    #[test]
    fn a_crlf_line_keeps_its_text_and_drops_its_carriage_return() {
        let mut buffer = WincBuffer::from_text("a.rs", "one\r\ntwo\r\n");
        let painted = visible_lines(&buffer, None, 0.0, 40.0, 20.0, &Fixed);
        assert_eq!(painted[0].text, "one");
        assert_eq!(painted[1].text, "two");
    }

    #[test]
    fn spans_carry_the_palette_colour_for_their_class() {
        let mut buffer = WincBuffer::from_text("a.rs", "fn main() {}");
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        let painted = visible_lines(&buffer, Some(&mut hl), 0.0, 20.0, 20.0, &Fixed);
        let keyword = painted[0]
            .spans
            .iter()
            .find(|span| &painted[0].text[span.range.clone()] == "fn")
            .expect("`fn` span");
        assert_eq!(keyword.color, gpui::rgb(0xFF0000).into());
    }

    #[test]
    fn a_viewport_opened_inside_a_block_comment_still_paints_it_as_comment() {
        // Five rows of comment, viewport starting at row 4: the chain must be
        // walked to row 4 before it can know the comment is still open.
        let mut buffer = WincBuffer::from_text("a.rs", "/*\na\nb\nc\nd */\nfn main() {}");
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        let painted = visible_lines(&buffer, Some(&mut hl), 80.0, 40.0, 20.0, &Fixed);
        assert_eq!(painted[0].line, 4);
        assert_eq!(painted[0].text, "d */");
        assert!(
            painted[0]
                .spans
                .iter()
                .any(|span| span.color == gpui::rgb(0x00FF00).into()),
            "the closing row of a comment opened above the viewport stays comment"
        );
    }

    #[test]
    fn a_line_below_a_closed_comment_paints_as_code() {
        let mut buffer = WincBuffer::from_text("a.rs", "/* a */\nfn main() {}");
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        let painted = visible_lines(&buffer, Some(&mut hl), 0.0, 80.0, 20.0, &Fixed);
        assert!(
            painted[1]
                .spans
                .iter()
                .any(|span| span.color == gpui::rgb(0xFF0000).into()),
            "the line after a one-line comment is code: {:?}",
            painted[1]
        );
    }

    /// The mutation that this test must catch is subtle: treating a token's char
    /// offsets as byte offsets still lands on a char boundary in most CJK cases
    /// (a 4-byte char swallows the difference), so a boundary-only check passes
    /// while the span quotes the wrong text. Asserting the *content* of the span
    /// is what makes the error visible.
    #[test]
    fn a_string_span_quotes_exactly_the_source_text() {
        let source = "let x = \"中文\";";
        let mut buffer = WincBuffer::from_text("a.rs", source);
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        let painted = visible_lines(&buffer, Some(&mut hl), 0.0, 20.0, 20.0, &Fixed);
        let span = painted[0]
            .spans
            .iter()
            .find(|span| span.color == gpui::rgb(0x0000FF).into())
            .expect("the string literal span");
        assert_eq!(
            &painted[0].text[span.range.clone()],
            "\"中文\"",
            "the span must quote the literal, not a slice of it"
        );
    }

    #[test]
    fn spans_stay_within_their_line() {
        let mut buffer = WincBuffer::from_text("a.rs", "fn a() {}\nlet x = \"中文\";");
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        let painted = visible_lines(&buffer, Some(&mut hl), 0.0, 80.0, 20.0, &Fixed);
        for line in &painted {
            for span in &line.spans {
                assert!(
                    span.range.end <= line.text.len(),
                    "span {:?} runs past its line of {} bytes",
                    span.range,
                    line.text.len()
                );
                assert!(
                    line.text.is_char_boundary(span.range.start)
                        && line.text.is_char_boundary(span.range.end),
                    "span {:?} splits a codepoint in {:?}",
                    span.range,
                    line.text
                );
            }
        }
    }
}
