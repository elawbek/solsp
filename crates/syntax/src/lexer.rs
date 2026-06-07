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
        } else if is_ident_start(c) {
            self.ident_or_keyword()
        } else if c.is_ascii_digit() {
            self.number()
        } else if c == '"' || c == '\'' {
            self.string_body(c);
            STRING
        } else {
            // Later tasks insert comment arms ABOVE this line.
            self.punctuation()
        };
        Token {
            kind,
            len: (self.pos - start) as u32,
        }
    }

    /// Scan an identifier, then classify: keyword table wins, else `IDENT`.
    fn ident_or_keyword(&mut self) -> SyntaxKind {
        let start = self.pos;
        self.bump(); // is_ident_start char
        self.eat_while(is_ident_continue);
        let text = &self.src[start..self.pos];
        // `hex"..."` / `unicode"..."` are single string literals, not ident+string.
        if (text == "hex" || text == "unicode")
            && matches!(self.first(), Some('"') | Some('\''))
        {
            let quote = self.first().unwrap();
            self.string_body(quote);
            return STRING;
        }
        SyntaxKind::from_keyword(text).unwrap_or(IDENT)
    }

    /// Consume a quoted body starting at the opening quote. Handles `\` escapes;
    /// stops at the matching quote, a bare newline (recovery), or EOF (lossless).
    fn string_body(&mut self, quote: char) {
        self.bump(); // opening quote
        while let Some(c) = self.first() {
            match c {
                '\\' => {
                    self.bump(); // backslash
                    self.bump(); // escaped char (if any)
                }
                '\n' => break, // unterminated line; let the parser flag it
                c if c == quote => {
                    self.bump(); // closing quote
                    break;
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Scan a numeric literal: decimal/hex integer, optional fraction, optional
    /// exponent; `_` digit separators allowed. One `NUMBER` kind covers all forms.
    fn number(&mut self) -> SyntaxKind {
        // Hex: 0x / 0X
        if self.first() == Some('0') && matches!(self.second(), Some('x') | Some('X')) {
            self.bump(); // 0
            self.bump(); // x
            self.eat_while(|c| c.is_ascii_hexdigit() || c == '_');
            return NUMBER;
        }
        self.eat_while(|c| c.is_ascii_digit() || c == '_');
        // Fraction — only if a digit follows the dot (else the dot is the operator).
        if self.first() == Some('.') && self.second().is_some_and(|c| c.is_ascii_digit()) {
            self.bump(); // .
            self.eat_while(|c| c.is_ascii_digit() || c == '_');
        }
        // Exponent.
        if matches!(self.first(), Some('e') | Some('E')) {
            self.bump();
            if matches!(self.first(), Some('+') | Some('-')) {
                self.bump();
            }
            self.eat_while(|c| c.is_ascii_digit() || c == '_');
        }
        NUMBER
    }

    /// Longest-match punctuation/operator scan. The table is ordered longest-first
    /// so `<<=` wins over `<<` over `<`. Unknown bytes become a 1-char ERROR token.
    fn punctuation(&mut self) -> SyntaxKind {
        const OPS: &[(&str, SyntaxKind)] = &[
            // 3-char
            ("<<=", SHL_EQ),
            (">>=", SHR_EQ),
            // 2-char
            ("**", STAR2),
            ("==", EQ2),
            ("!=", NEQ),
            ("<=", LT_EQ),
            (">=", GT_EQ),
            ("&&", AMP2),
            ("||", PIPE2),
            ("<<", SHL),
            (">>", SHR),
            ("+=", PLUS_EQ),
            ("-=", MINUS_EQ),
            ("*=", STAR_EQ),
            ("/=", SLASH_EQ),
            ("%=", PERCENT_EQ),
            ("&=", AMP_EQ),
            ("|=", PIPE_EQ),
            ("^=", CARET_EQ),
            ("++", PLUS2),
            ("--", MINUS2),
            ("=>", FAT_ARROW),
            ("->", THIN_ARROW),
            (":=", COLON_EQ), // Yul / inline-assembly assignment
            // 1-char
            ("(", L_PAREN),
            (")", R_PAREN),
            ("[", L_BRACK),
            ("]", R_BRACK),
            ("{", L_BRACE),
            ("}", R_BRACE),
            (";", SEMICOLON),
            (",", COMMA),
            (".", DOT),
            ("?", QUESTION),
            (":", COLON),
            ("=", EQ),
            ("<", LT),
            (">", GT),
            ("+", PLUS),
            ("-", MINUS),
            ("*", STAR),
            ("/", SLASH),
            ("%", PERCENT),
            ("!", BANG),
            ("~", TILDE),
            ("&", AMP),
            ("|", PIPE),
            ("^", CARET),
        ];
        let rest = self.rest();
        for (op, kind) in OPS {
            if rest.starts_with(op) {
                self.pos += op.len();
                return *kind;
            }
        }
        // Unknown byte: consume one char so we always make progress.
        self.bump();
        ERROR
    }
}

fn is_whitespace(c: char) -> bool {
    c.is_whitespace()
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphanumeric()
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

    #[test]
    fn single_char_punct() {
        assert_eq!(lex("()[]{};,.?:"), vec![
            (L_PAREN, "("), (R_PAREN, ")"), (L_BRACK, "["), (R_BRACK, "]"),
            (L_BRACE, "{"), (R_BRACE, "}"), (SEMICOLON, ";"), (COMMA, ","),
            (DOT, "."), (QUESTION, "?"), (COLON, ":"),
        ]);
    }

    #[test]
    fn longest_match_operators() {
        assert_eq!(lex("<<="), vec![(SHL_EQ, "<<=")]);
        assert_eq!(lex("<<"), vec![(SHL, "<<")]);
        assert_eq!(lex("<"), vec![(LT, "<")]);
        assert_eq!(lex("=="), vec![(EQ2, "==")]);
        assert_eq!(lex("="), vec![(EQ, "=")]);
        assert_eq!(lex("**"), vec![(STAR2, "**")]);
        assert_eq!(lex("=>"), vec![(FAT_ARROW, "=>")]);
        assert_eq!(lex("->"), vec![(THIN_ARROW, "->")]);
        assert_eq!(lex(":="), vec![(COLON_EQ, ":=")]); // Yul assign, longest-match over ':'
        assert_eq!(lex(":"), vec![(COLON, ":")]);
    }

    #[test]
    fn operator_then_paren() {
        assert_eq!(lex("a"), vec![(IDENT, "a")]);
        assert_eq!(lex(">=("), vec![(GT_EQ, ">="), (L_PAREN, "(")]);
    }

    #[test]
    fn idents_and_keywords() {
        assert_eq!(lex("contract"), vec![(CONTRACT_KW, "contract")]);
        assert_eq!(lex("Foo_$bar1"), vec![(IDENT, "Foo_$bar1")]);
        assert_eq!(lex("uint256"), vec![(IDENT, "uint256")]); // elementary type = IDENT
        assert_eq!(lex("contractFoo"), vec![(IDENT, "contractFoo")]); // not a keyword
        assert_eq!(lex("contract Foo"), vec![
            (CONTRACT_KW, "contract"), (WHITESPACE, " "), (IDENT, "Foo"),
        ]);
    }

    #[test]
    fn numbers() {
        assert_eq!(lex("0"), vec![(NUMBER, "0")]);
        assert_eq!(lex("123"), vec![(NUMBER, "123")]);
        assert_eq!(lex("1_000_000"), vec![(NUMBER, "1_000_000")]);
        assert_eq!(lex("0xDEAD_beef"), vec![(NUMBER, "0xDEAD_beef")]);
        assert_eq!(lex("1.5"), vec![(NUMBER, "1.5")]);
        assert_eq!(lex("2e10"), vec![(NUMBER, "2e10")]);
        assert_eq!(lex("1.2e-3"), vec![(NUMBER, "1.2e-3")]);
        // A trailing dot with no digit is the DOT operator, not part of the number:
        assert_eq!(lex("1.foo"), vec![(NUMBER, "1"), (DOT, "."), (IDENT, "foo")]);
    }

    #[test]
    fn strings() {
        assert_eq!(lex(r#""hello""#), vec![(STRING, r#""hello""#)]);
        assert_eq!(lex("'world'"), vec![(STRING, "'world'")]);
        assert_eq!(lex(r#""a\"b""#), vec![(STRING, r#""a\"b""#)]); // escaped quote
        assert_eq!(lex(r#"hex"00ff""#), vec![(STRING, r#"hex"00ff""#)]);
        assert_eq!(lex(r#"unicode"héllo""#), vec![(STRING, r#"unicode"héllo""#)]);
        // `hexx` is a plain identifier, not a hex-string prefix:
        assert_eq!(lex(r#"hexx"#), vec![(IDENT, "hexx")]);
    }

    #[test]
    fn unterminated_string_is_lossless() {
        // Runs to EOF; still a single STRING token covering everything.
        assert_eq!(lex("\"oops"), vec![(STRING, "\"oops")]);
        assert_lossless("\"oops");
    }
}
