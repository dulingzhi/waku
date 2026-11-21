//! Syntax highlighting for the rope-backed editor, bridging Winc's lexer to
//! Waku's paint classes.
//!
//! ## Why a bridge rather than one or the other
//!
//! Waku already tokenizes code for its Markdown blocks (`md::highlight`), and
//! Winc brings a much wider language table with cross-line state (block
//! comments, triple-quoted strings, heredocs). For a file editor the wider table
//! wins: a `.tsx` file inside a block comment should stay coloured to its end,
//! which a per-line-only lexer cannot do.
//!
//! What Waku keeps is the *appearance*. `TokenKind` is mapped onto the existing
//! `TokenClass`, so a colour is still chosen by `md::render`'s palette and the
//! editor never grows a second set of hardcoded hues.
//!
//! ## The offset rule again
//!
//! Winc's `Token` carries **char** offsets within its line; Waku's `TokenClass`
//! spans are **byte** ranges. The conversion happens per line in
//! [`LineHighlighter::highlight`], so nothing downstream sees a char index —
//! the same discipline `winc_buffer` enforces for the document text.

use std::ops::Range;

use winc_syntax::{Language, LineState, Token as WincToken, TokenKind, highlight_line};

use crate::md::highlight::TokenClass;

/// One painted span: a byte range within its line, with the class to paint it.
#[derive(Clone, Debug, PartialEq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub class: TokenClass,
}

/// Per-file highlighter: the line-start state of every line reached so far, so
/// a block comment or heredoc carries across lines.
///
/// The state chain is this type's whole job. `highlight_line` takes the state at
/// the *start* of a line and returns the state at the start of the next, so a
/// caller drawing a viewport near the middle of a file needs the chain walked to
/// that point first. That is [`Self::ensure_through`], and it is why this type
/// holds a `Vec` of states rather than asking Winc's `Highlighter` about a single
/// line: a one-line view cannot say what a multi-line construct opened earlier.
///
/// `Clone` so a paint pass can work on its own copy: painting must extend the
/// chain, and the paint path holds only a shared borrow of the editor.
#[derive(Clone)]
pub struct LineHighlighter {
    language: &'static Language,
    /// `states[i]` is the state at the start of line `i`; always holds line 0.
    states: Vec<LineState>,
    chars: Vec<char>,
    tokens: Vec<WincToken>,
}

impl LineHighlighter {
    /// Build a highlighter from a language id.
    ///
    /// The editor builds its highlighters from a *file name* (`for_file_name`),
    /// because that is what lets Winc and the document agree on the language.
    /// This exists for callers that already know the id, which today means the
    /// tests below: they need one specific language rather than whatever a name
    /// happens to resolve to.
    ///
    /// An unknown id — a language Winc does not carry, or plain text — yields
    /// `None`, and the caller then paints uncoloured text. That is the honest
    /// outcome: guessing a language would colour code wrongly, which reads as a
    /// bug in the code rather than as a missing feature.
    #[allow(dead_code, reason = "the tests build highlighters by language id")]
    pub fn for_language(id: &str) -> Option<Self> {
        let language = winc_syntax::find(id)?;
        Some(Self {
            language,
            states: vec![LineState::Normal],
            chars: Vec::new(),
            tokens: Vec::new(),
        })
    }

    /// Build a highlighter for a file, letting Winc decide what language it is.
    ///
    /// The same `detect` call the document itself makes, so the colours and the
    /// document's own idea of its language cannot disagree — which they would
    /// if this side did its own extension matching.
    pub fn for_file_name(file_name: &str) -> Option<Self> {
        let language = winc_syntax::detect(file_name);
        // Winc answers with its plain-text rules rather than `None` for a name
        // it does not know. Those rules classify nothing, and painting with them
        // would be the same as painting uncoloured, so the caller is told there
        // is nothing to do. `detect` on a name no language claims returns
        // exactly that rule set, which is how it is recognised here.
        let plain = winc_syntax::detect("waku-no-such-file");
        if std::ptr::eq(language, plain) {
            return None;
        }
        Some(Self {
            language,
            states: vec![LineState::Normal],
            chars: Vec::new(),
            tokens: Vec::new(),
        })
    }

    /// Discard state from `line` onward, after an edit changed it.
    ///
    /// Required for correctness, not speed: a `/*` that an edit removed must stop
    /// colouring the lines below it, and only this drops the cached states that
    /// said otherwise.
    pub fn invalidate_from(&mut self, line: usize) {
        self.states.truncate(line + 1);
    }

    /// The state at the start of `line`, or `None` when the chain has not
    /// reached it yet.
    pub fn known_state(&self, line: usize) -> Option<LineState> {
        self.states.get(line).copied()
    }

