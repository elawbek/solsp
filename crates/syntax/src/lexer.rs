//! Lexer: text -> a flat list of tokens, **including** trivia (whitespace and
//! comments) so the tree stays lossless (design §3.1). A single byte-cursor pass.

use crate::SyntaxError;
use crate::SyntaxKind::{self, *};
use rowan::TextRange;

/// A lexed token: its kind and byte length. Byte offsets are recovered by
/// accumulating `len` across the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub len: u32,
}

/// Tokenize the whole input. Total function: never panics, covers every byte.
/// Use [`tokenize_with_errors`] when lexical diagnostics are needed.
pub fn tokenize(text: &str) -> Vec<Token> {
    tokenize_with_errors(text).0
}

/// Tokenize while retaining lexical errors, including errors in trivia.
/// Malformed strings and comments keep their token kind for parser recovery.
pub fn tokenize_with_errors(text: &str) -> (Vec<Token>, Vec<SyntaxError>) {
    let mut cursor = Cursor::new(text);
    let mut tokens = Vec::new();
    while !cursor.is_eof() {
        tokens.push(cursor.next_token());
    }
    (tokens, cursor.errors)
}

/// Byte-cursor over the source. All scanning is forward-only.
struct Cursor<'a> {
    src: &'a str,
    pos: usize,
    errors: Vec<SyntaxError>,
}

impl<'a> Cursor<'a> {
    fn new(src: &'a str) -> Self {
        Cursor {
            src,
            pos: 0,
            errors: Vec::new(),
        }
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
        } else if c == '/' && self.second() == Some('/') {
            self.line_comment();
            COMMENT
        } else if c == '/' && self.second() == Some('*') {
            self.block_comment();
            COMMENT
        } else if c == '"' || c == '\'' {
            self.string_body(c, start);
            STRING
        } else {
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
        if (text == "hex" || text == "unicode") && matches!(self.first(), Some('"') | Some('\'')) {
            let quote = self.first().unwrap();
            self.string_body(quote, start);
            return STRING;
        }
        SyntaxKind::from_keyword(text).unwrap_or(IDENT)
    }

    /// Consume a quoted body starting at the opening quote. Handles `\` escapes;
    /// stops at the matching quote, a bare line break (recovery), or EOF (lossless).
    fn string_body(&mut self, quote: char, start: usize) {
        self.bump(); // opening quote
        while let Some(c) = self.first() {
            match c {
                '\\' => {
                    // Consume the backslash, then the escaped character.
                    // A CRLF continuation is one escaped line break.
                    self.bump();
                    if self.bump() == Some('\r') && self.first() == Some('\n') {
                        self.bump();
                    }
                }
                c if is_line_break(c) => break,
                c if c == quote => {
                    self.bump(); // closing quote
                    return;
                }
                _ => {
                    self.bump();
                }
            }
        }
        self.error(start, "unterminated string literal");
    }

    /// `// ...` to end of line (newline not included).
    fn line_comment(&mut self) {
        self.bump(); // /
        self.bump(); // /
        self.eat_while(|c| c != '\n');
    }

    /// `/* ... */`; runs to the closing `*/` or EOF (lossless).
    fn block_comment(&mut self) {
        let start = self.pos;
        self.bump(); // /
        self.bump(); // *
        while let Some(c) = self.bump() {
            if c == '*' && self.first() == Some('/') {
                self.bump(); // /
                return;
            }
        }
        self.error(start, "unterminated block comment");
    }

    fn error(&mut self, start: usize, message: &str) {
        self.errors.push(SyntaxError {
            message: message.to_owned(),
            range: TextRange::new((start as u32).into(), (self.pos as u32).into()),
        });
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

fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{000b}' | '\u{000c}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
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
        assert_eq!(
            total,
            src.len(),
            "tokens must cover the whole input: {src:?}"
        );
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
        assert_eq!(
            lex("()[]{};,.?:"),
            vec![
                (L_PAREN, "("),
                (R_PAREN, ")"),
                (L_BRACK, "["),
                (R_BRACK, "]"),
                (L_BRACE, "{"),
                (R_BRACE, "}"),
                (SEMICOLON, ";"),
                (COMMA, ","),
                (DOT, "."),
                (QUESTION, "?"),
                (COLON, ":"),
            ]
        );
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
        assert_eq!(
            lex("contract Foo"),
            vec![(CONTRACT_KW, "contract"), (WHITESPACE, " "), (IDENT, "Foo"),]
        );
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
        assert_eq!(
            lex("1.foo"),
            vec![(NUMBER, "1"), (DOT, "."), (IDENT, "foo")]
        );
    }

