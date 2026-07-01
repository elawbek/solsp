//! Type-oriented diagnostics.

use super::*;

pub(super) fn assignment_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{ASSIGN_EXPR, EQ, VAR_DECL, VAR_DECL_STMT};
    let mut out = Vec::new();
    for node in root.descendants() {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let (target, value) = match node.kind() {
            ASSIGN_EXPR => {
                let op_is_eq = node
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .any(|t| t.kind() == EQ);
                let exprs: Vec<_> = node.children().collect();
                if !op_is_eq || exprs.len() != 2 {
                    continue;
                }
                let lty = infer_arg_ty(state, uri, root, &exprs[0]);
                (lty, exprs[1].clone())
            }
            VAR_DECL_STMT => {
                let is_tuple = node
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .any(|t| t.kind() == solsp_syntax::SyntaxKind::L_PAREN);
                let decls: Vec<_> = node.children().filter(|c| c.kind() == VAR_DECL).collect();
                let Some(init) = node.children().find(|c| c.kind() != VAR_DECL) else {
                    continue;
                };
                if is_tuple || decls.len() != 1 {
                    continue;
                }
                let Some(ty) = type_text(&decls[0]) else {
                    continue;
                };
                (typecheck::parse_ty(&ty), init)
            }
            _ => continue,
        };
        if let Some(msg) = literal_range_error(&value, &target) {
            out.push(type_mismatch(li, &value, &msg));
            continue;
        }
        let value_ty = infer_arg_ty(state, uri, root, &value);
        if matches!(target, typecheck::Ty::Unknown) || matches!(value_ty, typecheck::Ty::Unknown) {
            continue;
        }
        if !types_compatible(state, uri, root, &value_ty, &target) {
            out.push(type_mismatch(
                li,
                &value,
                &format!(
                    "value of type `{}` is not implicitly convertible to `{}`",
                    arg_text(&value),
                    ty_label(&target),
                ),
            ));
        }
    }
    out
}

fn literal_range_error(value: &solsp_syntax::SyntaxNode, target: &typecheck::Ty) -> Option<String> {
    let (signed, bits) = match target {
        typecheck::Ty::Uint(b) => (false, *b),
        typecheck::Ty::Int(b) => (true, *b),
        _ => return None,
    };
    if bits >= 128 {
        return None;
    }
    let v = literal_u128(value)?;
    let max = if signed {
        (1u128 << (bits - 1)) - 1
    } else {
        (1u128 << bits) - 1
    };
    (v > max).then(|| format!("literal `{v}` does not fit in `{}`", ty_label(target)))
}

fn literal_u128(value: &solsp_syntax::SyntaxNode) -> Option<u128> {
    use solsp_syntax::SyntaxKind::{LITERAL_EXPR, NUMBER};
    if value.kind() != LITERAL_EXPR {
        return None;
    }
    let tok = value
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == NUMBER)?;
    let text = tok.text().replace('_', "");
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16).ok()
    } else if text.contains(['.', 'e', 'E']) {
        None
    } else {
        text.parse::<u128>().ok()
    }
}

