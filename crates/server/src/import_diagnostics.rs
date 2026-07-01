//! Import validity and unused-import diagnostics.

use super::*;

/// Flag named import entries whose target file does not export the requested symbol.
pub(super) fn invalid_import_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_hir::imports::ImportKind;
    use solsp_syntax::SyntaxKind::IMPORT_DIRECTIVE;

    let directives: Vec<_> = root
        .descendants()
        .filter(|node| node.kind() == IMPORT_DIRECTIVE)
        .collect();
    let mut out = Vec::new();
    for (dir, imp) in directives.iter().zip(solsp_hir::imports::imports(root)) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let ImportKind::Named(names) = &imp.kind else {
            continue;
        };
        for name in names {
            if import_export_exists(state, uri, &imp.path, &name.name) {
                continue;
            }
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, import_name_range(dir, &name.name)),
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                source: Some("solsp".to_string()),
                message: format!(
                    "Import target `{}` does not export `{}`",
                    imp.path, name.name
                ),
                ..Default::default()
            });
        }
    }
    out
}

fn import_export_exists(state: &ServerState, uri: &Url, path: &str, name: &str) -> bool {
    let Some(turi) = state::resolve_import_uri(uri, path) else {
        return true;
    };
    let Some(troot) = parse_root(state, &turi) else {
        return true;
    };
    solsp_hir::resolve::top_level_definition(&troot, name, None).is_some()
        || cross_file_definition(state, &turi, &troot, name, None).is_some()
}

fn import_name_range(import_directive: &solsp_syntax::SyntaxNode, name: &str) -> rowan::TextRange {
    use solsp_syntax::SyntaxKind::{AS_KW, IDENT, L_BRACE, R_BRACE};
    let tokens: Vec<_> = import_directive
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| !token.kind().is_trivia())
        .collect();
    let start = tokens
        .iter()
        .position(|token| token.kind() == L_BRACE)
        .map(|index| index + 1)
        .unwrap_or(0);
    let end = tokens
        .iter()
        .position(|token| token.kind() == R_BRACE)
        .unwrap_or(tokens.len());
    let mut index = start;
    while index < end {
        if tokens[index].kind() == IDENT && tokens[index].text() == name {
            return tokens[index].text_range();
        }
        if tokens.get(index + 1).map(|token| token.kind()) == Some(AS_KW) {
            index += 3;
        } else {
            index += 1;
        }
    }
    import_directive.text_range()
}

/// Flag imported names that are never referenced in the file (`import { A } from "x"` where
/// `A` appears nowhere else).
pub(super) fn unused_import_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_hir::imports::ImportKind;
    use solsp_syntax::SyntaxKind::{IDENT, IMPORT_DIRECTIVE};
    let directives: Vec<_> = root
        .descendants()
        .filter(|n| n.kind() == IMPORT_DIRECTIVE)
        .collect();
    if directives.is_empty() {
        return Vec::new();
    }
    let used = identifiers_outside_imports(root);

    let mut out = Vec::new();
    for (dir, imp) in directives.iter().zip(solsp_hir::imports::imports(root)) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        if has_unused_import_suppression(root, dir) {
            continue;
        }
        let locals: Vec<String> = match imp.kind {
            ImportKind::Named(names) => names
                .iter()
                .filter(|n| import_export_exists(state, uri, &imp.path, &n.name))
                .map(|n| n.local().to_string())
                .collect(),
            ImportKind::Namespace(n) => vec![n],
            ImportKind::Glob => continue,
        };
        for local in locals.iter().filter(|l| !used.contains(*l)) {
            let span = dir
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == IDENT && t.text() == local)
                .last()
                .map(|t| t.text_range())
                .unwrap_or_else(|| dir.text_range());
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, span),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                source: Some("solsp".to_string()),
                message: format!("`{local}` is imported but never used"),
                tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            });
        }
    }
    out
}

fn has_unused_import_suppression(
    root: &solsp_syntax::SyntaxNode,
    import_directive: &solsp_syntax::SyntaxNode,
) -> bool {
    const MARKER: &str = "forge-lint: disable-next-line(unused-import)";
    if import_directive.text().to_string().contains(MARKER) {
        return true;
    }
    let text = root.text().to_string();
    let start: usize = import_directive.text_range().start().into();
    let before = &text[..start];
    let previous_line = before
        .trim_end_matches([' ', '\t', '\r', '\n'])
        .rsplit_once('\n')
        .map_or(before.trim_end(), |(_, line)| line.trim());
    previous_line.contains(MARKER)
}

fn identifiers_outside_imports(
    root: &solsp_syntax::SyntaxNode,
) -> std::collections::HashSet<String> {
    use solsp_syntax::SyntaxKind::{IDENT, IMPORT_DIRECTIVE};
    let directives: Vec<_> = root
        .descendants()
        .filter(|n| n.kind() == IMPORT_DIRECTIVE)
        .collect();
    root.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == IDENT)
        .filter(|t| {
            !directives
                .iter()
                .any(|d| d.text_range().contains_range(t.text_range()))
        })
        .map(|t| t.text().to_string())
        .collect()
}
