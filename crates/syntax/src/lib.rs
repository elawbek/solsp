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

use rowan::{GreenNode, GreenNodeBuilder, TextRange};

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

/// Parse Solidity source into a lossless syntax tree.
///
/// Total function: never panics, never fails. The returned tree always covers the
/// full input byte-for-byte (trivia included); problems are surfaced via
/// [`Parse::errors`].
pub fn parse(text: &str) -> Parse {
    // TODO(M1 §3): wire the real pipeline:
    //   let tokens = lexer::tokenize(text);
    //   let events = parser::parse_events(&tokens);
    //   let (green, errors) = build_tree(text, &tokens, events);
    //
    // Placeholder so ide/server can be wired end-to-end now: wrap the whole input
    // as a single ERROR token under SOURCE_FILE. Lossless, total, compiles.
    use rowan::Language;
    let mut builder = GreenNodeBuilder::new();
    builder.start_node(SolidityLanguage::kind_to_raw(SyntaxKind::SOURCE_FILE));
    if !text.is_empty() {
        builder.token(SolidityLanguage::kind_to_raw(SyntaxKind::ERROR), text);
    }
    builder.finish_node();
    Parse {
        green: builder.finish(),
        errors: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_total_and_lossless() {
        for src in ["", "contract C {}", "this is not solidity !!!"] {
            let parse = parse(src);
            // Tree always spans the full input (lossless invariant).
            assert_eq!(parse.syntax().text().to_string(), src);
        }
    }
}
