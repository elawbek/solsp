//! `solsp-syntax` — lossless parser for Solidity (rust-analyzer style).
//!
//! Pipeline (see design §3):
//! ```text
//! text -> lexer -> [tokens incl. trivia] -> parser -> [events] -> tree builder -> rowan green tree
//! ```
//! This crate is **pure**: it knows nothing about LSP or salsa. The single entry
//! point is [`parse`], a total function that never panics and never fails — errors
//! are reported in [`Parse::errors`] and the tree always spans the whole input.

mod event;
mod input;
mod syntax_kind;

pub mod ast;
pub mod lexer;
mod grammar;
pub mod parser;

pub use syntax_kind::SyntaxKind;

use rowan::{GreenNode, TextRange};

/// The rowan language marker for Solidity. Ties our [`SyntaxKind`] to rowan trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SolidityLanguage {}

impl rowan::Language for SolidityLanguage {
    type Kind = SyntaxKind;
    fn kind_from_raw(raw: rowan::SyntaxKind) -> SyntaxKind {
        SyntaxKind::from_u16(raw.0)
    }
    fn kind_to_raw(kind: SyntaxKind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind.to_u16())
    }
}

pub type SyntaxNode = rowan::SyntaxNode<SolidityLanguage>;
pub type SyntaxToken = rowan::SyntaxToken<SolidityLanguage>;
pub type SyntaxElement = rowan::SyntaxElement<SolidityLanguage>;

/// A syntax error with the source range it covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
}

/// The result of parsing: an immutable green tree plus any syntax errors.
#[derive(Debug, Clone)]
pub struct Parse {
    green: GreenNode,
    errors: Vec<SyntaxError>,
}

impl Parse {
    /// The typed-untyped root node of the tree.
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }
    pub fn errors(&self) -> &[SyntaxError] {
        &self.errors
    }
}

/// Parse Solidity source into a lossless syntax tree. Total: never panics, never
/// fails; problems are surfaced via [`Parse::errors`] and the tree spans the whole
/// input byte-for-byte.
pub fn parse(text: &str) -> Parse {
    let tokens = lexer::tokenize(text);
    let input = input::Input::new(&tokens);
    let mut p = parser::Parser::new(&input);
    grammar::source_file(&mut p);
    let events = p.finish();
    let (green, errors) = event::build_tree(text, &tokens, events);
    Parse { green, errors }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic tree dump for assertions: `KIND@start..end` per line, tokens
    /// show their text. Independent of rowan's own Debug formatting.
    fn debug_tree(text: &str) -> String {
        use std::fmt::Write;
        let node = parse(text).syntax();
        let mut out = String::new();
        fn go(out: &mut String, el: SyntaxElement, indent: usize) {
            for _ in 0..indent {
                out.push_str("  ");
            }
            match el {
                rowan::NodeOrToken::Node(n) => {
                    let r = n.text_range();
                    let _ = writeln!(out, "{:?}@{}..{}", n.kind(), u32::from(r.start()), u32::from(r.end()));
                    for c in n.children_with_tokens() {
                        go(out, c, indent + 1);
                    }
                }
                rowan::NodeOrToken::Token(t) => {
                    let r = t.text_range();
                    let _ = writeln!(out, "{:?}@{}..{} {:?}", t.kind(), u32::from(r.start()), u32::from(r.end()), t.text());
                }
            }
        }
        go(&mut out, rowan::NodeOrToken::Node(node), 0);
        out
    }

    #[test]
    fn parse_is_total_and_lossless() {
        for src in [
            "",
            "contract C {}",
            "this is not solidity !!!",
            "  \n// leading + trailing\ncontract C {}  \n",
            "// only a comment",
            "contract C { string s = unicode\"héllo 🌍\"; }",
            "contract C {",
        ] {
            assert_eq!(parse(src).syntax().text().to_string(), src);
        }
    }

    #[test]
    fn parses_pragma_and_contract() {
        let src = "pragma solidity ^0.8.20;\ncontract C {}";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        // Sanity on structure (not a brittle full snapshot): both items present.
        assert!(dump.contains("PRAGMA_DIRECTIVE@"));
        assert!(dump.contains("CONTRACT_DEF@"));
        assert!(dump.contains("NAME@"));
    }

    #[test]
    fn recovers_on_garbage_then_continues() {
        // Leading junk becomes ERROR nodes; the contract after it still parses.
        let src = "@@@ contract C {}";
        let p = parse(src);
        let dump = debug_tree(src);
        assert!(dump.contains("ERROR@"));
        assert!(dump.contains("CONTRACT_DEF@"));
        assert_eq!(p.syntax().text().to_string(), src); // still lossless
    }
}
