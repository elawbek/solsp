//! Control-flow diagnostics.

use super::*;

/// Flag statements that follow a `return` / `revert` / `break` / `continue` in the same
/// block.
pub(super) fn unreachable_diagnostics(
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{BLOCK, BREAK_STMT, CONTINUE_STMT, RETURN_STMT, REVERT_STMT};
    let mut out = Vec::new();
    for block in root.descendants().filter(|n| n.kind() == BLOCK) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let stmts: Vec<_> = block.children().collect();
        let Some(term) = stmts.iter().position(|s| {
            matches!(
                s.kind(),
                RETURN_STMT | REVERT_STMT | BREAK_STMT | CONTINUE_STMT
            )
        }) else {
            continue;
        };
        if let Some(dead) = stmts.get(term + 1) {
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, dead.text_range()),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                source: Some("solsp".to_string()),
                message: "unreachable code".to_string(),
                tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            });
        }
    }
    out
}
