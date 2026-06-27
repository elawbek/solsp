//! Server-side document store. M1: a flat map of open documents, each holding its
//! text, latest parse, and a `LineIndex` for byte ↔ LSP-position mapping. Replaced
//! by a salsa database in M2 (design §5).

use dashmap::mapref::one::Ref;
use dashmap::DashMap;
use lsp_types::Url;
use solsp_ide::LineIndex;
use solsp_syntax::Parse;

/// One open document: source text, its current parse tree, and the line index built
/// from that text. All three are refreshed together on every edit (FULL sync).
pub struct Document {
    pub text: String,
    pub parse: Parse,
    pub line_index: LineIndex,
}

impl Document {
    pub fn new(text: String) -> Document {
        let parse = solsp_syntax::parse(&text);
        let line_index = LineIndex::new(&text);
        Document {
            text,
            parse,
            line_index,
        }
    }
}

/// All open documents, keyed by URI string. `DashMap` gives interior mutability, so
/// handlers take `&ServerState` (no `&mut`) — matching the future salsa database.
#[derive(Default)]
pub struct ServerState {
    documents: DashMap<String, Document>,
}

impl ServerState {
    /// Insert or replace a document, reparsing and rebuilding its line index.
    pub fn set(&self, uri: &Url, text: String) {
        self.documents.insert(uri.to_string(), Document::new(text));
    }

    /// Borrow an open document, or `None` if it is not open.
    pub fn get(&self, uri: &Url) -> Option<Ref<'_, String, Document>> {
        self.documents.get(uri.as_str())
    }

    /// Drop a document from the store (on `didClose`).
    pub fn remove(&self, uri: &Url) {
        self.documents.remove(uri.as_str());
    }
}
