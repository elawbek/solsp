//! Recursive-descent parser. It does **not** build the tree directly — it emits a
//! flat event stream that a tree builder later replays into rowan. Decoupling
//! "parser ⊥ tree" is what enables error recovery (design §3.3).

use crate::SyntaxKind;

/// One step in the parse. The tree builder consumes a `Vec<Event>` into a green tree.
#[derive(Debug, Clone)]
pub enum Event {
    /// Open a new node of `kind`.
    Start { kind: SyntaxKind },
    /// Attach the next input token (advancing the cursor) as a leaf of `kind`.
    Token { kind: SyntaxKind },
    /// Close the innermost open node.
    Finish,
    /// Record a syntax error at the current position.
    Error { message: String },
}

/// Run the parser over lexed tokens, producing events.
///
/// TODO(M1 §3.3): recursive descent with recovery sets (skip to `}`/`;`/next decl
/// on unexpected input) and Pratt/precedence-climbing for expressions.
pub fn parse_events(_tokens: &[crate::lexer::Token]) -> Vec<Event> {
    Vec::new()
}
