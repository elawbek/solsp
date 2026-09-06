//! Cross-file import and re-export resolution helpers.

use super::*;

/// A resolved name denotes a declaration or an overload group. An explicit import
/// alias is a separate binding, retained when a call selects one of the overloads.
pub(super) struct ResolvedSymbol {
    pub uri: Url,
    pub definitions: Vec<solsp_hir::resolve::Definition>,
    pub alias: Option<RefTarget>,
}

impl ResolvedSymbol {
    pub fn single(uri: Url, definition: solsp_hir::resolve::Definition) -> Self {
        Self {
            uri,
            definitions: vec![definition],
            alias: None,
        }
    }

    pub fn select_definition(
        &self,
        state: &ServerState,
        arity: Option<usize>,
    ) -> Option<solsp_hir::resolve::Definition> {
        let root = parse_root(state, &self.uri)?;
        solsp_hir::resolve::select_named(
            &self.definitions,
            &self.definitions.first()?.name,
            arity,
            &root,
        )
    }

    pub fn declaration_target(&self, state: &ServerState) -> Option<RefTarget> {
        definition_target(state, self.uri.clone(), self.definitions.first()?)
    }

    pub fn rename_target(&self, state: &ServerState) -> Option<RefTarget> {
        self.alias
            .clone()
            .or_else(|| self.declaration_target(state))
    }

    pub fn references(&self, state: &ServerState, target: &RefTarget) -> bool {
        if self.uri != target.uri {
            return false;
        }
        let Some(root) = parse_root(state, &self.uri) else {
            return false;
        };
        self.definitions
            .iter()
            .any(|definition| def_name_range(&root, definition) == target.range)
    }
}

/// Look up a name supplied by a file's imports. Lexical declarations are handled
/// by the caller before reaching this scope, as in cross_file_definition.
pub(super) fn imported_symbol(
    state: &ServerState,
    uri: &Url,
    name: &str,
) -> Option<ResolvedSymbol> {
    resolve_symbol(
        state,
        uri,
        name,
        false,
        &mut std::collections::HashSet::new(),
    )
}

/// Look up an export, including declarations owned by the target file itself.
pub(super) fn exported_symbol(
    state: &ServerState,
    uri: &Url,
    name: &str,
) -> Option<ResolvedSymbol> {
    resolve_symbol(
        state,
        uri,
        name,
        true,
        &mut std::collections::HashSet::new(),
    )
}

/// The single import-graph traversal used by navigation, completion, rename and
/// references. Resolve the full overload group before selecting by call context.
fn resolve_symbol(
    state: &ServerState,
    uri: &Url,
    name: &str,
    include_declarations: bool,
    visited: &mut std::collections::HashSet<(Url, String)>,
) -> Option<ResolvedSymbol> {
    use solsp_hir::imports::ImportKind;
    if !visited.insert((uri.clone(), name.to_string())) {
        return None;
    }
    let index = state.file_index(uri)?;
    if include_declarations {
        let definitions: Vec<_> = index
            .defs
            .iter()
            .filter(|def| def.name == name)
            .cloned()
            .collect();
        if !definitions.is_empty() {
            return Some(ResolvedSymbol {
                uri: uri.clone(),
                definitions,
                alias: None,
            });
        }
    }
    for import in &index.imports {
        let Some(target) = &import.target else {
            continue;
        };
        let (export, alias_range) = match &import.kind {
            ImportKind::Glob => (name, None),
            ImportKind::Named(names) => {
                let Some(binding) = names.iter().find(|binding| binding.local() == name) else {
                    continue;
                };
                (binding.name.as_str(), binding.alias_range)
            }
            ImportKind::Namespace(_) => continue,
        };
        if let Some(mut symbol) = resolve_symbol(state, target, export, true, visited) {
            if let Some(range) = alias_range {
                symbol.alias = Some(RefTarget {
                    uri: uri.clone(),
                    range,
                });
            }
            return Some(symbol);
        }
    }
    None
}

/// Each side of a named import resolves against that directive's target file.
/// The source side retains any re-exported binding; the alias side creates its own.
pub(super) fn import_symbol_at(
    state: &ServerState,
    uri: &Url,
    range: rowan::TextRange,
) -> Option<ResolvedSymbol> {
    use solsp_hir::imports::ImportKind;
    for import in &state.file_index(uri)?.imports {
        let ImportKind::Named(names) = &import.kind else {
            continue;
        };
        for name in names {
            if name.name_range != range && name.alias_range != Some(range) {
                continue;
            }
            let mut symbol = exported_symbol(state, import.target.as_ref()?, &name.name)?;
            if name.alias_range == Some(range) {
                symbol.alias = Some(RefTarget {
                    uri: uri.clone(),
                    range,
                });
            }
            return Some(symbol);
        }
    }
    None
}

/// Find an imported top-level symbol referenced in root, following re-exports.
pub(super) fn cross_file_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<(Url, rowan::TextRange)> {
    let (turi, def) = cross_file_definition(state, uri, root, name, arity)?;
    let troot = parse_root(state, &turi)?;
    Some((turi, def_name_range(&troot, &def)))
}

/// Compatibility entry point for features that need only a selected declaration.
/// Import and alias resolution is shared with rename and references.
pub(super) fn cross_file_definition(
    state: &ServerState,
    uri: &Url,
    _root: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    let symbol = imported_symbol(state, uri, name)?;
    let definition = symbol.select_definition(state, arity)?;
    Some((symbol.uri, definition))
}
