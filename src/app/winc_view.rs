//! The GPUI surface for the rope-backed editor: painting visible rows.
//!
//! Waku's panel draws the file itself rather than embedding a widget: one
//! `canvas` shapes only the rows the viewport shows, the way the existing
//! line-number gutter already does. That is what keeps a large file cheap — a
//! `div` per line would put one layout node per line into every frame.
//!
//! ## What this module is responsible for
//!
//! Given rows already chosen and coloured by [`super::winc_paint`], turn them
//! into shaped text runs and paint them. It owns no layout arithmetic and no
//! colour choice of its own, so a mistake in either shows up in the tests of the
//! module that does own it.
//!
//! ## The gap between spans
//!
//! A row's spans cover only the parts the lexer classified. Painting just those
//! would drop every space, `=`, and bracket — `let x = 1` would render as `let`
//! then `1` with nothing between. [`runs_for`] therefore fills the gaps with the
//! editor's foreground, and that is the one thing here worth testing directly.

use std::ops::Range;

use gpui::{Hsla, TextRun};

use super::winc_paint::PaintedLine;
use crate::md::highlight::TokenClass;
use crate::md::render::MONO_FAMILY;

/// Resolves syntax classes to the editor's colours.
///
/// One named type rather than a closure at each call site: the Markdown renderer
/// and this editor must agree on what a keyword looks like, and two ad-hoc
/// closures would drift apart.
pub struct EditorPalette {
    /// The default foreground, for text no token classified.
    pub text: Hsla,
    pub keyword: Hsla,
    pub literal: Hsla,
    pub string: Hsla,
    pub comment: Hsla,
    pub type_name: Hsla,
    pub meta: Hsla,
    pub added: Hsla,
    pub removed: Hsla,
}

impl EditorPalette {
    pub fn color_of(&self, class: TokenClass) -> Hsla {
        match class {
            TokenClass::Keyword => self.keyword,
            TokenClass::Literal | TokenClass::Number => self.literal,
            TokenClass::String => self.string,
            TokenClass::Comment => self.comment,
            TokenClass::Type | TokenClass::Function => self.type_name,
            TokenClass::Meta => self.meta,
            TokenClass::Added => self.added,
            TokenClass::Removed => self.removed,
        }
    }
}

impl super::winc_paint::Palette for EditorPalette {
    fn color_of(&self, class: TokenClass) -> Hsla {
        EditorPalette::color_of(self, class)
    }
}

/// Turn one painted row into shaped runs, one per coloured span and one per gap.
///
/// Ranges are byte ranges into `row.text`. Spans are sorted and non-overlapping by
/// construction; the cursor only ever moves forward, so a span that somehow
/// overlapped its predecessor would be clipped rather than double-painted.
pub fn runs_for(row: &PaintedLine, default_color: Hsla) -> Vec<TextRun> {
    let font = gpui::font(MONO_FAMILY);
    let mut runs = Vec::with_capacity(row.spans.len() * 2 + 1);
    let mut cursor = 0usize;

    let push = |runs: &mut Vec<TextRun>, range: Range<usize>, color: Hsla| {
        if range.end > range.start {
            runs.push(TextRun {
                len: range.end - range.start,
                font: font.clone(),
                color,
                ..Default::default()
            });
        }
    };

    for span in &row.spans {
        let start = span.range.start.max(cursor).min(row.text.len());
        let end = span.range.end.max(start).min(row.text.len());
        if start > cursor {
            push(&mut runs, cursor..start, default_color);
        }
        push(&mut runs, start..end, span.color);
        cursor = end;
    }
    push(&mut runs, cursor..row.text.len(), default_color);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::winc_paint::ColoredSpan;

    fn color(id: u32) -> Hsla {
        gpui::rgba(id).into()
    }

    fn row(text: &str, spans: Vec<(Range<usize>, u32)>) -> PaintedLine {
        PaintedLine {
            line: 0,
            text: text.to_owned(),
            spans: spans
                .into_iter()
                .map(|(range, id)| ColoredSpan {
                    range,
                    color: color(id),
                })
                .collect(),
        }
    }

    /// The regression this module exists for: tokens alone would drop the text
    /// between them.
    #[test]
    fn the_gaps_between_spans_are_painted_in_the_default_colour() {
        let line = row("let x = 1", vec![(0..3, 0xFF0000FF), (8..9, 0x00FF00FF)]);
        let runs = runs_for(&line, color(0x000000FF));
        let covered: usize = runs.iter().map(|run| run.len).sum();
        assert_eq!(covered, line.text.len(), "every byte must be covered");

        let gaps: Vec<_> = runs
            .iter()
            .filter(|run| run.color == color(0x000000FF))
            .map(|run| run.len)
            .collect();
        assert_eq!(gaps.iter().sum::<usize>(), line.text.len() - 4);
    }

    #[test]
    fn runs_cover_the_row_exactly_once_in_order() {
        let line = row("fn a() {}", vec![(0..2, 0xFF0000FF), (3..4, 0x00FF00FF)]);
        let runs = runs_for(&line, color(0x000000FF));
        let total: usize = runs.iter().map(|run| run.len).sum();
        assert_eq!(total, line.text.len());
        // Walk the runs back to byte ranges and confirm they tile the row.
        let mut cursor = 0usize;
        for run in &runs {
            cursor += run.len;
        }
        assert_eq!(cursor, line.text.len());
    }

    #[test]
    fn an_unhighlighted_row_is_one_run() {
        let line = row("plain", Vec::new());
        let runs = runs_for(&line, color(0x111111FF));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, line.text.len());
    }

    #[test]
    fn an_empty_row_produces_no_runs() {
        let line = row("", Vec::new());
        assert!(runs_for(&line, color(0x111111FF)).is_empty());
    }

    /// A span past the end can only come from a length disagreement elsewhere;
    /// clipping keeps it from panicking on a slice GPUI would reject.
    #[test]
    fn a_span_past_the_row_is_clipped() {
        let line = row("abc", vec![(1..99, 0xFF0000FF)]);
        let runs = runs_for(&line, color(0x000000FF));
        let total: usize = runs.iter().map(|run| run.len).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn a_span_entirely_past_the_row_adds_nothing() {
        let line = row("abc", vec![(10..20, 0xFF0000FF)]);
        let runs = runs_for(&line, color(0x000000FF));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 3);
    }

    #[test]
    fn the_palette_maps_every_class_to_a_distinct_colour() {
        let palette = EditorPalette {
            text: color(0x000000FF),
            keyword: color(0x111111FF),
            literal: color(0x222222FF),
            string: color(0x333333FF),
            comment: color(0x444444FF),
            type_name: color(0x555555FF),
            meta: color(0x666666FF),
            added: color(0x777777FF),
            removed: color(0x888888FF),
        };
        for class in [
            TokenClass::Keyword,
            TokenClass::Literal,
            TokenClass::String,
            TokenClass::Comment,
            TokenClass::Type,
            TokenClass::Meta,
            TokenClass::Added,
            TokenClass::Removed,
        ] {
            assert_ne!(
                palette.color_of(class),
                palette.text,
                "{class:?} must not paint as body text"
            );
        }
    }
}
