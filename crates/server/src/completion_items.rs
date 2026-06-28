//! Mapping HIR definitions into LSP completion items.

use lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};

use super::trigger_signature_help;

/// Build completion items from definitions, keeping the first of each name (inner scopes
/// come first, so a local shadows an inherited member of the same name).
pub(super) fn completion_items_from(
    defs: Vec<solsp_hir::resolve::Definition>,
) -> Vec<CompletionItem> {
    let mut seen = std::collections::HashSet::new();
    defs.into_iter()
        .filter(|d| seen.insert(d.name.clone()))
        .map(|d| {
            let (insert_text, insert_text_format) = callable_snippet(&d.name, d.kind);
            // A callable inserts `name()`; ask the client to pop signature help inside.
            let command = insert_text_format.map(|_| trigger_signature_help());
            CompletionItem {
                kind: Some(completion_kind(d.kind)),
                // A value member shows its declared type; everything else its kind label.
                detail: Some(
                    d.ty.clone()
                        .unwrap_or_else(|| def_detail(d.kind).to_string()),
                ),
                insert_text,
                insert_text_format,
                command,
                label: d.name,
                ..Default::default()
            }
        })
        .collect()
}

/// For a callable (function/modifier/event/error), a snippet inserting `name()` with the
/// cursor between the parentheses; `(None, None)` otherwise.
fn callable_snippet(
    name: &str,
    kind: solsp_hir::resolve::DefKind,
) -> (Option<String>, Option<InsertTextFormat>) {
    use solsp_hir::resolve::DefKind::*;
    if matches!(kind, Function | Modifier | Event | Error) {
        (Some(format!("{name}($0)")), Some(InsertTextFormat::SNIPPET))
    } else {
        (None, None)
    }
}

fn completion_kind(k: solsp_hir::resolve::DefKind) -> CompletionItemKind {
    use solsp_hir::resolve::DefKind::*;
    match k {
        Function => CompletionItemKind::FUNCTION,
        Modifier => CompletionItemKind::FUNCTION,
        StateVariable | Local | Parameter => CompletionItemKind::VARIABLE,
        Field => CompletionItemKind::FIELD,
        Variant => CompletionItemKind::ENUM_MEMBER,
        Contract => CompletionItemKind::CLASS,
        Interface => CompletionItemKind::INTERFACE,
        Library => CompletionItemKind::MODULE,
        Struct => CompletionItemKind::STRUCT,
        Enum => CompletionItemKind::ENUM,
        Event => CompletionItemKind::EVENT,
        Error => CompletionItemKind::CONSTRUCTOR,
        UserType => CompletionItemKind::TYPE_PARAMETER,
    }
}

fn def_detail(k: solsp_hir::resolve::DefKind) -> &'static str {
    use solsp_hir::resolve::DefKind::*;
    match k {
        Function => "function",
        Modifier => "modifier",
        StateVariable => "state variable",
        Local => "local",
        Parameter => "parameter",
        Field => "field",
        Variant => "enum variant",
        Contract => "contract",
        Interface => "interface",
        Library => "library",
        Struct => "struct",
        Enum => "enum",
        Event => "event",
        Error => "error",
        UserType => "type",
    }
}
