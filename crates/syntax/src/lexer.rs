//! Lexer: text -> a flat list of tokens, **including** trivia (whitespace and
//! comments) so the tree stays lossless (design §3.1). A single byte-cursor pass.

use crate::SyntaxKind::{self, *};

/// A lexed token: its kind and byte length. Byte offsets are recovered by
/// accumulating `len` across the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub len: u32,
}

/// Tokenize the whole input. Total function: never panics, covers every byte.
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut cursor = Cursor::new(text);
    let mut tokens = Vec::new();
    while !cursor.is_eof() {
        tokens.push(cursor.next_token());
    }
    tokens
}

/// Byte-cursor over the source. All scanning is forward-only.
struct Cursor<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(src: &'a str) -> Self {
        Cursor { src, pos: 0 }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn first(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn second(&self) -> Option<char> {
        let mut it = self.rest().chars();
        it.next();
        it.next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.first()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat_while(&mut self, mut pred: impl FnMut(char) -> bool) {
        while let Some(c) = self.first() {
            if pred(c) {
                self.bump();
            } else {
                break;
            }
        }
    }

    /// Scan a single token starting at the cursor. Precondition: not EOF.
    fn next_token(&mut self) -> Token {
        let start = self.pos;
        let c = self.first().expect("next_token called at EOF");
        let kind = if is_whitespace(c) {
            self.eat_while(is_whitespace);
            WHITESPACE
        } else {
            // Later tasks add arms BEFORE this fallback (comments, idents, numbers,
            // strings, punctuation). For now, any non-whitespace byte is an error.
            self.bump();
            ERROR
        };
        Token {
            kind,
            len: (self.pos - start) as u32,
        }
    }
}

fn is_whitespace(c: char) -> bool {
    c.is_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntaxKind::*;

    /// Render tokens as (kind, text) pairs for readable assertions.
    fn lex(src: &str) -> Vec<(crate::SyntaxKind, &str)> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        for t in tokenize(src) {
            let end = pos + t.len as usize;
            out.push((t.kind, &src[pos..end]));
            pos = end;
        }
        out
    }

    /// The core invariant for EVERY lexer task: token lengths tile the input exactly.
    fn assert_lossless(src: &str) {
        let total: usize = tokenize(src).iter().map(|t| t.len as usize).sum();
        assert_eq!(total, src.len(), "tokens must cover the whole input: {src:?}");
    }

    #[test]
    fn empty_input_has_no_tokens() {
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn whitespace_is_one_token() {
        assert_eq!(lex("  \t\n "), vec![(WHITESPACE, "  \t\n ")]);
    }

    #[test]
    fn unknown_char_is_error_token() {
        assert_eq!(lex("\u{00A7}"), vec![(ERROR, "\u{00A7}")]); // § : not yet handled
    }

    #[test]
    fn lossless_on_mixed_garbage() {
        for s in ["", "   ", "\u{00A7}\u{00A7}", " \u{00A7} "] {
            assert_lossless(s);
        }
    }
}