pub(super) fn return_type_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{
        CALL_EXPR, COMMA, FUNCTION_DEF, PARAM, PARAM_LIST, RETURN_STMT, TUPLE_EXPR,
    };
    let mut out = Vec::new();
    for ret in root.descendants().filter(|n| n.kind() == RETURN_STMT) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let Some(value) = ret.children().next() else {
            continue;
        };
        let Some(func) = ret.ancestors().find(|n| n.kind() == FUNCTION_DEF) else {
            continue;
        };
        let Some(returns) = func.children().filter(|n| n.kind() == PARAM_LIST).nth(1) else {
            continue;
        };
        let ret_params: Vec<_> = returns.children().filter(|n| n.kind() == PARAM).collect();
        if value.kind() == TUPLE_EXPR {
            let elems = value
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == COMMA)
                .count()
                + 1;
            if elems != ret_params.len() {
                out.push(type_mismatch(
                    li,
                    &value,
                    &format!(
                        "returns {} value(s), but the function declares {}",
                        elems,
                        ret_params.len(),
                    ),
                ));
            }
            continue;
        }
        if ret_params.len() != 1 {
            if value.kind() != CALL_EXPR {
                out.push(type_mismatch(
                    li,
                    &value,
                    &format!(
                        "returns 1 value, but the function declares {}",
                        ret_params.len(),
                    ),
                ));
            }
            continue;
        }
        let Some(ty) = type_text(&ret_params[0]) else {
            continue;
        };
        let target = typecheck::parse_ty(&ty);
        if let Some(msg) = literal_range_error(&value, &target) {
            out.push(type_mismatch(li, &value, &msg));
            continue;
        }
        let value_ty = infer_arg_ty(state, uri, root, &value);
        if matches!(target, typecheck::Ty::Unknown) || matches!(value_ty, typecheck::Ty::Unknown) {
            continue;
        }
        if !types_compatible(state, uri, root, &value_ty, &target) {
            out.push(type_mismatch(
                li,
                &value,
                &format!(
                    "returned value of type `{}` is not implicitly convertible to `{}`",
                    arg_text(&value),
                    ty_label(&target),
                ),
            ));
        }
    }
    out
}

pub(super) fn cast_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, CALL_EXPR, NAME_REF, PATH_EXPR};
    let mut out = Vec::new();
    for call in root.descendants().filter(|n| n.kind() == CALL_EXPR) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let Some(callee) = call.first_child() else {
            continue;
        };
        if !matches!(
            callee_display_name(&callee).as_deref(),
            Some("address" | "payable")
        ) {
            continue;
        }
        let Some(arg_list) = call.children().find(|n| n.kind() == ARG_LIST) else {
            continue;
        };
        let args: Vec<_> = arg_list.children().collect();
        let [arg] = args.as_slice() else { continue };
        if !matches!(arg.kind(), PATH_EXPR | NAME_REF) {
            continue;
        }
        let Some(def) = resolve_receiver_def(state, uri, root, arg) else {
            continue;
        };
        if !is_value_kind(def.kind) {
            out.push(type_mismatch(
                li,
                arg,
                &format!(
                    "cannot convert {} `{}` to an address",
                    def_kind_noun(def.kind),
                    arg_text(arg),
                ),
            ));
        }
    }
    out
}

fn is_value_kind(kind: solsp_hir::resolve::DefKind) -> bool {
    use solsp_hir::resolve::DefKind::{Field, Local, Parameter, StateVariable, Variant};
    matches!(kind, StateVariable | Parameter | Local | Field | Variant)
}

fn def_kind_noun(kind: solsp_hir::resolve::DefKind) -> &'static str {
    use solsp_hir::resolve::DefKind::*;
    match kind {
        Function => "function",
        Modifier => "modifier",
        Event => "event",
        Error => "error",
        Contract => "contract",
        Interface => "interface",
        Library => "library",
        Struct => "struct",
        Enum => "enum",
        UserType => "type",
        _ => "value",
    }
}

pub(super) fn binary_op_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{
        AMP, BIN_EXPR, CARET, MINUS, PERCENT, PIPE, PLUS, SHL, SHR, SLASH, STAR, STAR2,
    };
    let mut out = Vec::new();
    for bin in root.descendants().filter(|n| n.kind() == BIN_EXPR) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let is_arith = bin
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| {
                matches!(
                    t.kind(),
                    PLUS | MINUS | STAR | SLASH | PERCENT | STAR2 | AMP | PIPE | CARET | SHL | SHR
                )
            });
        if !is_arith {
            continue;
        }
        for operand in bin.children() {
            let ty = infer_arg_ty(state, uri, root, &operand);
            if is_non_arithmetic_type(&ty) {
                out.push(type_mismatch(
                    li,
                    &operand,
                    &format!(
                        "`{}` of type `{}` cannot be used in an arithmetic or bitwise expression",
                        arg_text(&operand),
                        ty_label(&ty),
                    ),
                ));
            }
        }
    }
    out
}

