//! Function mutability diagnostics and storage/Yul effect detection.

use super::*;

pub(super) fn mutability_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_hir::resolve::DefKind;
    use solsp_syntax::SyntaxKind::{
        BLOCK, FUNCTION_DEF, MODIFIER_INVOCATION, NAME_REF, PAYABLE_KW, PURE_KW, VIEW_KW,
    };
    let mut out = Vec::new();
    for func in root.descendants().filter(|n| n.kind() == FUNCTION_DEF) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let is_view = func
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == VIEW_KW);
        let is_pure = func
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == PURE_KW);
        let yul_effects = yul_mutability_effects(&func);
        let storage_writes = storage_mutability_writes(state, uri, root, &func);

        let mut reads_state = false;
        let mut writes_state = false;
        for nr in func.descendants().filter(|n| n.kind() == NAME_REF) {
            let Some((_, def_root, def)) = resolve_receiver_def_target(state, uri, root, &nr)
            else {
                continue;
            };
            if def.kind != DefKind::StateVariable {
                continue;
            }
            if state_variable_is_constant_or_immutable(&def.full_ptr.to_node(&def_root)) {
                continue;
            }
            if state_write_lhs(&nr).is_some() {
                writes_state = true;
            } else {
                reads_state = true;
                if is_pure {
                    out.push(type_mismatch(
                        li,
                        &nr,
                        "cannot read state in a `pure` function",
                    ));
                }
            }
        }

        if (is_view || is_pure) && writes_state {
            for lhs in func.descendants().filter_map(|n| {
                (n.kind() == NAME_REF
                    && resolve_receiver_def(state, uri, root, &n)
                        .is_some_and(|def| def.kind == DefKind::StateVariable))
                .then(|| state_write_lhs(&n))
                .flatten()
            }) {
                out.push(type_mismatch(
                    li,
                    &lhs,
                    "cannot write to state in a `view`/`pure` function",
                ));
            }
        }
        if is_view || is_pure {
            for write in &storage_writes {
                out.push(type_mismatch(
                    li,
                    write,
                    "cannot write to state in a `view`/`pure` function",
                ));
            }
        }
        if is_pure {
            for read in &yul_effects.state_reads {
                out.push(type_mismatch(
                    li,
                    read,
                    "cannot read state in a `pure` function",
                ));
            }
        }
        if is_view || is_pure {
            for write in &yul_effects.state_writes {
                out.push(type_mismatch(
                    li,
                    write,
                    "cannot write to state in a `view`/`pure` function",
                ));
            }
        }

        if is_view
            || is_pure
            || writes_state
            || !storage_writes.is_empty()
            || !yul_effects.state_writes.is_empty()
            || yul_effects.has_unknown_call
            || has_unknown_mutability_calls(state, uri, root, &func)
            || func.children().all(|n| n.kind() != BLOCK)
            || func.children().any(|n| n.kind() == MODIFIER_INVOCATION)
            || func
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .any(|t| t.kind() == PAYABLE_KW)
        {
            continue;
        }

        if reads_state || !yul_effects.state_reads.is_empty() {
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, function_name_range(&func)),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                source: Some("solsp".to_string()),
                message: "function can be marked `view`".to_string(),
                ..Default::default()
            });
        } else {
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, function_name_range(&func)),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                source: Some("solsp".to_string()),
                message: "function can be marked `pure`".to_string(),
                ..Default::default()
            });
        }
    }
    out
}

struct YulMutabilityEffects {
    state_reads: Vec<solsp_syntax::SyntaxNode>,
    state_writes: Vec<solsp_syntax::SyntaxNode>,
    has_unknown_call: bool,
}

fn yul_mutability_effects(function: &solsp_syntax::SyntaxNode) -> YulMutabilityEffects {
    use solsp_syntax::SyntaxKind::{NAME_REF, YUL_FUNCTION_CALL};

    let mut effects = YulMutabilityEffects {
        state_reads: Vec::new(),
        state_writes: Vec::new(),
        has_unknown_call: false,
    };
    for call in function
        .descendants()
        .filter(|node| node.kind() == YUL_FUNCTION_CALL)
    {
        let Some(callee) = call.descendants().find(|node| node.kind() == NAME_REF) else {
            effects.has_unknown_call = true;
            continue;
        };
        let text = callee.text().to_string();
        let name = text.trim();
        if yul_builtin_writes_state(name) {
            effects.state_writes.push(callee);
        } else if yul_builtin_reads_state(name) {
            effects.state_reads.push(callee);
        } else if !yul_builtin_is_pure(name) {
            effects.has_unknown_call = true;
        }
    }
    effects
}

