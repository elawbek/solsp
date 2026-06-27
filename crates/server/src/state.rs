//! Server-side document store, now backed by the salsa database (M2 P6). Each open
//! file is a salsa [`SourceFile`] input; editing it (`set`) bumps the revision so
//! `parse` and downstream queries recompute only what changed. A per-file
//! [`LineIndex`] is cached alongside for fast position ↔ byte mapping.

use std::collections::HashMap;

use lsp_types::Url;
use salsa::Setter;
use solsp_base_db::{RootDatabase, SourceFile};
use solsp_ide::LineIndex;

struct FileEntry {
    file: SourceFile,
    line_index: LineIndex,
}

/// All open documents plus the salsa database they live in. The main loop is
/// single-threaded, so mutations take `&mut self` and reads take `&self`.
#[derive(Default)]
pub struct ServerState {
    db: RootDatabase,
    files: HashMap<String, FileEntry>,
}

impl ServerState {
    /// Open or replace a document with `text`: update its salsa input (reusing the
    /// same `SourceFile` handle on re-set, so the revision bump invalidates exactly
    /// its dependents) and rebuild its line index.
    pub fn set(&mut self, uri: &Url, text: String) {
        let key = uri.to_string();
        let line_index = LineIndex::new(&text);
        if let Some(file) = self.files.get(&key).map(|e| e.file) {
            file.set_text(&mut self.db).to(text);
            self.files.insert(key, FileEntry { file, line_index });
        } else {
            let file = SourceFile::new(&self.db, key.clone(), text);
            self.files.insert(key, FileEntry { file, line_index });
        }
    }

    /// Drop a document (on `didClose`).
    pub fn remove(&mut self, uri: &Url) {
        self.files.remove(uri.as_str());
    }

    /// The salsa database (read-only access for queries).
    pub fn db(&self) -> &RootDatabase {
        &self.db
    }

    /// The `SourceFile` input handle for an open document.
    pub fn file(&self, uri: &Url) -> Option<SourceFile> {
        self.files.get(uri.as_str()).map(|e| e.file)
    }

    /// The cached line index for an open document.
    pub fn line_index(&self, uri: &Url) -> Option<&LineIndex> {
        self.files.get(uri.as_str()).map(|e| &e.line_index)
    }

    /// A snapshot of an open document's current text (for applying incremental edits).
    pub fn text(&self, uri: &Url) -> Option<String> {
        let file = self.file(uri)?;
        Some(file.text(&self.db).clone())
    }
}
