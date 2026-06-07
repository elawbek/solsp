//! Semantic tokens: walk the tree and classify each identifier token by its
//! **syntactic** position (contract name -> type, callee -> function, type in
//! `mapping(...)` -> type, ...). No name resolution in M1 — that sharpens this in
//! M2 (e.g. state var vs local) (design §4, feature 3).

use rowan::TextRange;
use solsp_syntax::SyntaxNode;

/// Token classification (maps to the LSP semantic-tokens legend in the server).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    Keyword,
    Type,
    Function,
    Variable,
    Parameter,
    Property,
    Number,
    String,
    Comment,
}

/// One classified token span.
#[derive(Debug, Clone, Copy)]
pub struct SemanticToken {
    pub range: TextRange,
    pub token_type: TokenType,
}

/// Classify every relevant token in the file.
///
/// TODO(M1 §4): walk `root`, classify identifier/keyword/literal tokens by the
/// shape of their parent nodes. Server encodes the result into LSP delta form.
pub fn semantic_tokens(_root: &SyntaxNode) -> Vec<SemanticToken> {
    Vec::new()
}