fn yul_builtin_writes_state(name: &str) -> bool {
    matches!(
        name,
        "sstore"
            | "tstore"
            | "log0"
            | "log1"
            | "log2"
            | "log3"
            | "log4"
            | "create"
            | "create2"
            | "call"
            | "callcode"
            | "delegatecall"
            | "selfdestruct"
    )
}

fn yul_builtin_reads_state(name: &str) -> bool {
    matches!(
        name,
        "sload"
            | "tload"
            | "balance"
            | "selfbalance"
            | "extcodesize"
            | "extcodecopy"
            | "extcodehash"
            | "origin"
            | "caller"
            | "callvalue"
            | "gasprice"
            | "coinbase"
            | "timestamp"
            | "number"
            | "difficulty"
            | "prevrandao"
            | "gaslimit"
            | "chainid"
            | "basefee"
            | "blobbasefee"
            | "blobhash"
            | "gas"
    )
}

fn yul_builtin_is_pure(name: &str) -> bool {
    matches!(
        name,
        "stop"
            | "add"
            | "sub"
            | "mul"
            | "div"
            | "sdiv"
            | "mod"
            | "smod"
            | "exp"
            | "not"
            | "lt"
            | "gt"
            | "slt"
            | "sgt"
            | "eq"
            | "iszero"
            | "and"
            | "or"
            | "xor"
            | "byte"
            | "shl"
            | "shr"
            | "sar"
            | "addmod"
            | "mulmod"
            | "signextend"
            | "keccak256"
            | "pc"
            | "pop"
            | "mload"
            | "mstore"
            | "mstore8"
            | "msize"
            | "mcopy"
            | "calldataload"
            | "calldatasize"
            | "calldatacopy"
            | "codesize"
            | "codecopy"
            | "returndatasize"
            | "returndatacopy"
            | "return"
            | "revert"
    )
}

fn state_variable_is_constant_or_immutable(var: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{CONSTANT_KW, IMMUTABLE_KW};
    var.children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| matches!(token.kind(), CONSTANT_KW | IMMUTABLE_KW))
}

fn has_unknown_mutability_calls(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    function: &solsp_syntax::SyntaxNode,
) -> bool {
    use solsp_hir::resolve::DefKind;
    use solsp_syntax::SyntaxKind::CALL_EXPR;

    for call in function
        .descendants()
        .filter(|node| node.kind() == CALL_EXPR)
    {
        let Some(callee) = call.first_child() else {
            return true;
        };
        let Some((target_uri, def)) = resolve_callee(state, uri, root, &callee, arg_count(&call))
        else {
            return true;
        };
        match def.kind {
            DefKind::Function => {
                let Some(target_root) = parse_root(state, &target_uri) else {
                    return true;
                };
                let target_node = def.full_ptr.to_node(&target_root);
                if !function_has_view_or_pure(&target_node) {
                    return true;
                }
            }
            DefKind::Error => {}
            _ => return true,
        }
    }
    false
}

fn function_has_view_or_pure(function: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{PURE_KW, VIEW_KW};
    function
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| matches!(token.kind(), VIEW_KW | PURE_KW))
}

fn state_write_lhs(name_ref: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{ASSIGN_EXPR, NAME_REF};
    let asn = name_ref.ancestors().find(|n| n.kind() == ASSIGN_EXPR)?;
    let lhs = asn.first_child()?;
    if !lhs.text_range().contains_range(name_ref.text_range()) {
        return None;
    }
    let base = if lhs.kind() == NAME_REF {
        lhs.clone()
    } else {
        lhs.descendants().find(|n| n.kind() == NAME_REF)?
    };
    (base.text_range() == name_ref.text_range()).then_some(lhs)
}

