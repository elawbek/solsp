//! Completion-visible symbols provided by imports, import paths, and re-exports.

use lsp_types::{CompletionItem, CompletionItemKind, Url};
use std::path::PathBuf;

use crate::state::{self, ServerState};

/// Files/directories available at the current import string cursor.
pub(super) fn import_path_items(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<Vec<CompletionItem>> {
    let typed = import_path_prefix(root, offset)?;
    let (dir_part, segment) = typed
        .rsplit_once('/')
        .map_or(("", typed.as_str()), |(dir, seg)| {
            (&typed[..dir.len() + 1], seg)
        });
    let base_dir = import_dir(uri, dir_part)?;
    let entries = state.import_dir_entries(&base_dir)?;
    let mut items = Vec::new();
    for entry in entries.iter() {
        if !entry.name.starts_with(segment) || should_hide_import_entry(&entry.name, entry.is_dir) {
            continue;
        }
        if entry.is_dir {
            items.push(CompletionItem {
                label: entry.name.clone(),
                kind: Some(CompletionItemKind::FOLDER),
                detail: Some("directory".to_string()),
                ..Default::default()
            });
        } else if entry.is_sol {
            items.push(CompletionItem {
                label: entry.name.clone(),
                kind: Some(CompletionItemKind::FILE),
                detail: Some("Solidity file".to_string()),
                ..Default::default()
            });
        }
    }
    items.sort_by(|a, b| {
        let ak = a.kind != Some(CompletionItemKind::FOLDER);
        let bk = b.kind != Some(CompletionItemKind::FOLDER);
        ak.cmp(&bk).then_with(|| a.label.cmp(&b.label))
    });
    Some(items)
}

fn import_path_prefix(root: &solsp_syntax::SyntaxNode, offset: rowan::TextSize) -> Option<String> {
    use solsp_syntax::SyntaxKind::{IMPORT_DIRECTIVE, STRING};
    let token = root
        .token_at_offset(offset)
        .left_biased()
        .or_else(|| root.token_at_offset(offset).right_biased())?;
    if token.kind() != STRING {
        return None;
    }
    token
        .parent()?
        .ancestors()
        .any(|n| n.kind() == IMPORT_DIRECTIVE)
        .then_some(())?;

    let text = token.text();
    let range = token.text_range();
    let offset: usize = offset.into();
    let start: usize = range.start().into();
    let end: usize = range.end().into();
    if text.len() < 2 || offset <= start || offset > end {
        return None;
    }
    let inner_cursor = offset.saturating_sub(start + 1).min(text.len() - 2);
    text.get(1..1 + inner_cursor).map(str::to_string)
}

fn import_dir(uri: &Url, dir_part: &str) -> Option<PathBuf> {
    let file = uri.to_file_path().ok()?;
    let current_dir = file.parent()?;
    let path = if dir_part.starts_with("./") || dir_part.starts_with("../") {
        current_dir.join(dir_part)
    } else {
        state::project_root(current_dir)?.join(dir_part.trim_start_matches('/'))
    };
    path.is_dir().then_some(path)
}

fn should_hide_import_entry(name: &str, is_dir: bool) -> bool {
    name.starts_with('.') || (is_dir && matches!(name, "out" | "cache" | "target" | "node_modules"))
}

/// A completion item for each `import * as N` namespace alias.
pub(super) fn namespace_alias_items(root: &solsp_syntax::SyntaxNode) -> Vec<CompletionItem> {
    use solsp_hir::imports::ImportKind;
    solsp_hir::imports::imports(root)
        .into_iter()
        .filter_map(|imp| match imp.kind {
            ImportKind::Namespace(alias) => Some(CompletionItem {
                kind: Some(CompletionItemKind::MODULE),
                detail: Some("import namespace".to_string()),
                label: alias,
                ..Default::default()
            }),
            _ => None,
        })
        .collect()
}

/// Every symbol the file's imports bring into scope (so `new Roles(` offers `Roles`):
/// named imports under their local name, and glob imports' transitively re-exported
/// top-level declarations.
pub(super) fn imported_symbols(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
) -> Vec<solsp_hir::resolve::Definition> {
    use solsp_hir::imports::ImportKind;
    let mut out = Vec::new();
    let mut visited = std::collections::HashSet::new();
    for imp in solsp_hir::imports::imports(root) {
        let Some(turi) = state::resolve_import_uri(uri, &imp.path) else {
            continue;
        };
        let Some(tfile) = state.file(&turi) else {
            continue;
        };
        let troot = solsp_base_db::parse(state.db(), tfile).syntax();
        match &imp.kind {
            ImportKind::Named(list) => {
                for n in list {
                    if let Some((_, mut def)) =
                        solsp_hir::resolve::top_level_definition(&troot, &n.name, None)
                            .map(|d| (turi.clone(), d))
                            .or_else(|| {
                                super::cross_file_definition(state, &turi, &troot, &n.name, None)
                            })
                    {
                        def.name = n.local().to_string(); // the label is the local alias
                        out.push(def);
                    }
                }
            }
            ImportKind::Glob => collect_file_exports(state, &turi, &troot, &mut visited, &mut out),
            // `* as N` — `N.member` is member completion, not a bare name.
            ImportKind::Namespace(_) => {}
        }
    }
    out
}

/// Collect a file's top-level declarations plus everything it re-exports transitively
/// (a glob import re-exports its own imports). Cycle-safe via `visited`.
pub(super) fn collect_file_exports(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    visited: &mut std::collections::HashSet<Url>,
    out: &mut Vec<solsp_hir::resolve::Definition>,
) {
    use solsp_hir::imports::ImportKind;
    if !visited.insert(uri.clone()) {
        return;
    }
    out.extend(solsp_hir::resolve::file_definitions(root));
    for imp in solsp_hir::imports::imports(root) {
        let Some(turi) = state::resolve_import_uri(uri, &imp.path) else {
            continue;
        };
        let Some(tfile) = state.file(&turi) else {
            continue;
        };
        let troot = solsp_base_db::parse(state.db(), tfile).syntax();
        match &imp.kind {
            ImportKind::Glob => collect_file_exports(state, &turi, &troot, visited, out),
            ImportKind::Named(list) => {
                for n in list {
                    if let Some((_, mut def)) =
                        solsp_hir::resolve::top_level_definition(&troot, &n.name, None)
                            .map(|d| (turi.clone(), d))
                            .or_else(|| {
                                super::cross_file_definition(state, &turi, &troot, &n.name, None)
                            })
                    {
                        def.name = n.local().to_string();
                        out.push(def);
                    }
                }
            }
            ImportKind::Namespace(_) => {}
        }
    }
}
