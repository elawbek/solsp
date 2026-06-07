//! Parser output: a flat event stream the tree builder replays into a rowan green
//! tree. Producing events (not nodes) decouples parsing from tree construction,
//! which is what makes error recovery tractable (design §3.3).

use crate::SyntaxKind;

#[derive(Debug)]
#[allow(dead_code)] // consumers (Parser, build_tree) land in later tasks of this plan
pub(crate) enum Event {
    /// Open a node. `forward_parent` (if set) is the *relative* index of a later
    /// `Start` that should become this node's parent — the mechanism behind
    /// `CompletedMarker::precede` (used for left-associative expressions later).
    Start {
        kind: SyntaxKind,
        forward_parent: Option<u32>,
    },
    /// Close the innermost open node.
    Finish,
    /// Attach the next non-trivia input token as a leaf of `kind`.
    Token { kind: SyntaxKind },
    /// Record a syntax error at the current position.
    Error { msg: String },
    /// An abandoned/placeholder slot. Skipped by the tree builder.
    Tombstone,
}