    /// Walk the state chain until `line` has a known starting state.
    ///
    /// `line_at` returns a line's text without its newline. Rows past the end
    /// clamp, so a caller that draws one row beyond the file — a trailing empty
    /// line — does not force a special case.
    ///
    /// Returns the state if the chain now reaches `line`, `None` when the file
    /// ends first.
    pub fn ensure_through(
        &mut self,
        line: usize,
        mut line_at: impl FnMut(usize) -> Option<String>,
    ) -> Option<LineState> {
        while self.states.len() <= line {
            let index = self.states.len() - 1;
            let text = line_at(index)?;
            let state = self.states[index];
            let next = self.scan(&text, state);
            self.states.push(next);
        }
        self.states.get(line).copied()
    }

    /// Highlight one line whose starting state is already known.
    ///
    /// `line_text` is the line's content without its newline. The returned spans
    /// cover only the parts the lexer classified; unmatched text is left for the
    /// caller's default foreground, which is how `md::highlight` treats `Text`.
    ///
    /// An unknown starting state means [`Self::ensure_through`] was not called;
    /// the line is then drawn uncoloured rather than guessed as plain code, since
    /// a wrong guess inside a string or block comment is worse than no colour.
    pub fn highlight(&mut self, line: usize, line_text: &str) -> Vec<HighlightSpan> {
        let Some(start_state) = self.known_state(line) else {
            return Vec::new();
        };
        self.chars.clear();
        self.chars.extend(line_text.chars());
        highlight_line(self.language, &self.chars, start_state, &mut self.tokens);

        let mut spans = Vec::with_capacity(self.tokens.len());
        for token in &self.tokens {
            let Some(class) = class_of(token.kind) else {
                continue;
            };
            let range = byte_range(line_text, token.start, token.end);
            if !range.is_empty() {
                spans.push(HighlightSpan { range, class });
            }
        }
        // Keep the chain in step when a caller highlights without having walked
        // it first, so the next line still starts from the right state.
        if self.states.len() == line + 1 {
            let next = self.scan(line_text, start_state);
            self.states.push(next);
        }
        spans
    }

    /// The state at the start of the line after `line_text`.
    fn scan(&mut self, line_text: &str, state: LineState) -> LineState {
        self.chars.clear();
        self.chars.extend(line_text.chars());
        highlight_line(self.language, &self.chars, state, &mut self.tokens)
    }
}

/// Translate a char range within `line` into a byte range.
fn byte_range(line: &str, start: usize, end: usize) -> Range<usize> {
    let start = char_to_byte(line, start);
    let end = char_to_byte(line, end).max(start);
    start..end
}

fn char_to_byte(line: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    match line.char_indices().nth(char_idx) {
        Some((byte, _)) => byte,
        None => line.len(),
    }
}

