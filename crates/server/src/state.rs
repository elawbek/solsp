//! Server-side document store, now backed by the salsa database (M2 P6). Each open
//! file is a salsa [`SourceFile`] input; editing it (`set`) bumps the revision so
//! `parse` and downstream queries recompute only what changed. A per-file
//! [`LineIndex`] is cached alongside for fast position ↔ byte mapping.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use lsp_types::Url;
use salsa::Setter;
use solsp_base_db::{RootDatabase, SourceFile};
use solsp_hir::imports::ImportKind;
use solsp_hir::resolve::Definition;
use solsp_ide::LineIndex;

struct FileEntry {
    file: SourceFile,
    line_index: LineIndex,
}

/// One of a file's imports with its target already resolved to a URI (the filesystem
/// lookup is the expensive part, so it is done once and cached).
pub struct ResolvedImport {
    pub kind: ImportKind,
    pub target: Option<Url>,
}

/// A file's cross-file resolution surface, computed once and reused until the file
/// changes: its top-level definitions and its resolved imports. Caching this turns the
/// per-query tree walks and filesystem probes (which dominated big-file latency) into
/// hashmap lookups.
pub struct FileIndex {
    pub defs: Vec<Definition>,
    pub imports: Vec<ResolvedImport>,
}

/// All open documents plus the salsa database they live in. The main loop is
/// single-threaded, so mutations take `&mut self` and reads take `&self`.
#[derive(Default)]
pub struct ServerState {
    db: RootDatabase,
    files: HashMap<String, FileEntry>,
    /// Per-file [`FileIndex`] memo; an entry is dropped when its file is `set` (its tree
    /// changed). `RefCell` because queries read through `&self`.
    index_cache: RefCell<HashMap<String, Rc<FileIndex>>>,
}

impl ServerState {
    /// Open or replace a document with `text`: update its salsa input (reusing the
    /// same `SourceFile` handle on re-set, so the revision bump invalidates exactly
    /// its dependents) and rebuild its line index.
    pub fn set(&mut self, uri: &Url, text: String) {
        let key = uri.to_string();
        let line_index = LineIndex::new(&text);
        // the file's tree changes → its cached index is stale.
        self.index_cache.borrow_mut().remove(&key);
        if let Some(file) = self.files.get(&key).map(|e| e.file) {
            file.set_text(&mut self.db).to(text);
            self.files.insert(key, FileEntry { file, line_index });
        } else {
            let file = SourceFile::new(&self.db, key.clone(), text);
            self.files.insert(key, FileEntry { file, line_index });
        }
    }

    /// The cached [`FileIndex`] for a tracked file (built on first use). `None` if the
    /// file is not tracked.
    pub fn file_index(&self, uri: &Url) -> Option<Rc<FileIndex>> {
        let key = uri.to_string();
        if let Some(idx) = self.index_cache.borrow().get(&key) {
            return Some(idx.clone());
        }
        let file = self.file(uri)?;
        let root = solsp_base_db::parse(&self.db, file).syntax();
        let imports = solsp_hir::imports::imports(&root)
            .iter()
            .map(|imp| ResolvedImport {
                kind: imp.kind.clone(),
                target: resolve_import_uri(uri, &imp.path),
            })
            .collect();
        let idx = Rc::new(FileIndex {
            defs: solsp_hir::resolve::file_definitions(&root),
            imports,
        });
        self.index_cache.borrow_mut().insert(key, idx.clone());
        Some(idx)
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

/// Resolve an import path against the importing file's URI into the target file URI,
/// trying in order: relative to the importing file (`./X.sol`, `../Y.sol`, bare
/// `X.sol`); remappings (`remappings.txt` + `foundry.toml`) from the project root; then
/// `node_modules/<path>` or forge `lib/<path>`. `None` if nothing resolves to a file.
pub fn resolve_import_uri(base: &Url, path: &str) -> Option<Url> {
    if path.is_empty() {
        return None;
    }
    let base_path = base.to_file_path().ok()?;
    let dir = base_path.parent()?;

    // 1. relative to the importing file.
    if let Some(uri) = file_uri(dir.join(path)) {
        return Some(uri);
    }

    // 2 & 3. package / remapped imports, resolved against the project root.
    let root = project_root(dir)?;
    for (prefix, target) in load_remappings(&root) {
        if let Some(rest) = path.strip_prefix(&prefix) {
            if let Some(uri) = file_uri(root.join(&target).join(rest)) {
                return Some(uri);
            }
        }
    }
    // 4. project-root-relative — Foundry resolves a bare `contracts/X.sol` from the
    //    project root (`src = 'contracts'`), and relative to common source dirs.
    if let Some(uri) = file_uri(root.join(path)) {
        return Some(uri);
    }
    for src in ["src", "contracts"] {
        if let Some(uri) = file_uri(root.join(src).join(path)) {
            return Some(uri);
        }
    }
    // 5. package roots.
    for base_dir in ["node_modules", "lib"] {
        if let Some(uri) = file_uri(root.join(base_dir).join(path)) {
            return Some(uri);
        }
    }
    None
}

/// Canonicalize an existing **file** path into a `file://` URL (None if missing/a dir).
fn file_uri(path: PathBuf) -> Option<Url> {
    let canonical = fs::canonicalize(path).ok()?;
    if !canonical.is_file() {
        return None;
    }
    Url::from_file_path(canonical).ok()
}

/// The project root: the nearest ancestor of `start` carrying a `remappings.txt` /
/// `foundry.toml` / `node_modules` / `.git` marker.
fn project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| {
            d.join("remappings.txt").exists()
                || d.join("foundry.toml").exists()
                || d.join("node_modules").is_dir()
                || d.join(".git").exists()
        })
        .map(Path::to_path_buf)
}

/// Import remappings (`prefix=target`) read from `remappings.txt` and `foundry.toml`.
/// (Re-read per call; the files are small and OS-cached. Caching can come later.)
fn load_remappings(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(text) = fs::read_to_string(root.join("remappings.txt")) {
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((p, t)) = line.split_once('=') {
                out.push((p.to_string(), t.to_string()));
            }
        }
    }
    if let Ok(text) = fs::read_to_string(root.join("foundry.toml")) {
        // remappings appear as quoted "prefix=target" entries; a path target contains a
        // `/`, which filters out unrelated quoted strings (versions, names).
        for token in text.split('"').skip(1).step_by(2) {
            if let Some((p, t)) = token.split_once('=') {
                if !p.is_empty() && t.contains('/') {
                    out.push((p.to_string(), t.to_string()));
                }
            }
        }
    }
    out
}
