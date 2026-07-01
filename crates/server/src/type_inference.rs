//! Shared type inference and type-diagnostic formatting helpers.

use super::*;

/// A readable Solidity name for a type in a diagnostic message.
pub(super) fn ty_label(ty: &typecheck::Ty) -> String {
    use typecheck::Ty::*;
    match ty {
        Uint(n) => format!("uint{n}"),
        Int(n) => format!("int{n}"),
        Address => "address".into(),
        AddressPayable => "address payable".into(),
        Bool => "bool".into(),
        StringT => "string".into(),
        Bytes => "bytes".into(),
        BytesN(n) => format!("bytes{n}"),
        Array(inner) | FixedArray(inner) => format!("{}[]", ty_label(inner)),
        Mapping => "mapping".into(),
        User(n) => n.clone(),
        NumberLiteral | HexLiteral | StringLiteral | BoolLiteral => "literal".into(),
        Unknown => "?".into(),
    }
}

pub(super) fn arg_text(arg: &solsp_syntax::SyntaxNode) -> String {
    arg.text()
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn type_mismatch(
    li: &solsp_ide::LineIndex,
    node: &solsp_syntax::SyntaxNode,
    message: &str,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: to_proto::range(li, node.text_range()),
        severity: Some(lsp_types::DiagnosticSeverity::ERROR),
        source: Some("solsp".to_string()),
        message: message.to_string(),
        ..Default::default()
    }
}

/// The inferred [`typecheck::Ty`] of a call argument: a literal, a cast, or a value whose
/// declared/return type is read (`receiver_value_info`). `Unknown` when not inferrable.
pub(super) fn infer_arg_ty(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    arg: &solsp_syntax::SyntaxNode,
) -> typecheck::Ty {
    use solsp_syntax::SyntaxKind::*;
    match arg.kind() {
        LITERAL_EXPR => {
            let tok = arg
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|t| !matches!(t.kind(), WHITESPACE | COMMENT));
            match tok.as_ref().map(|t| t.kind()) {
                Some(NUMBER)
                    if tok.as_ref().is_some_and(|t| {
                        t.text().starts_with("0x") || t.text().starts_with("0X")
                    }) =>
                {
                    typecheck::Ty::HexLiteral
                }
                Some(NUMBER) => typecheck::Ty::NumberLiteral,
                Some(STRING) => typecheck::Ty::StringLiteral,
                Some(TRUE_KW | FALSE_KW) => typecheck::Ty::BoolLiteral,
                _ => typecheck::Ty::Unknown,
            }
        }
        CALL_EXPR => {
            let Some(callee) = arg.first_child() else {
                return typecheck::Ty::Unknown;
            };
            if callee.kind() == NEW_EXPR {
                return callee
                    .children()
                    .next()
                    .map(|t| typecheck::parse_ty(&node_type_text(&t)))
                    .unwrap_or(typecheck::Ty::Unknown);
            }
            let Some(cname) = callee_display_name(&callee) else {
                return typecheck::Ty::Unknown;
            };
            let parsed = typecheck::parse_ty(&cname);
            if !matches!(parsed, typecheck::Ty::User(_)) {
                return parsed;
            }
            match resolve_named_callee(state, uri, root, &callee) {
                Some((_, def)) if is_type_kind(def.kind) => typecheck::Ty::User(cname),
                _ => receiver_value_info(state, uri, root, arg)
                    .map(|(t, _)| typecheck::parse_ty(&t))
                    .unwrap_or(typecheck::Ty::Unknown),
            }
        }
        PATH_EXPR | NAME_REF | MEMBER_EXPR | INDEX_EXPR => {
            receiver_value_info(state, uri, root, arg)
                .map(|(t, _)| typecheck::parse_ty(&t))
                .unwrap_or(typecheck::Ty::Unknown)
        }
        _ => typecheck::Ty::Unknown,
    }
}