/// Map Winc's token kinds onto the classes Waku already paints.
///
/// `Text` returns `None`: it means "nothing matched", and painting it would
/// override the editor's foreground with a plain-text colour.
fn class_of(kind: TokenKind) -> Option<TokenClass> {
    Some(match kind {
        TokenKind::Text => return None,
        TokenKind::Keyword => TokenClass::Keyword,
        TokenKind::Builtin => TokenClass::Literal,
        TokenKind::Type => TokenClass::Type,
        TokenKind::Function => TokenClass::Function,
        TokenKind::String | TokenKind::Escape => TokenClass::String,
        TokenKind::Number => TokenClass::Number,
        TokenKind::Comment | TokenKind::Warning => TokenClass::Comment,
        // Attributes, decorators, tags and markup structure all read as "meta"
        // in Waku's palette, which is the closest existing class.
        TokenKind::Attribute
        | TokenKind::Decorator
        | TokenKind::Tag
        | TokenKind::Heading
        | TokenKind::Emphasis
        | TokenKind::Strong
        | TokenKind::Link => TokenClass::Meta,
        // Identifiers whose colour the palette has no class for keep the
        // editor's own foreground rather than borrowing a keyword's colour.
        TokenKind::Property | TokenKind::Variable => return None,
        TokenKind::Inserted => TokenClass::Added,
        TokenKind::Deleted => TokenClass::Removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(id: &str, line: &str) -> Vec<(Range<usize>, TokenClass)> {
        let mut hl = LineHighlighter::for_language(id).expect("language should exist");
        hl.highlight(0, line)
            .into_iter()
            .map(|span| (span.range, span.class))
            .collect()
    }

    #[test]
    fn a_rust_keyword_is_classified_and_sliced_by_byte() {
        let line = "fn main() {}";
        let found = spans("rust", line);
        let (_, class) = found
            .iter()
            .find(|(range, _)| &line[range.clone()] == "fn")
            .expect("`fn` should be a token");
        assert_eq!(*class, TokenClass::Keyword);
    }

    /// The conversion this module exists for: token offsets are char-based, so a
    /// line with CJK before a token must not shift that token's byte range.
    #[test]
    fn char_offsets_become_byte_offsets_on_a_cjk_line() {
        let line = "// 中文注释\nfn main() {}";
        let line = line.split('\n').next().unwrap();
        let found = spans("rust", line);
        // The whole line is a comment; its byte range must cover all 15 bytes,
        // not the 11 chars the lexer counted.
        let comment = found
            .iter()
            .find(|(_, class)| *class == TokenClass::Comment)
            .expect("a comment token");
        assert_eq!(
            line[comment.0.clone()].to_owned(),
            line,
            "the byte range must reproduce the line's bytes"
        );
    }

    #[test]
    fn text_between_tokens_is_not_painted() {
        let line = "let x = 1;";
        let found = spans("rust", line);
        // Whitespace and `=` are unclassified, so no span may start at byte 3
        // (the space after `let`).
        assert!(
            !found.iter().any(|(range, _)| range.start == 3),
            "unclassified text must keep the default foreground: {found:?}"
        );
    }

    #[test]
    fn an_unknown_language_yields_no_highlighter() {
        assert!(LineHighlighter::for_language("klingon").is_none());
    }

    #[test]
    fn a_known_language_is_found_by_wakus_id() {
        for id in ["rust", "json", "yaml", "toml", "typescript"] {
            assert!(
                LineHighlighter::for_language(id).is_some(),
                "Waku names `{id}`, so Winc must carry it"
            );
        }
    }

    #[test]
    fn a_block_comment_carries_state_across_lines() {
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        let first = hl.highlight(0, "/* start");
        assert!(first.iter().any(|s| s.class == TokenClass::Comment));
        // Line 1 keeps the state line 0 opened, so it is comment even though a
        // per-line lexer alone would call it code.
        assert!(
            hl.highlight(1, "still comment */")
                .iter()
                .any(|s| s.class == TokenClass::Comment),
            "state from the previous line must colour this one"
        );
    }

    #[test]
    fn invalidating_drops_the_state_an_edit_removed() {
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        let _ = hl.highlight(0, "/* open");
        // The edit removed the comment, so line 0 is code again and line 1 must
        // no longer be treated as its body.
        hl.invalidate_from(0);
        let after = hl.highlight(0, "fn main() {}");
        assert!(
            after.iter().any(|s| s.class == TokenClass::Keyword),
            "the stale comment state must not survive invalidation"
        );
    }

    #[test]
    fn ensure_through_walks_the_chain_to_a_viewport_far_below() {
        // A viewport opened at line 3 without walking lines 0..3 first cannot
        // know they opened a block comment.
        let lines = ["/* a", "b", "c", "d */", "fn main() {}"];
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        let state = hl
            .ensure_through(3, |i| lines.get(i).map(|s| (*s).to_owned()))
            .expect("line 3 exists");
        assert_ne!(
            state,
            LineState::Normal,
            "three lines of open comment must reach line 3 as comment state"
        );
        // Line 3 is the comment's last row, so it still paints as comment even
        // though the state after it returns to Normal.
        let spans = hl.highlight(3, lines[3]);
        assert!(
            spans.iter().any(|s| s.class == TokenClass::Comment),
            "the closing line of a block comment is still comment: {spans:?}"
        );
    }

    #[test]
    fn a_line_after_the_comment_closes_is_code_again() {
        let lines = ["/* a", "d */", "fn main() {}"];
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        let state = hl
            .ensure_through(2, |i| lines.get(i).map(|s| (*s).to_owned()))
            .expect("line 2 exists");
        assert_eq!(state, LineState::Normal, "the comment closed on line 1");
        assert!(
            hl.highlight(2, lines[2])
                .iter()
                .any(|s| s.class == TokenClass::Keyword)
        );
    }

    #[test]
    fn ensure_through_past_the_last_line_returns_none() {
        let lines = ["one"];
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        assert!(
            hl.ensure_through(5, |i| lines.get(i).map(|s| (*s).to_owned()))
                .is_none()
        );
    }

    #[test]
    fn highlighting_without_a_known_state_paints_nothing() {
        let mut hl = LineHighlighter::for_language("rust").expect("rust");
        // Line 4 is three lines past what the chain has reached.
        assert!(hl.highlight(4, "fn main() {}").is_empty());
    }
}
