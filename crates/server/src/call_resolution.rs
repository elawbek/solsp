//! Call and overload resolution helpers.

use super::*;

/// Hover for a positional call argument: show the parameter expected by the callee.
pub(super) fn positional_arg_hover(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<Hover> {
    use solsp_syntax::SyntaxKind::IDENT;

    if root.token_at_offset(offset).any(|t| t.kind() == IDENT) {
        return None;
    }
    let (_, label, range) = positional_arg_label(state, uri, root, offset)?;
    Some(markup_hover(
        format!("```solidity\n{label}\n```"),
        Some(to_proto::range(state.line_index(uri)?, range)),
    ))
}

/// The parameter expected at a positional call argument offset.
pub(super) fn positional_arg_label(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<(String, String, rowan::TextRange)> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, CALL_EXPR};

    let tok = root
        .token_at_offset(offset)
        .left_biased()
        .or_else(|| root.token_at_offset(offset).right_biased())?;
    let arg_list = tok.parent()?.ancestors().find(|n| n.kind() == ARG_LIST)?;
    let call = arg_list.parent()?;
    if call.kind() != CALL_EXPR {
        return None;
    }
    let callee = call.first_child()?;
    if callee.text_range().contains(offset) {
        return None;
    }

    let args: Vec<_> = arg_list.children().collect();
    let arg_index = args
        .iter()
        .position(|arg| arg.text_range().contains(offset))?;
    let name = callee_display_name(&callee)?;
    let (def_uri, def) = resolve_named_callee(state, uri, root, &callee)?;
    let droot = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&droot);
    let candidates = signature_candidates(&def, &def_node, &name, &droot);
    let candidate = select_positional_candidate(state, uri, root, &candidates, &args)?;
    let params = named_arg_fields(candidate.0, &candidate.1);
    let (pname, ptype) = params.get(arg_index)?;
    Some((
        name,
        parameter_label(ptype, pname),
        args[arg_index].text_range(),
    ))
}

fn select_positional_candidate<'a>(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    candidates: &'a [(solsp_hir::resolve::DefKind, solsp_syntax::SyntaxNode)],
    args: &[solsp_syntax::SyntaxNode],
) -> Option<&'a (solsp_hir::resolve::DefKind, solsp_syntax::SyntaxNode)> {
    let arity_matches: Vec<_> = candidates
        .iter()
        .filter(|(kind, node)| named_arg_fields(*kind, node).len() == args.len())
        .collect();
    if arity_matches.len() == 1 {
        return Some(arity_matches[0]);
    }

    let arg_tys: Vec<typecheck::Ty> = args
        .iter()
        .map(|arg| infer_arg_ty(state, uri, root, arg))
        .collect();
    let is_base = |a: &str, b: &str| is_subtype(state, uri, root, a, b);
    let mut typed_matches = arity_matches.into_iter().filter(|(kind, node)| {
        let params = named_arg_fields(*kind, node);
        args.iter().enumerate().all(|(i, _)| {
            params
                .get(i)
                .map(|(_, ptype)| typecheck::parse_ty(ptype))
                .is_some_and(|pty| typecheck::implicitly_convertible(&arg_tys[i], &pty, &is_base))
        })
    });
    let candidate = typed_matches.next()?;
    if typed_matches.next().is_some() {
        None
    } else {
        Some(candidate)
    }
}

fn parameter_label(ty: &str, name: &str) -> String {
    match (ty.is_empty(), name.is_empty()) {
        (true, _) => name.to_string(),
        (_, true) => ty.to_string(),
        _ => format!("{ty} {name}"),
    }
}

/// The display name of a call's callee: `f` / `S` / `obj.method` / `new T`.
pub(super) fn callee_display_name(callee: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::{MEMBER_EXPR, NAME_REF, NEW_EXPR, PATH_EXPR};
    match callee.kind() {
        PATH_EXPR | NAME_REF => solsp_hir::resolve::receiver_name(callee),
        MEMBER_EXPR => member_name(callee),
        NEW_EXPR => callee
            .descendants()
            .filter(|n| n.kind() == NAME_REF)
            .last()
            .and_then(|nr| node_ident(&nr)),
        _ => None,
    }
}

/// The declarations to show as signatures: every same-file overload of a function (sorted
/// by parameter count), or the single struct / constructor.
pub(super) fn signature_candidates(
    def: &solsp_hir::resolve::Definition,
    def_node: &solsp_syntax::SyntaxNode,
    name: &str,
    droot: &solsp_syntax::SyntaxNode,
) -> Vec<(solsp_hir::resolve::DefKind, solsp_syntax::SyntaxNode)> {
    use solsp_hir::resolve::DefKind::{Function, Modifier};
    if !matches!(def.kind, Function | Modifier) {
        return vec![(def.kind, def_node.clone())];
    }
    let pool = match enclosing_contract(def_node) {
        Some(c) => solsp_hir::resolve::type_members(&c),
        None => solsp_hir::resolve::file_definitions(droot),
    };
    let mut nodes: Vec<solsp_syntax::SyntaxNode> = pool
        .into_iter()
        .filter(|d| d.kind == Function && d.name == name)
        .map(|d| d.full_ptr.to_node(droot))
        .collect();
    if nodes.is_empty() {
        nodes.push(def_node.clone());
    }
    nodes.sort_by_key(|n| named_arg_fields(Function, n).len());
    nodes.into_iter().map(|n| (def.kind, n)).collect()
}

