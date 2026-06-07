//! Document symbols (outline): walk the tree, collect declarations into a
//! hierarchy `contract -> { functions, state vars, structs, events, ... }`. No name
//! resolution needed — pure tree shape (design §4, feature 2).

use rowan::TextRange;
use solsp_syntax::SyntaxNode;

/// Kind of an outline symbol (maps to LSP `SymbolKind` in the server).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Contract,
    Interface,
    Library,
    Function,
    StateVariable,
    Struct,
    Enum,
    Event,
    Error,
}

/// A nested outline symbol.
#[derive(Debug, Clone)]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Full range of the declaration.
    pub range: TextRange,
    /// Range of just the name (where the cursor lands).
    pub selection_range: TextRange,
    pub children: Vec<DocumentSymbol>,
}

/// Build the outline for a file.
///
/// TODO(M1 §4): walk `root`, match declaration nodes, build the nested symbols.
pub fn document_symbols(_root: &SyntaxNode) -> Vec<DocumentSymbol> {
    Vec::new()
}
