//! `solsp-base-db` — the salsa incremental-computation foundation for M2 analysis.
//!
//! `solsp-syntax` stays pure (no salsa); this crate *wraps* it: source text is a
//! salsa [`SourceFile`] input, and parsing is the [`parse`] tracked query so every
//! downstream analysis (HIR, scopes, resolution) is memoized and incrementally
//! recomputed (design §2, §7). salsa 0.27 API.

use rowan::GreenNode;
use solsp_syntax::{SyntaxError, SyntaxNode};

/// The database interface that analysis code depends on. Extended with query groups
/// as M2 grows; for now it is just the salsa marker.
#[salsa::db]
pub trait Db: salsa::Database {}

/// The concrete database. `salsa::Storage` holds the memo tables; cloning is cheap
/// (the storage is reference-counted) and yields a second handle onto the same data.
#[salsa::db]
#[derive(Clone, Default)]
pub struct RootDatabase {
    storage: salsa::Storage<Self>,
}

#[salsa::db]
impl salsa::Database for RootDatabase {}

#[salsa::db]
impl Db for RootDatabase {}

/// One source file as a salsa input: its identity (`path`) and current `text`.
/// Editing a file is `file.set_text(&mut db).to(new_text)`; salsa then invalidates
/// exactly the queries that read it. The server (M2 P8) keeps the path → `SourceFile`
/// map so the same input handle is reused across edits.
#[salsa::input]
pub struct SourceFile {
    #[returns(ref)]
    pub path: String,
    #[returns(ref)]
    pub text: String,
}

/// A salsa-storable parse result: the immutable green tree plus its syntax errors.
/// `solsp_syntax::Parse` is not salsa-aware (the syntax crate stays pure), so we
/// re-wrap it here. The `GreenNode` is `Send + Sync`; a (`!Send`) `SyntaxNode` is
/// rebuilt on demand via [`SolParse::syntax`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolParse {
    green: GreenNode,
    errors: Vec<SyntaxError>,
}

impl SolParse {
    /// The typed-untyped root node (rebuilt from the green tree each call).
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    /// The syntax errors collected while parsing.
    pub fn errors(&self) -> &[SyntaxError] {
        &self.errors
    }
}

// salsa needs `Update` to decide whether a recomputed value differs from the memoized
// one. `SolParse` is a fully-owned immutable value compared by equality, so the
// canonical "fallback" impl is correct: overwrite iff different, reporting the change.
// SAFETY: `old_pointer` points to a valid, fully-owned `SolParse`; we only read it via
// `Eq` and overwrite it in place — no borrowed/`'db` data is involved.
unsafe impl salsa::Update for SolParse {
    unsafe fn maybe_update(old_pointer: *mut Self, new_value: Self) -> bool {
        let old = unsafe { &mut *old_pointer };
        if *old == new_value {
            false
        } else {
            *old = new_value;
            true
        }
    }
}

/// Parse a source file's text into a lossless tree. Tracked: re-runs only when the
/// file's `text` changes, and memoizes otherwise.
#[salsa::tracked]
pub fn parse(db: &dyn Db, file: SourceFile) -> SolParse {
    let parsed = solsp_syntax::parse(file.text(db));
    SolParse {
        green: parsed.syntax().green().into_owned(),
        errors: parsed.errors().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use salsa::Setter;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Counts executions of `error_count` (below) so tests can observe salsa's
    // memoization: the counter only advances when the query genuinely re-runs.
    static RUNS: AtomicUsize = AtomicUsize::new(0);

    /// A downstream tracked query over [`parse`]; the side-effecting counter is a
    /// test-only probe for "did this actually recompute?".
    #[salsa::tracked]
    fn error_count(db: &dyn Db, file: SourceFile) -> usize {
        RUNS.fetch_add(1, Ordering::SeqCst);
        parse(db, file).errors().len()
    }

    #[test]
    fn parse_produces_tree_and_errors() {
        let db = RootDatabase::default();
        let f = SourceFile::new(&db, "x.sol".to_string(), "contract C {}".to_string());
        let p = parse(&db, f);
        assert!(p.errors().is_empty());
        assert_eq!(p.syntax().kind(), solsp_syntax::SyntaxKind::SOURCE_FILE);

        let bad = SourceFile::new(&db, "y.sol".to_string(), "@@@ contract".to_string());
        assert!(!parse(&db, bad).errors().is_empty());
    }

    #[test]
    fn parse_is_memoized_and_incremental() {
        let mut db = RootDatabase::default();
        let f1 = SourceFile::new(&db, "a.sol".to_string(), "contract C {}".to_string());
        let f2 = SourceFile::new(&db, "b.sol".to_string(), "contract D {}".to_string());

        RUNS.store(0, Ordering::SeqCst);

        // first query executes
        assert_eq!(error_count(&db, f1), 0);
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);

        // re-query without changes → memoized, no re-execution
        assert_eq!(error_count(&db, f1), 0);
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);

        // change an UNRELATED file → f1's result stays memoized
        f2.set_text(&mut db).to("contract D2 {}".to_string());
        assert_eq!(error_count(&db, f1), 0);
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);

        // change f1's own text (introduce a syntax error) → re-executes
        f1.set_text(&mut db).to("@@@ contract".to_string());
        assert!(error_count(&db, f1) >= 1);
        assert_eq!(RUNS.load(Ordering::SeqCst), 2);
    }
}