/// Resolve a named-call callee to its declaration: `new T(...)` -> the type `T`, else a
/// function/struct/contract name or `obj.method`.
pub(super) fn resolve_named_callee(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    callee: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_syntax::SyntaxKind::{NAME_REF, NEW_EXPR};
    if callee.kind() == NEW_EXPR {
        let nr = callee.descendants().find(|n| n.kind() == NAME_REF)?;
        let name = solsp_hir::resolve::receiver_name(&nr)?;
        return solsp_hir::resolve::resolve(&nr)
            .map(|d| (uri.clone(), d))
            .or_else(|| cross_file_definition(state, uri, root, &name, None));
    }
    resolve_callee(state, uri, root, callee, None)
}

/// When the cursor is on the callee of an overloaded call, pick the overload by argument
/// types, returning a match only when exactly one overload accepts the arguments.
pub(super) fn typed_overload_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_hir::resolve::DefKind;
    use solsp_syntax::SyntaxKind::{ARG_LIST, CALL_EXPR, NAME_REF};
    let nr = root
        .token_at_offset(offset)
        .find_map(|t| t.parent_ancestors().find(|n| n.kind() == NAME_REF))?;
    let call = nr.ancestors().find(|n| n.kind() == CALL_EXPR)?;
    let callee = call.first_child()?;
    if !callee.text_range().contains(offset) {
        return None;
    }
    let (def_uri, def) = resolve_named_callee(state, uri, root, &callee)?;
    if def.kind != DefKind::Function {
        return None;
    }
    let droot = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&droot);
    let name = callee_display_name(&callee)?;
    let candidates = signature_candidates(&def, &def_node, &name, &droot);
    if candidates.len() < 2 {
        return None;
    }
    use solsp_syntax::SyntaxKind::NAMED_ARG_LIST;
    let args: Vec<(Option<String>, solsp_syntax::SyntaxNode)> =
        if let Some(al) = call.children().find(|n| n.kind() == ARG_LIST) {
            al.children().map(|v| (None, v)).collect()
        } else if let Some(nal) = call.children().find(|n| n.kind() == NAMED_ARG_LIST) {
            named_arg_pairs(&nal)
        } else {
            return None;
        };
    let arg_tys: Vec<typecheck::Ty> = args
        .iter()
        .map(|(_, v)| infer_arg_ty(state, uri, root, v))
        .collect();
    let is_base = |a: &str, b: &str| is_subtype(state, uri, root, a, b);
    let accepts = |node: &solsp_syntax::SyntaxNode| {
        let params = named_arg_fields(DefKind::Function, node);
        if params.len() != args.len() {
            return false;
        }
        (0..args.len()).all(|i| {
            let ptype = match &args[i].0 {
                Some(key) => params.iter().find(|(pn, _)| pn == key).map(|(_, t)| t),
                None => params.get(i).map(|(_, t)| t),
            };
            ptype.is_some_and(|p| {
                typecheck::implicitly_convertible(&arg_tys[i], &typecheck::parse_ty(p), &is_base)
            })
        })
    };
    let mut matches = candidates.iter().filter(|(_, node)| accepts(node));
    let (_, node) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let def = solsp_hir::resolve::definition(node)?;
    Some((def_uri, def))
}

/// A callable's overloads, each as its parameter `(name, type)` list.
type Overloads = Vec<Vec<(String, String)>>;

