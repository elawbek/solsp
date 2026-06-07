//! Server-side document store. M1: a flat map of open documents, each holding its
//! text and latest parse. Replaced by a salsa database in M2 (design §5).

use dashmap::DashMap;
use solsp_syntax::Parse;

/// One open document: source text and its current parse tree.
pub struct Document {
    pub text: String,
    pub parse: Parse,
}

impl Document {
    pub fn new(text: String) -> Document {
        let parse = solsp_syntax::parse(&text);
        Document { text, parse }
    }
}

/// All open documents, keyed by URI string.
#[derive(Default)]
pub struct ServerState {
    pub documents: DashMap<String, Document>,
}
