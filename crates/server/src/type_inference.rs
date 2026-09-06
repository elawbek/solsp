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
        PAREN_EXPR => arg
            .first_child()
            .map(|inner| infer_arg_ty(state, uri, root, &inner))
            .unwrap_or(typecheck::Ty::Unknown),
        BIN_EXPR => {
            use typecheck::Ty;
            let mut operands = arg.children();
            let (Some(left), Some(right)) = (operands.next(), operands.next()) else {
                return Ty::Unknown;
            };
            let left = infer_arg_ty(state, uri, root, &left);
            let right = infer_arg_ty(state, uri, root, &right);
            let op = arg
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|token| !token.kind().is_trivia())
                .map(|token| token.kind());
            match (op, &left, &right) {
                (
                    Some(SHL | SHR | STAR2),
                    Ty::Uint(_) | Ty::Int(_),
                    Ty::Uint(_) | Ty::NumberLiteral | Ty::HexLiteral,
                ) => left,
                (Some(PLUS | MINUS | STAR | SLASH | PERCENT | AMP | PIPE | CARET), _, _) => {
                    match (&left, &right) {
                        (Ty::Uint(a), Ty::Uint(b)) => Ty::Uint((*a).max(*b)),
                        (Ty::Int(a), Ty::Int(b)) => Ty::Int((*a).max(*b)),
                        (Ty::Uint(_) | Ty::Int(_), Ty::NumberLiteral | Ty::HexLiteral) => left,
                        (Ty::NumberLiteral | Ty::HexLiteral, Ty::Uint(_) | Ty::Int(_)) => right,
                        _ => Ty::Unknown,
                    }
                }
                _ => Ty::Unknown,
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overloaded_call_return_type_uses_named_and_positional_argument_types() {
        let boolean =
            "function read(string calldata name, bool defaultValue) external view returns (bool);";
        let numeric = "function read(string calldata name, uint256 defaultValue) external view returns (uint256);";
        for overloads in [
            format!("{boolean} {numeric}"),
            format!("{numeric} {boolean}"),
        ] {
            let source = format!(
                r#"interface Settings {{ {overloads}
                function clock() external view returns (uint256);
            }} contract C {{ Settings settings;
                function run(uint timestamp) public view {{
                    uint a = settings.read({{name: "a", defaultValue: settings.clock() / 1000}});
                    uint b = settings.read({{defaultValue: (timestamp / 15), name: "b"}});
                    bool c = settings.read({{name: "c", defaultValue: true}});
                    uint d = settings.read("d", timestamp);
                    bool e = settings.read("e", false);
                    uint unknown = settings.read("unknown", unresolved);
                }}
            }}"#
            );
            let mut state = ServerState::default();
            let uri = Url::parse("file:///overload-inference.sol").unwrap();
            state.set(&uri, source);
            let root = parse_root(&state, &uri).unwrap();
            let calls: Vec<_> = root
                .descendants()
                .filter(|node| {
                    node.kind() == solsp_syntax::SyntaxKind::CALL_EXPR
                        && node.first_child().is_some_and(|callee| {
                            callee.text().to_string().trim() == "settings.read"
                        })
                })
                .collect();
            let types: Vec<_> = calls
                .iter()
                .map(|call| infer_arg_ty(&state, &uri, &root, call))
                .collect();
            assert_eq!(
                types,
                vec![
                    typecheck::Ty::Uint(256),
                    typecheck::Ty::Uint(256),
                    typecheck::Ty::Bool,
                    typecheck::Ty::Uint(256),
                    typecheck::Ty::Bool,
                    typecheck::Ty::Unknown
                ]
            );
        }
    }
}