fn storage_mutability_writes(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    function: &solsp_syntax::SyntaxNode,
) -> Vec<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{ASSIGN_EXPR, CALL_EXPR, POSTFIX_EXPR, PREFIX_EXPR};

    let mut out = Vec::new();
    for expr in function.descendants() {
        match expr.kind() {
            ASSIGN_EXPR => {
                if let Some(lhs) = expr.first_child() {
                    if storage_assignment_target(state, uri, root, &lhs)
                        && !direct_state_assignment_lhs(state, uri, root, &lhs)
                    {
                        out.push(lhs);
                    }
                }
            }
            PREFIX_EXPR | POSTFIX_EXPR => {
                if mutating_unary_expr(&expr) && storage_write_target(state, uri, root, &expr) {
                    out.push(expr.clone());
                }
            }
            CALL_EXPR => {
                if storage_mutating_member_call(state, uri, root, &expr) {
                    out.push(expr.clone());
                }
            }
            _ => {}
        }
    }
    out
}

fn mutating_unary_expr(expr: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{DELETE_KW, MINUS2, PLUS2};
    expr.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| matches!(t.kind(), DELETE_KW | PLUS2 | MINUS2))
}

fn direct_state_assignment_lhs(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    lhs: &solsp_syntax::SyntaxNode,
) -> bool {
    use solsp_hir::resolve::DefKind;
    use solsp_syntax::SyntaxKind::NAME_REF;

    lhs.descendants().filter(|n| n.kind() == NAME_REF).any(|n| {
        state_write_lhs(&n).is_some()
            && resolve_receiver_def(state, uri, root, &n)
                .is_some_and(|def| def.kind == DefKind::StateVariable)
    })
}

fn storage_mutating_member_call(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    call: &solsp_syntax::SyntaxNode,
) -> bool {
    use solsp_syntax::SyntaxKind::MEMBER_EXPR;

    let Some(callee) = call.first_child() else {
        return false;
    };
    if callee.kind() != MEMBER_EXPR {
        return false;
    }
    let Some(member) = member_name(&callee) else {
        return false;
    };
    if !matches!(member.as_str(), "push" | "pop") {
        return false;
    }
    let Some(receiver) = callee.first_child() else {
        return false;
    };
    expression_lives_in_storage(state, uri, root, &receiver)
}

fn storage_write_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    expr: &solsp_syntax::SyntaxNode,
) -> bool {
    use solsp_syntax::SyntaxKind::{INDEX_EXPR, MEMBER_EXPR, NAME_REF, PATH_EXPR};

    match expr.kind() {
        NAME_REF | PATH_EXPR | INDEX_EXPR | MEMBER_EXPR => {
            expression_lives_in_storage(state, uri, root, expr)
        }
        _ => expr
            .children()
            .any(|child| storage_write_target(state, uri, root, &child)),
    }
}

fn storage_assignment_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    expr: &solsp_syntax::SyntaxNode,
) -> bool {
    use solsp_syntax::SyntaxKind::{INDEX_EXPR, MEMBER_EXPR, NAME_REF, PATH_EXPR};

    match expr.kind() {
        // `S storage r = ...` or `ret = s().items[id]` only rebinds a storage
        // reference. The storage object changes when writing through it:
        // `r.field = ...`, `r[i] = ...`, `r.arr.push(...)`.
        NAME_REF | PATH_EXPR => false,
        INDEX_EXPR | MEMBER_EXPR => expression_lives_in_storage(state, uri, root, expr),
        _ => expr
            .children()
            .any(|child| storage_assignment_target(state, uri, root, &child)),
    }
}

fn expression_lives_in_storage(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    expr: &solsp_syntax::SyntaxNode,
) -> bool {
    use solsp_syntax::SyntaxKind::{CALL_EXPR, INDEX_EXPR, MEMBER_EXPR, NAME_REF, PATH_EXPR};

    match expr.kind() {
        NAME_REF | PATH_EXPR => {
            receiver_decl(state, uri, root, expr).is_some_and(|decl| is_storage_decl(&decl))
        }
        CALL_EXPR => {
            receiver_value_info(state, uri, root, expr).is_some_and(|(_, storage)| storage)
        }
        INDEX_EXPR | MEMBER_EXPR => expr
            .first_child()
            .is_some_and(|base| expression_lives_in_storage(state, uri, root, &base)),
        _ => false,
    }
}
