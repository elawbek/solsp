//! Lexer: text -> a flat list of tokens, **including** trivia (whitespace and
//! comments) so the tree stays lossless (design §3.1).

use crate::SyntaxKind;

/// A lexed token: its kind and byte length. Offsets are recovered by accumulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub len: u32,
}

/// Tokenize the whole input.
///
/// TODO(M1 §3.1): real lexer covering identifiers, Solidity keywords, integer/
/// hex/scientific numbers, string/hex/unicode literals, and line/block comments.
pub fn tokenize(_text: &str) -> Vec<Token> {
    Vec::new()
}