pub(super) fn comparison_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{BIN_EXPR, EQ2, GT, GT_EQ, LT, LT_EQ, NEQ};
    let mut out = Vec::new();
    for bin in root.descendants().filter(|n| n.kind() == BIN_EXPR) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let ordered = bin
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find_map(|t| match t.kind() {
                LT | GT | LT_EQ | GT_EQ => Some(true),
                EQ2 | NEQ => Some(false),
                _ => None,
            });
        let Some(ordered) = ordered else {
            continue;
        };
        let operands: Vec<_> = bin.children().collect();
        let [lhs, rhs] = operands.as_slice() else {
            continue;
        };
        let lt = infer_arg_ty(state, uri, root, lhs);
        let rt = infer_arg_ty(state, uri, root, rhs);
        if matches!(lt, typecheck::Ty::Unknown) || matches!(rt, typecheck::Ty::Unknown) {
            continue;
        }
        if !types_compatible(state, uri, root, &lt, &rt)
            && !types_compatible(state, uri, root, &rt, &lt)
        {
            out.push(type_mismatch(
                li,
                &bin,
                &format!("cannot compare `{}` and `{}`", ty_label(&lt), ty_label(&rt)),
            ));
            continue;
        }
        if ordered && (!is_ordered_comparable(&lt) || !is_ordered_comparable(&rt)) {
            let bad = if is_ordered_comparable(&lt) { &rt } else { &lt };
            out.push(type_mismatch(
                li,
                &bin,
                &format!("`{}` does not support ordered comparison", ty_label(bad)),
            ));
        }
    }
    out
}

pub(super) fn condition_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{DO_WHILE_STMT, FOR_STMT, IF_STMT, WHILE_STMT};
    let mut out = Vec::new();
    for stmt in root
        .descendants()
        .filter(|n| matches!(n.kind(), IF_STMT | WHILE_STMT | DO_WHILE_STMT | FOR_STMT))
    {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let Some(condition) = condition_expr(&stmt) else {
            continue;
        };
        let ty = infer_arg_ty(state, uri, root, &condition);
        if matches!(
            ty,
            typecheck::Ty::Unknown | typecheck::Ty::Bool | typecheck::Ty::BoolLiteral
        ) {
            continue;
        }
        out.push(type_mismatch(
            li,
            &condition,
            &format!("condition must be `bool`, got `{}`", ty_label(&ty)),
        ));
    }
    out
}

fn condition_expr(stmt: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{DO_WHILE_STMT, FOR_STMT, IF_STMT, SEMICOLON, WHILE_STMT};
    match stmt.kind() {
        IF_STMT | WHILE_STMT => stmt.children().next(),
        DO_WHILE_STMT => stmt.children().last(),
        FOR_STMT => {
            let semis: Vec<_> = stmt
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == SEMICOLON)
                .collect();
            let [first, second, ..] = semis.as_slice() else {
                return None;
            };
            stmt.children().find(|n| {
                first.text_range().end() <= n.text_range().start()
                    && n.text_range().end() <= second.text_range().start()
            })
        }
        _ => None,
    }
}

fn is_ordered_comparable(ty: &typecheck::Ty) -> bool {
    use typecheck::Ty::*;
    matches!(
        ty,
        Uint(_) | Int(_) | Address | AddressPayable | BytesN(_) | NumberLiteral | HexLiteral
    )
}

fn is_non_arithmetic_type(ty: &typecheck::Ty) -> bool {
    use typecheck::Ty::*;
    matches!(
        ty,
        Address | AddressPayable | Bool | StringT | User(_) | Array(_) | FixedArray(_) | Mapping
    )
}

fn types_compatible(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    from: &typecheck::Ty,
    to: &typecheck::Ty,
) -> bool {
    typecheck::implicitly_convertible(from, to, &|a, b| is_subtype(state, uri, root, a, b))
}