/// Type-check the positional call arguments in `root`: an argument whose inferred type is
/// not implicitly convertible to the parameter type yields a diagnostic.
pub(super) fn type_check_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, CALL_EXPR, NAMED_ARG_LIST};
    use std::cell::RefCell;
    use std::collections::HashMap;
    let mut out = Vec::new();
    let mut callee_cache: HashMap<String, Option<Overloads>> = HashMap::new();
    let subtype_memo: RefCell<HashMap<(String, String), bool>> = RefCell::new(HashMap::new());

    for call in root.descendants().filter(|n| n.kind() == CALL_EXPR) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let args: Vec<(Option<String>, solsp_syntax::SyntaxNode)> =
            if let Some(al) = call.children().find(|n| n.kind() == ARG_LIST) {
                al.children().map(|v| (None, v)).collect()
            } else if let Some(nal) = call.children().find(|n| n.kind() == NAMED_ARG_LIST) {
                named_arg_pairs(&nal)
            } else {
                continue;
            };
        let Some(callee) = call.first_child() else {
            continue;
        };
        if is_cheatcode_receiver(&callee) {
            continue;
        }
        let key = callee.text().to_string();
        if !callee_cache.contains_key(&key) {
            let v = resolve_callee_overloads(state, uri, root, &callee);
            callee_cache.insert(key.clone(), v);
        }
        let Some(all_overloads) = callee_cache.get(&key).and_then(|v| v.as_ref()) else {
            continue;
        };
        let overloads: Vec<Vec<(String, String)>> = all_overloads
            .iter()
            .filter(|params| params.len() == args.len())
            .cloned()
            .collect();
        if overloads.is_empty() {
            let name = callee_display_name(&callee).unwrap_or_default();
            let counts: Vec<String> = all_overloads.iter().map(|p| p.len().to_string()).collect();
            let span = call
                .children()
                .find(|n| matches!(n.kind(), ARG_LIST | NAMED_ARG_LIST));
            out.push(type_mismatch(
                li,
                span.as_ref().unwrap_or(&call),
                &format!(
                    "`{name}` expects {} argument(s), but {} given",
                    counts.join(" or "),
                    args.len(),
                ),
            ));
            continue;
        }
        let arg_tys: Vec<typecheck::Ty> = args
            .iter()
            .map(|(_, v)| infer_arg_ty(state, uri, root, v))
            .collect();
        let is_base = |a: &str, b: &str| {
            let k = (a.to_string(), b.to_string());
            if let Some(&v) = subtype_memo.borrow().get(&k) {
                return v;
            }
            let v = is_subtype(state, uri, root, a, b);
            subtype_memo.borrow_mut().insert(k, v);
            v
        };
        let param_for = |params: &[(String, String)], i: usize| -> Option<String> {
            match &args[i].0 {
                Some(key) => params
                    .iter()
                    .find(|(pn, _)| pn == key)
                    .map(|(_, t)| t.clone()),
                None => params.get(i).map(|(_, t)| t.clone()),
            }
        };
        let accepts = |params: &[(String, String)]| {
            (0..args.len()).all(|i| {
                param_for(params, i).is_some_and(|p| {
                    typecheck::implicitly_convertible(
                        &arg_tys[i],
                        &typecheck::parse_ty(&p),
                        &is_base,
                    )
                })
            })
        };
        if overloads.iter().any(|p| accepts(p)) {
            continue;
        }
        if overloads.len() == 1 {
            for (i, (_, value)) in args.iter().enumerate() {
                if matches!(arg_tys[i], typecheck::Ty::Unknown) {
                    continue;
                }
                let Some(ptype) = param_for(&overloads[0], i) else {
                    continue;
                };
                if !typecheck::implicitly_convertible(
                    &arg_tys[i],
                    &typecheck::parse_ty(&ptype),
                    &is_base,
                ) {
                    out.push(type_mismatch(li, value, &format!(
                        "argument of type `{}` is not implicitly convertible to expected type `{ptype}`",
                        arg_text(value),
                    )));
                }
            }
        } else {
            let name = callee_display_name(&callee).unwrap_or_default();
            let span = call
                .children()
                .find(|n| matches!(n.kind(), ARG_LIST | NAMED_ARG_LIST));
            out.push(type_mismatch(
                li,
                span.as_ref().unwrap_or(&call),
                &format!("no overload of `{name}` accepts these argument types"),
            ));
        }
    }
    out
}

/// Whether a callee is a member call on a forge-std cheatcode / logging handle.
pub(super) fn is_cheatcode_receiver(callee: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::MEMBER_EXPR;
    if callee.kind() != MEMBER_EXPR {
        return false;
    }
    callee
        .first_child()
        .and_then(|recv| solsp_hir::resolve::receiver_name(&recv))
        .is_some_and(|n| matches!(n.as_str(), "vm" | "console" | "console2"))
}

fn resolve_callee_overloads(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    callee: &solsp_syntax::SyntaxNode,
) -> Option<Overloads> {
    use solsp_hir::resolve::DefKind;
    let (def_uri, def) = resolve_named_callee(state, uri, root, callee)?;
    if !matches!(
        def.kind,
        DefKind::Function | DefKind::Event | DefKind::Error
    ) {
        return None;
    }
    let droot = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&droot);
    let name = callee_display_name(callee)?;
    Some(
        signature_candidates(&def, &def_node, &name, &droot)
            .into_iter()
            .map(|(_, n)| named_arg_fields(DefKind::Function, &n))
            .collect(),
    )
}

fn named_arg_pairs(
    nal: &solsp_syntax::SyntaxNode,
) -> Vec<(Option<String>, solsp_syntax::SyntaxNode)> {
    use solsp_syntax::SyntaxKind::NAME;
    let mut out = Vec::new();
    let mut key: Option<String> = None;
    for child in nal.children() {
        if child.kind() == NAME {
            key = node_ident(&child);
        } else {
            out.push((key.take(), child));
        }
    }
    out
}
