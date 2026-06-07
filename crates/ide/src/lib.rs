//! `solsp-ide` — IDE features over the syntax tree.
//!
//! Every feature is a pure function `(&SyntaxNode, &LineIndex) -> bare data`. The
//! data is **not** in LSP types — `solsp-server` maps it to the protocol. This
//! keeps features trivially testable and decoupled from the wire format (design §4).

pub mod diagnostics;
pub mod document_symbols;
pub mod line_index;
pub mod semantic_tokens;

pub use line_index::{LineCol, LineIndex};
