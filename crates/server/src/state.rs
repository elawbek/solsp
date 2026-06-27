//! Server-side document store, now backed by the salsa database (M2 P6). Each open
//! file is a salsa [`SourceFile`] input; editing it (`set`) bumps the revision so
//! `parse` and downstream queries recompute only what changed. A per-file
//! [`LineIndex`] is cached alongside for fast position ↔ byte mapping.

use std::collections::{HashMap, HashSet};
use std::fs;

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

    /// On `didClose`, refresh the file from disk rather than dropping it: it may still
    /// be imported by open files, so cross-file resolution must keep seeing it (with
    /// the saved-on-disk content, discarding any unsaved editor edits). If it no longer
    /// exists on disk, drop it.
    pub fn reload_or_drop(&mut self, uri: &Url) {
        if let Ok(path) = uri.to_file_path() {
            if let Ok(text) = fs::read_to_string(&path) {
                self.set(uri, text);
                return;
            }
        }
        self.remove(uri);
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

    /// Load a file from disk into the db if it is not already tracked (a disk-loaded
    /// file behaves like an opened one for queries; an editor `didOpen` later replaces
    /// it). No-op on read failure.
    fn ensure_loaded(&mut self, uri: &Url) {
        if self.files.contains_key(uri.as_str()) {
            return;
        }
        if let Ok(path) = uri.to_file_path() {
            if let Ok(text) = fs::read_to_string(&path) {
                self.set(uri, text);
            }
        }
    }

    /// Follow `root_uri`'s relative-import graph and load every reachable file from
    /// disk into the db, so cross-file resolution can read them. Visited-guarded, so
    /// import cycles terminate.
    pub fn load_import_graph(&mut self, root_uri: &Url) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue = vec![root_uri.clone()];
        while let Some(uri) = queue.pop() {
            if !seen.insert(uri.to_string()) {
                continue;
            }
            for target in self.import_targets(&uri) {
                self.ensure_loaded(&target);
                queue.push(target);
            }
        }
    }

    /// The relative-import target URIs of a tracked file (empty if untracked).
    fn import_targets(&self, uri: &Url) -> Vec<Url> {
        let Some(file) = self.file(uri) else {
            return Vec::new();
        };
        let root = solsp_base_db::parse(&self.db, file).syntax();
        solsp_hir::imports::imports(&root)
            .iter()
            .filter_map(|imp| resolve_import_uri(uri, &imp.path))
            .collect()
    }
}

/// Resolve an import path against the importing file's URI into the target file URI.
/// Tries the path relative to the importing file's directory — covering `./X.sol`,
/// `../lib/Y.sol`, and bare `X.sol`. Returns `None` if that does not exist on disk
/// (remapped / package specifiers like `@openzeppelin/...` need a configured resolver,
/// a later step; they simply won't canonicalize here).
pub fn resolve_import_uri(base: &Url, path: &str) -> Option<Url> {
    if path.is_empty() {
        return None;
    }
    let base_path = base.to_file_path().ok()?;
    let dir = base_path.parent()?;
    let canonical = fs::canonicalize(dir.join(path)).ok()?;
    Url::from_file_path(canonical).ok()
}