    #[test]
    fn strings() {
        assert_eq!(lex(r#""hello""#), vec![(STRING, r#""hello""#)]);
        assert_eq!(lex("'world'"), vec![(STRING, "'world'")]);
        assert_eq!(lex(r#""a\"b""#), vec![(STRING, r#""a\"b""#)]); // escaped quote
        assert_eq!(lex(r#"hex"00ff""#), vec![(STRING, r#"hex"00ff""#)]);
        assert_eq!(
            lex(r#"unicode"héllo""#),
            vec![(STRING, r#"unicode"héllo""#)]
        );
        // `hexx` is a plain identifier, not a hex-string prefix:
        assert_eq!(lex(r#"hexx"#), vec![(IDENT, "hexx")]);
    }

    #[test]
    fn unterminated_string_is_lossless() {
        // Runs to EOF; still a single STRING token covering everything.
        assert_eq!(lex("\"oops"), vec![(STRING, "\"oops")]);
        assert_lossless("\"oops");
    }

    #[test]
    fn comments() {
        assert_eq!(lex("// hi"), vec![(COMMENT, "// hi")]);
        assert_eq!(lex("/* a b */"), vec![(COMMENT, "/* a b */")]);
        assert_eq!(
            lex("a // tail\nb"),
            vec![
                (IDENT, "a"),
                (WHITESPACE, " "),
                (COMMENT, "// tail"),
                (WHITESPACE, "\n"),
                (IDENT, "b"),
            ]
        );
        // `/` alone is still the SLASH operator.
        assert_eq!(
            lex("a / b"),
            vec![
                (IDENT, "a"),
                (WHITESPACE, " "),
                (SLASH, "/"),
                (WHITESPACE, " "),
                (IDENT, "b"),
            ]
        );
    }

    #[test]
    fn unterminated_literals_report_exact_byte_ranges() {
        for literal in [
            "\"",
            "'oops",
            "\"oops\\",
            "\"oops\\\"",
            "hex\"00",
            "hex'00",
            "unicode\"привет",
            "unicode'🌍",
            "/*",
            "/* open",
            "/** doc",
            "/**",
            "/*/",
        ] {
            let prefix = "// 🌍\n ";
            let src = format!("{prefix}{literal}");
            let (tokens, errors) = tokenize_with_errors(&src);
            assert_eq!(errors.len(), 1, "{src:?}: {errors:?}");
            let (kind, message) = if literal.starts_with("/*") {
                (COMMENT, "unterminated block comment")
            } else {
                (STRING, "unterminated string literal")
            };
            assert_eq!(tokens.last().unwrap().kind, kind);
            assert_eq!(errors[0].message, message);
            assert_eq!(
                errors[0].range,
                TextRange::new((prefix.len() as u32).into(), (src.len() as u32).into())
            );
            assert_lossless(&src);
        }
    }

    #[test]
    fn unterminated_strings_recover_at_line_breaks() {
        for line_break in [
            "\n", "\r\n", "\r", "\u{000b}", "\u{000c}", "\u{0085}", "\u{2028}", "\u{2029}",
        ] {
            for literal in ["\"oops", "'oops", "hex\"00", "unicode\"🌍"] {
                let src = format!("{literal}{line_break}contract Next {{}}");
                let (tokens, errors) = tokenize_with_errors(&src);
                assert_eq!(errors.len(), 1, "{src:?}: {errors:?}");
                assert_eq!(u32::from(errors[0].range.end()) as usize, literal.len());
                assert_eq!(tokens[0].kind, STRING);
                assert_eq!(tokens[1].kind, WHITESPACE);
                assert_eq!(tokens[2].kind, CONTRACT_KW);
                assert_lossless(&src);
            }
        }
    }

    #[test]
    fn closed_literals_and_escaped_line_breaks_have_no_errors() {
        for src in [
            "\"\"",
            "''",
            "\"escaped\\\"quote\"",
            "'escaped\\'quote'",
            "\"backslash\\\\\"",
            "hex\"00ff\"",
            "hex'00ff'",
            "unicode\"привет 🌍\"",
            "unicode'привет'",
            "\"a\\\nb\"",
            "\"a\\\r\nb\"",
            "\"a\\\rb\"",
            "unicode\"🌍\\\r\nb\"",
            "//",
            "// no newline",
            "/**/",
            "/* closed */",
            "/** doc */",
            "/* \" * /\n */",
        ] {
            let (_, errors) = tokenize_with_errors(src);
            assert!(errors.is_empty(), "{src:?}: {errors:?}");
            assert_lossless(src);
        }
    }

    #[test]
    fn unterminated_block_comment_is_lossless() {
        assert_eq!(lex("/* open"), vec![(COMMENT, "/* open")]);
        assert_lossless("/* open");
    }

    const CORPUS: &[&str] = &[
        "",
        "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\n",
        r#"
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

contract Vault is Ownable {
    mapping(address => uint256) public balances;
    uint256 constant FEE = 1_000;

    /// deposit ether
    function deposit() external payable {
        balances[msg.sender] += msg.value; // track
        require(balances[msg.sender] <= 2**128, "overflow");
    }
}
"#,
        "function f(uint a, uint b) pure returns (uint) { return a ** b; }",
        "contract C { string s = unicode\"héllo 🌍\"; bytes h = hex\"00ff\"; }",
        "function id(uint x) pure returns (uint r) { assembly { r := x } }",
        "this is \u{00A7} not valid !! @# but must still round-trip",
    ];

    #[test]
    fn corpus_is_lossless_and_reconstructs() {
        for &src in CORPUS {
            // 1. Lengths tile the input.
            assert_lossless(src);
            // 2. Concatenating token texts reproduces the source byte-for-byte.
            let mut rebuilt = String::new();
            let mut pos = 0usize;
            for t in tokenize(src) {
                let end = pos + t.len as usize;
                rebuilt.push_str(&src[pos..end]);
                pos = end;
            }
            assert_eq!(rebuilt, src, "reconstruction mismatch for {src:?}");
        }
    }

    #[test]
    fn corpus_has_no_zero_length_tokens() {
        for &src in CORPUS {
            for t in tokenize(src) {
                assert!(t.len > 0, "zero-length token {:?} in {src:?}", t.kind);
            }
        }
    }
}
