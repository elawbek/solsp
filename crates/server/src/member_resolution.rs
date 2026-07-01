//! Receiver, member, inheritance, and Solidity type resolution helpers.

use super::*;

/// Resolve a receiver expression to the declaration it names - a bare name (`MyError`,
/// `myFunc`) or a qualified one (`Lib.MyError`). For looking up what kind of thing a
/// receiver is, not its type.
pub(super) fn resolve_receiver_def(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<solsp_hir::resolve::Definition> {
    resolve_receiver_def_target(state, uri, root, receiver).map(|(_, _, def)| def)
}

pub(super) fn resolve_receiver_def_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<(
    Url,
    solsp_syntax::SyntaxNode,
    solsp_hir::resolve::Definition,
)> {
    use solsp_syntax::SyntaxKind::{MEMBER_EXPR, NAME_REF, PATH_EXPR};
    match receiver.kind() {
        PATH_EXPR | NAME_REF => {
            let nr = receiver_name_ref(receiver)?;
            if let Some(d) = solsp_hir::resolve::resolve(&nr) {
                return Some((uri.clone(), root.clone(), d));
            }
            let name = nameref_text(&nr)?;
            if let Some(c) = enclosing_contract(receiver) {
                if let Some((target_uri, d)) = inherited_member(state, uri, root, &c, &name, None) {
                    let target_root = parse_root(state, &target_uri)?;
                    return Some((target_uri, target_root, d));
                }
            }
            let (target_uri, d) = cross_file_definition(state, uri, root, &name, None)?;
            let target_root = parse_root(state, &target_uri)?;
            Some((target_uri, target_root, d))
        }
        // `A.B` -> resolve the member `B` at its own offset.
        MEMBER_EXPR => {
            let member_nr = receiver
                .children()
                .filter(|n| n.kind() == NAME_REF)
                .last()?;
            let off = member_nr.text_range().start();
            let (target_uri, d) = member_resolve(state, uri, root, off)?;
            let target_root = parse_root(state, &target_uri)?;
            Some((target_uri, target_root, d))
        }
        _ => None,
    }
}

/// Members of `type(X)`: integer/enum `min`/`max`, contract bytecode metadata, or an
/// interface `interfaceId`. `None` if the receiver isn't `type(X)`.
pub(super) fn type_expr_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<Vec<CompletionItem>> {
    use solsp_syntax::SyntaxKind::{CONTRACT_DEF, ENUM_DEF, PATH_TYPE, TYPE_EXPR};
    if receiver.kind() != TYPE_EXPR {
        return None;
    }
    let pt = receiver.children().find(|n| n.kind() == PATH_TYPE)?;
    let name = solsp_hir::resolve::path_type_segments(&pt).pop()?;
    if is_integer_type_name(&name) {
        let minmax = vec![("min", name.as_str(), false), ("max", name.as_str(), false)];
        return Some(synthetic_members(&minmax));
    }
    if let Some((_, tdef)) = resolve_path_type(state, uri, root, &pt) {
        return Some(match tdef.kind() {
            ENUM_DEF => synthetic_members(&[("min", "", false), ("max", "", false)]),
            CONTRACT_DEF if is_interface_node(&tdef) => {
                synthetic_members(&[("name", "string", false), ("interfaceId", "bytes4", false)])
            }
            CONTRACT_DEF => synthetic_members(&[
                ("name", "string", false),
                ("creationCode", "bytes", false),
                ("runtimeCode", "bytes", false),
            ]),
            _ => Vec::new(),
        });
    }
    Some(Vec::new())
}

/// Builtin members of an `address` / array / `bytes` value, by the receiver's declared
/// type. `None` for other types.
pub(super) fn value_type_builtin_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<Vec<CompletionItem>> {
    let (ty, is_storage) = receiver_value_info(state, uri, root, receiver)?;
    let ty = ty.trim();
    if ty == "address" {
        return Some(synthetic_members(&[
            ("balance", "uint256", false),
            ("code", "bytes", false),
            ("codehash", "bytes32", false),
            ("call", "", true),
            ("delegatecall", "", true),
            ("staticcall", "", true),
        ]));
    }
    if ty == "address payable" {
        return Some(synthetic_members(&[
            ("balance", "uint256", false),
            ("code", "bytes", false),
            ("codehash", "bytes32", false),
            ("call", "", true),
            ("delegatecall", "", true),
            ("staticcall", "", true),
            ("transfer", "", true),
            ("send", "", true),
        ]));
    }
    // a dynamic array or `bytes` - `.length` always; `.push`/`.pop` only in storage.
    if ty.ends_with("[]") || ty == "bytes" {
        let mut m: Vec<(&str, &str, bool)> = vec![("length", "uint256", false)];
        if is_storage {
            m.push(("push", "", true));
            m.push(("pop", "", true));
        }
        return Some(synthetic_members(&m));
    }
    // a fixed-size array `T[N]` or `bytesN` - `.length` only.
    if ty.ends_with(']') || is_fixed_bytes(ty) {
        return Some(synthetic_members(&[("length", "uint256", false)]));
    }
    None
}

/// The `(type text, lives in storage)` of a receiver value: simple/cross-file variables,
/// member accesses, address casts (`address(x)`/`payable(x)`), and the builtin
/// address-returning members (`msg.sender`, `tx.origin`, `block.coinbase`).
pub(super) fn receiver_value_info(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<(String, bool)> {
    use solsp_syntax::SyntaxKind::{CALL_EXPR, INDEX_EXPR, MEMBER_EXPR};
    if receiver.kind() == CALL_EXPR {
        let callee = receiver.first_child()?;
        match callee_display_name(&callee)?.as_str() {
            "address" => return Some(("address".to_string(), false)),
            "payable" => return Some(("address payable".to_string(), false)),
            name => {
                let parsed = typecheck::parse_ty(name);
                if !matches!(parsed, typecheck::Ty::User(_)) {
                    return Some((ty_label(&parsed), false));
                }
            }
        }
        // a function call -> its return type. Library helpers may return storage refs.
        let (duri, def) = resolve_named_callee(state, uri, root, &callee)?;
        let droot = parse_root(state, &duri)?;
        let ret = function_return_param(&def.full_ptr.to_node(&droot))?;
        return Some((type_text(&ret)?, is_storage_decl(&ret)));
    }
    if receiver.kind() == INDEX_EXPR {
        // `base[i]` -> the array element / mapping value type; storage follows the base.
        let base = receiver.first_child()?;
        // a declared array/mapping -> its element/value type (a nested mapping value stays
        // a mapping, which is reportable when a struct is expected).
        if let Some(base_decl) = receiver_decl(state, uri, root, &base) {
            if let Some(t) = indexed_type_text(&base_decl) {
                return Some((t, is_storage_decl(&base_decl)));
            }
        }
        // a nested index / call base -> strip one array level from its type text.
        let (base_ty, storage) = receiver_value_info(state, uri, root, &base)?;
        return Some((base_ty.strip_suffix("[]")?.trim().to_string(), storage));
    }
    if receiver.kind() == MEMBER_EXPR {
        // a builtin global member (`msg.sender`, `msg.data`, `tx.origin`, `block.coinbase`)
        // -> its declared type, so chains like `msg.data.length` resolve.
        let recv = receiver.first_child()?;
        let member = member_name(receiver)?;
        if let Some(items) = builtin_member_items(&recv) {
            if let Some(d) = items
                .iter()
                .find(|i| i.label == member)
                .and_then(|i| i.detail.as_deref())
                .filter(|d| !d.is_empty())
            {
                // drop a data location so the type model sees `bytes`, not `bytes calldata`.
                let ty = d
                    .trim_end_matches(" calldata")
                    .trim_end_matches(" memory")
                    .trim_end_matches(" storage")
                    .to_string();
                return Some((ty, false));
            }
        }
        if let Some(items) = value_type_builtin_members(state, uri, root, &recv) {
            if let Some(d) = items
                .iter()
                .find(|i| i.label == member)
                .and_then(|i| i.detail.as_deref())
                .filter(|d| !d.is_empty())
            {
                let ty = d
                    .trim_end_matches(" calldata")
                    .trim_end_matches(" memory")
                    .trim_end_matches(" storage")
                    .to_string();
                return Some((ty, false));
            }
        }
    }
    let decl = receiver_decl(state, uri, root, receiver)?;
    Some((type_text(&decl)?, is_storage_decl(&decl)))
}

/// Whether a declaration's value lives in storage: a state variable, or a local with the
/// `storage` data location.
pub(super) fn is_storage_decl(decl: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{STATE_VAR_DEF, STORAGE_KW};
    decl.kind() == STATE_VAR_DEF
        || decl
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == STORAGE_KW)
}

/// The declaration node a receiver value refers to: a simple/cross-file variable or a
/// member access.
pub(super) fn receiver_decl(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{MEMBER_EXPR, NAME_REF, PATH_EXPR};
    match receiver.kind() {
        PATH_EXPR | NAME_REF => {
            let nr = receiver_name_ref(receiver)?;
            if let Some(d) = solsp_hir::resolve::resolve(&nr) {
                return Some(d.full_ptr.to_node(root));
            }
            // a cross-file inherited variable.
            let name = solsp_hir::resolve::receiver_name(receiver)?;
            let c = enclosing_contract(receiver)?;
            let (duri, d) = inherited_member(state, uri, root, &c, &name, None)?;
            let droot = parse_root(state, &duri)?;
            Some(d.full_ptr.to_node(&droot))
        }
        MEMBER_EXPR => {
            let recv = receiver.first_child()?;
            let member = member_name(receiver)?;
            let (turi, tdef) = receiver_type(state, uri, root, &recv, false)?;
            let troot = parse_root(state, &turi)?;
            let (muri, mdef) = if is_super_receiver(&recv) {
                inherited_base_member(state, &turi, &troot, &tdef, &member, None)?
            } else {
                (
                    turi.clone(),
                    member_lookup(state, &turi, &tdef, &member, None)?,
                )
            };
            let mroot = parse_root(state, &muri)?;
            Some(mdef.full_ptr.to_node(&mroot))
        }
        _ => None,
    }
}

/// Whether a `CONTRACT_DEF` node is a `library`.
pub(super) fn is_library_node(c: &solsp_syntax::SyntaxNode) -> bool {
    c.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == solsp_syntax::SyntaxKind::LIBRARY_KW)
}

/// Whether a `CONTRACT_DEF` node is an `interface`.
pub(super) fn is_interface_node(c: &solsp_syntax::SyntaxNode) -> bool {
    c.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == solsp_syntax::SyntaxKind::INTERFACE_KW)
}

pub(super) fn is_super_receiver(receiver: &solsp_syntax::SyntaxNode) -> bool {
    solsp_hir::resolve::receiver_name(receiver).as_deref() == Some("super")
}

/// Whether a receiver is a *value* (a contract instance) rather than a bare type name -
/// i.e. `instance.member` (external access) vs `Type.member` (static).
pub(super) fn is_instance_receiver(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> bool {
    use solsp_syntax::SyntaxKind::{NAME_REF, PATH_EXPR};
    if !matches!(receiver.kind(), PATH_EXPR | NAME_REF) {
        return true; // a member/call/index expression is always a value
    }
    let Some(name) = solsp_hir::resolve::receiver_name(receiver) else {
        return true;
    };
    let resolved = receiver_name_ref(receiver)
        .and_then(|nr| solsp_hir::resolve::resolve(&nr))
        .or_else(|| cross_file_definition(state, uri, root, &name, None).map(|(_, d)| d));
    // a bare name that resolves to a type (library/contract/struct/enum) is a static
    // receiver; anything else (a variable, or unresolved) is treated as an instance.
    !resolved.map(|d| is_type_kind(d.kind)).unwrap_or(false)
}

/// The receiver expression of a `receiver.member` access at `offset`, when the cursor is
/// on the member side (after the `.`). `None` otherwise.
pub(super) fn dotted_receiver(
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{DOT, MEMBER_EXPR};
    let tok = root.token_at_offset(offset).left_biased()?;
    let member_expr = tok
        .parent()?
        .ancestors()
        .find(|n| n.kind() == MEMBER_EXPR)?;
    let dot = member_expr
        .children_with_tokens()
        .find(|e| e.kind() == DOT)?;
    if offset < dot.text_range().end() {
        return None; // cursor is in the receiver, not after the dot
    }
    member_expr.first_child()
}

/// Resolve a member access `receiver.member` at `offset`: returns the target file URI
/// and the member's [`Definition`]. Handles a receiver that is a type name
/// (contract/library/interface/struct/enum) or a variable (following its declared
/// type), same-file or imported.
pub(super) fn member_resolve(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_syntax::SyntaxKind;
    // the clicked identifier must be the member side of a `receiver.member`.
    let token = root
        .token_at_offset(offset)
        .find(|t| t.kind() == SyntaxKind::IDENT)?;
    let member_ref = token.parent()?;
    let (receiver, member) = solsp_hir::resolve::member_access(&member_ref)?;
    // `obj.method(args)` - pick the overload matching the call's argument count.
    let arity = solsp_hir::resolve::call_arity(&member_ref);

    // `N.member` where `N` is an `import * as N` namespace alias -> the imported file's
    // top-level symbol.
    if let Some(found) = namespace_member(state, uri, root, &receiver, &member, arity) {
        return Some(found);
    }

    if let Some((type_uri, type_def)) = resolve_receiver_type(state, uri, root, &receiver) {
        if is_super_receiver(&receiver) {
            let troot = parse_root(state, &type_uri)?;
            return inherited_base_member(state, &type_uri, &troot, &type_def, &member, arity);
        }
        if let Some(def) = member_lookup(state, &type_uri, &type_def, &member, arity) {
            return Some((type_uri, def));
        }
        // the member may be inherited from a cross-file base of the receiver's type.
        if let Some(troot) = parse_root(state, &type_uri) {
            if let Some(found) =
                inherited_member(state, &type_uri, &troot, &type_def, &member, arity)
            {
                return Some(found);
            }
        }
    }
    // `using L for T` - a library function attached to the receiver's type.
    using_member(state, uri, root, &receiver, &member, arity)
}

/// The file a `* as N` namespace import aliases, if `receiver` is that bare alias `N`.
pub(super) fn namespace_target_uri(
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<Url> {
    use solsp_hir::imports::ImportKind::Namespace;
    use solsp_syntax::SyntaxKind::{NAME_REF, PATH_EXPR};
    if !matches!(receiver.kind(), PATH_EXPR | NAME_REF) {
        return None;
    }
    let alias = solsp_hir::resolve::receiver_name(receiver)?;
    solsp_hir::imports::imports(root)
        .into_iter()
        .find_map(|imp| {
            matches!(&imp.kind, Namespace(a) if *a == alias)
                .then(|| state::resolve_import_uri(uri, &imp.path))
                .flatten()
        })
}

/// Resolve `N.member` where `N` is a `* as N` namespace alias -> the imported file's
/// top-level symbol (following re-exports).
pub(super) fn namespace_member(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
    member: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    let turi = namespace_target_uri(uri, root, receiver)?;
    let tfile = state.file(&turi)?;
    let troot = solsp_base_db::parse(state.db(), tfile).syntax();
    if let Some(def) = solsp_hir::resolve::top_level_definition(&troot, member, arity) {
        return Some((turi, def));
    }
    cross_file_definition(state, &turi, &troot, member, arity)
}

/// Resolve the receiver of a member access to its type definition node and the file
/// that node lives in.
pub(super) fn resolve_receiver_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    receiver_type(state, uri, root, receiver, false)
}

/// The type definition of an expression (structural, recursive). With `element`, the
/// array element type (for an indexed expression). Handles names, member access, calls
/// (-> the function's return type), indexing, and parentheses - so a chain like
/// `a.b().c[d].e` resolves segment by segment.
pub(super) fn receiver_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    expr: &solsp_syntax::SyntaxNode,
    element: bool,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    use solsp_syntax::SyntaxKind::*;
    match expr.kind() {
        PAREN_EXPR | TUPLE_EXPR => receiver_type(state, uri, root, &expr.first_child()?, element),
        INDEX_EXPR => receiver_type(state, uri, root, &expr.first_child()?, true),
        CALL_EXPR => call_result_type(state, uri, root, expr, element),
        MEMBER_EXPR => {
            let recv = expr.first_child()?;
            let member = member_name(expr)?;
            // `N.Type` where `N` is an `import * as N` namespace alias -> the imported type.
            if let Some((turi, def)) = namespace_member(state, uri, root, &recv, &member, None) {
                if is_type_kind(def.kind) && !element {
                    let troot = parse_root(state, &turi)?;
                    return Some((turi, def.full_ptr.to_node(&troot)));
                }
            }
            let (turi, tdef) = receiver_type(state, uri, root, &recv, false)?;
            let troot = parse_root(state, &turi)?;
            let (muri, mdef) = if is_super_receiver(&recv) {
                inherited_base_member(state, &turi, &troot, &tdef, &member, None)?
            } else {
                (
                    turi.clone(),
                    member_lookup(state, &turi, &tdef, &member, None)?,
                )
            };
            let mroot = parse_root(state, &muri)?;
            member_value_type(state, &muri, &mroot, &mdef, element)
        }
        PATH_EXPR | NAME_REF => {
            // `this` / `super` -> the enclosing contract's type.
            if !element {
                if let Some(name) = solsp_hir::resolve::receiver_name(expr) {
                    if (name == "this" || name == "super") && enclosing_contract(expr).is_some() {
                        return Some((uri.clone(), enclosing_contract(expr)?));
                    }
                }
            }
            resolve_value_type(state, uri, root, expr, element)
        }
        _ => None,
    }
}

/// The result type of a call expression: the callee's return type, or - for a cast /
/// constructor `Type(x)` - the type itself.
pub(super) fn call_result_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    call: &solsp_syntax::SyntaxNode,
    element: bool,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    use solsp_hir::resolve::DefKind;
    let callee = call.first_child()?;
    let arity = arg_count(call);
    let (def_uri, def) = resolve_callee(state, uri, root, &callee, arity)?;
    let def_root = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&def_root);
    match def.kind {
        DefKind::Function => {
            let ret = function_return_param(&def_node)?;
            let path_type = solsp_hir::resolve::decl_type_path(&ret, element)?;
            resolve_path_type(state, &def_uri, &def_root, &path_type)
        }
        _ if is_type_kind(def.kind) && !element => Some((def_uri, def_node)),
        _ => None,
    }
}

/// Resolve a call's callee to its declaration: a plain name (function, or a type for a
/// cast/constructor), or a member `obj.method`.
pub(super) fn resolve_callee(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    callee: &solsp_syntax::SyntaxNode,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_syntax::SyntaxKind::*;
    match callee.kind() {
        PATH_EXPR | NAME_REF => {
            let nr = receiver_name_ref(callee)?;
            if let Some(def) = solsp_hir::resolve::resolve(&nr) {
                return Some((uri.clone(), def));
            }
            let name = solsp_hir::resolve::receiver_name(callee)?;
            if let Some(found) = cross_file_definition(state, uri, root, &name, arity) {
                return Some(found);
            }
            // a bare call to an internal/private method inherited from a cross-file base.
            let contract = enclosing_contract(callee)?;
            inherited_member(state, uri, root, &contract, &name, arity)
        }
        MEMBER_EXPR => {
            let recv = callee.first_child()?;
            let member = member_name(callee)?;
            let (turi, tdef) = receiver_type(state, uri, root, &recv, false)?;
            if is_super_receiver(&recv) {
                let troot = parse_root(state, &turi)?;
                return inherited_base_member(state, &turi, &troot, &tdef, &member, arity);
            }
            // same-file C3 first, then cross-file inheritance.
            if let Some(mdef) = member_lookup(state, &turi, &tdef, &member, arity) {
                return Some((turi, mdef));
            }
            let troot = parse_root(state, &turi)?;
            inherited_member(state, &turi, &troot, &tdef, &member, arity)
        }
        _ => None,
    }
}

/// The type of a member (`a.b` as a value): a field/variable follows its declared type;
/// a nested type is itself. With `element`, the array element type.
pub(super) fn member_value_type(
    state: &ServerState,
    member_uri: &Url,
    member_root: &solsp_syntax::SyntaxNode,
    mdef: &solsp_hir::resolve::Definition,
    element: bool,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    use solsp_hir::resolve::DefKind;
    let node = mdef.full_ptr.to_node(member_root);
    match mdef.kind {
        DefKind::Contract
        | DefKind::Interface
        | DefKind::Library
        | DefKind::Struct
        | DefKind::Enum
            if !element =>
        {
            Some((member_uri.clone(), node))
        }
        DefKind::StateVariable | DefKind::Field | DefKind::Local | DefKind::Parameter => {
            let path_type = solsp_hir::resolve::decl_type_path(&node, element)?;
            resolve_path_type(state, member_uri, member_root, &path_type)
        }
        _ => None,
    }
}

/// The member name of a `MEMBER_EXPR` (`b` in `a.b`).
pub(super) fn member_name(member_expr: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::{IDENT, NAME_REF};
    let nr = member_expr.children().nth(1)?; // [receiver, member]
    if nr.kind() != NAME_REF {
        return None;
    }
    nr.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == IDENT)
        .map(|t| t.text().to_string())
}

/// The argument count of a call's `ARG_LIST` / `NAMED_ARG_LIST`.
pub(super) fn arg_count(call: &solsp_syntax::SyntaxNode) -> Option<usize> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, NAMED_ARG_LIST};
    let args = call
        .children()
        .find(|n| matches!(n.kind(), ARG_LIST | NAMED_ARG_LIST))?;
    Some(args.children().count())
}

/// The first `PARAM` of a function's return list (its second `PARAM_LIST`).
pub(super) fn function_return_param(
    func: &solsp_syntax::SyntaxNode,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{PARAM, PARAM_LIST};
    let returns = func.children().filter(|n| n.kind() == PARAM_LIST).nth(1)?;
    returns.children().find(|n| n.kind() == PARAM)
}

/// Resolve a receiver to a type def. With `element`, take the array element type
/// (the receiver was indexed). A bare type name resolves to itself; a variable follows
/// its declared type. Same-file lexical resolution first, then imported symbols.
pub(super) fn resolve_value_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
    element: bool,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    use solsp_hir::resolve::DefKind;
    let name = solsp_hir::resolve::receiver_name(receiver)?;
    let recv_ref = receiver_name_ref(receiver)?;

    let (def_uri, def) = solsp_hir::resolve::resolve(&recv_ref)
        .map(|d| (uri.clone(), d))
        .or_else(|| cross_file_definition(state, uri, root, &name, None))
        .or_else(|| {
            // an inherited member from a cross-file base (e.g. forge-std's `vm`).
            let contract = enclosing_contract(receiver)?;
            inherited_member(state, uri, root, &contract, &name, None)
        })?;
    let def_root = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&def_root);

    match def.kind {
        // the receiver IS a type (only meaningful without indexing).
        DefKind::Contract
        | DefKind::Interface
        | DefKind::Library
        | DefKind::Struct
        | DefKind::Enum
            if !element =>
        {
            Some((def_uri, def_node))
        }
        // the receiver is a value; follow its declared (or element) type path.
        DefKind::StateVariable | DefKind::Parameter | DefKind::Local => {
            let path_type = solsp_hir::resolve::decl_type_path(&def_node, element)?;
            resolve_path_type(state, &def_uri, &def_root, &path_type)
        }
        _ => None,
    }
}

/// The nearest enclosing contract/interface/library definition of a node.
pub(super) fn enclosing_contract(
    node: &solsp_syntax::SyntaxNode,
) -> Option<solsp_syntax::SyntaxNode> {
    node.ancestors()
        .find(|n| n.kind() == solsp_syntax::SyntaxKind::CONTRACT_DEF)
}

/// Resolve a base contract name to its definition node and file - same-file first, then
/// an imported base.
pub(super) fn resolve_base(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    base_name: &str,
) -> Option<(Url, solsp_syntax::SyntaxNode, solsp_syntax::SyntaxNode)> {
    if let Some(node) = solsp_hir::resolve::find_contract(root, base_name) {
        return Some((uri.clone(), root.clone(), node));
    }
    let (buri, bdef) = cross_file_definition(state, uri, root, base_name, None)?;
    if !is_type_kind(bdef.kind) {
        return None;
    }
    let broot = parse_root(state, &buri)?;
    let bnode = bdef.full_ptr.to_node(&broot);
    Some((buri, broot, bnode))
}

/// Look up `name` as a member inherited by `contract`, walking its base contracts across
/// files (BFS, diamond-safe). Returns the file + [`Definition`] of the first match.
pub(super) fn inherited_member(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    inherited_member_impl(state, uri, root, contract, name, arity, true)
}

/// Resolve a `super.name` target: direct and transitive bases only, never the current
/// contract's override.
pub(super) fn inherited_base_member(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    inherited_member_impl(state, uri, root, contract, name, arity, false)
}

fn inherited_member_impl(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
    include_self: bool,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use std::collections::{HashSet, VecDeque};
    let mut visited: HashSet<(Url, String)> = HashSet::new();
    // (uri, root, contract, is_base) - a base's `private` member is not accessible here.
    let mut queue: VecDeque<(
        Url,
        solsp_syntax::SyntaxNode,
        solsp_syntax::SyntaxNode,
        bool,
    )> = VecDeque::new();
    if include_self {
        queue.push_back((uri.clone(), root.clone(), contract.clone(), false));
    } else {
        for base in solsp_hir::resolve::base_names(contract) {
            if let Some((bu, br, bn)) = resolve_base(state, uri, root, &base) {
                queue.push_back((bu, br, bn, true));
            }
        }
    }
    while let Some((u, r, c, is_base)) = queue.pop_front() {
        let key = (
            u.clone(),
            solsp_hir::resolve::contract_def_name(&c).unwrap_or_default(),
        );
        if !visited.insert(key) {
            continue; // already searched this contract (diamond)
        }
        if let Some(def) = solsp_hir::resolve::contract_member(&c, name, arity) {
            if !is_base || !solsp_hir::resolve::is_private(&def.full_ptr.to_node(&r)) {
                return Some((u, def));
            }
            // a private base member - not accessible from here; keep searching.
        }
        for base in solsp_hir::resolve::base_names(&c) {
            if let Some((bu, br, bn)) = resolve_base(state, &u, &r, &base) {
                queue.push_back((bu, br, bn, true));
            }
        }
    }
    None
}

/// Go-to-def target for a bare name used inside a contract that resolves to an inherited
/// member from a cross-file base (e.g. a forge-std `Test` helper). Skips member-access
/// positions (handled by member resolution).
pub(super) fn inherited_name_at(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_syntax::SyntaxKind::{IDENT, MEMBER_EXPR, NAME_REF};
    let token = root.token_at_offset(offset).find(|t| t.kind() == IDENT)?;
    let nr = token.parent()?;
    if nr.kind() != NAME_REF {
        return None;
    }
    // the `.member` of `recv.member` is member resolution's job, not inheritance.
    if let Some(p) = nr.parent() {
        if p.kind() == MEMBER_EXPR && p.first_child().as_ref() != Some(&nr) {
            return None;
        }
    }
    let contract = enclosing_contract(&nr)?;
    let arity = solsp_hir::resolve::call_arity(&nr);
    inherited_member(state, uri, root, &contract, token.text(), arity)
}

/// Resolve a type path node (`IRoles` or qualified `ICraftV2.TokenInput`) to its type
/// definition and file: the first segment is a top-level/imported type, each further
/// segment a nested type member of the previous.
pub(super) fn resolve_path_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    path_type: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    let segments = solsp_hir::resolve::path_type_segments(path_type);
    let (first, rest) = segments.split_first()?;
    let (turi, mut type_def) = resolve_type_by_name(state, uri, root, first, Some(path_type))?;
    for seg in rest {
        let member = member_lookup(state, &turi, &type_def, seg, None)?;
        if !is_type_kind(member.kind) {
            return None;
        }
        let troot = parse_root(state, &turi)?; // nested types live in the same file
        type_def = member.full_ptr.to_node(&troot);
    }
    Some((turi, type_def))
}

/// Resolve a *type* name to its definition node and file: same-file top-level first,
/// then an imported type.
pub(super) fn resolve_type_by_name(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    type_name: &str,
    context: Option<&solsp_syntax::SyntaxNode>,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    // resolving a type name cross-file is hot and repeats across a file's many uses of the
    // same type, so memoize it keyed by (file, name, enclosing contract).
    let key = (
        uri.to_string(),
        type_name.to_string(),
        context.and_then(enclosing_contract).map(|c| c.text_range()),
    );
    let resolved = match state.cached_type(&key) {
        Some(hit) => hit,
        None => {
            let r = resolve_type_def_by_name(state, uri, root, type_name, context);
            state.cache_type(key, r.clone());
            r
        }
    };
    let (turi, def) = resolved?;
    let troot = parse_root(state, &turi)?;
    Some((turi, def.full_ptr.to_node(&troot)))
}

/// The uncached resolution behind [`resolve_type_by_name`], returning the definition (the
/// node is rebuilt by the caller from the cache).
fn resolve_type_def_by_name(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    type_name: &str,
    context: Option<&solsp_syntax::SyntaxNode>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    // 1. a contract-nested type visible where the name is used (its enclosing contract +
    //    cross-file bases) - these shadow file scope.
    if let Some(contract) = context.and_then(enclosing_contract) {
        if let Some(def) = member_lookup(state, uri, &contract, type_name, None) {
            if is_type_kind(def.kind) {
                return Some((uri.clone(), def));
            }
        }
        if let Some((turi, def)) = inherited_member(state, uri, root, &contract, type_name, None) {
            if is_type_kind(def.kind) {
                return Some((turi, def));
            }
        }
    }
    // 2. a top-level type in this file (via the cached file index).
    if let Some(index) = state.file_index(uri) {
        if let Some(def) = solsp_hir::resolve::select_named(&index.defs, type_name, None, root) {
            if is_type_kind(def.kind) {
                return Some((uri.clone(), def));
            }
        }
    }
    // 3. an imported type.
    let (turi, def) = cross_file_definition(state, uri, root, type_name, None)?;
    is_type_kind(def.kind).then_some((turi, def))
}

pub(super) fn is_type_kind(kind: solsp_hir::resolve::DefKind) -> bool {
    use solsp_hir::resolve::DefKind::*;
    matches!(
        kind,
        Contract | Interface | Library | Struct | Enum | UserType
    )
}

/// Look up `member` in a type, caching a contract's member list to avoid re-walking its
/// body and same-file C3 bases on every access (the dominant member-resolution cost on
/// big types). `type_uri` is the file `type_def` lives in. Only the common arity-free
/// contract lookup is cached; struct/enum and overload-by-arity take the direct path,
/// which preserves exact base-precedence semantics.
pub(super) fn member_lookup(
    state: &ServerState,
    type_uri: &Url,
    type_def: &solsp_syntax::SyntaxNode,
    member: &str,
    arity: Option<usize>,
) -> Option<solsp_hir::resolve::Definition> {
    if arity.is_some() || type_def.kind() != solsp_syntax::SyntaxKind::CONTRACT_DEF {
        return solsp_hir::resolve::member_in_type(type_def, member, arity);
    }
    // arity-free contract lookup = first member of that name in C3 order.
    state
        .member_index(type_uri, type_def)
        .iter()
        .find(|d| d.name == member)
        .cloned()
}

/// The `NAME_REF` node of a receiver expression (`PATH_EXPR` -> `NAME_REF`, or a bare
/// `NAME_REF`).
pub(super) fn receiver_name_ref(
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::NAME_REF;
    if receiver.kind() == NAME_REF {
        Some(receiver.clone())
    } else {
        receiver.children().find(|n| n.kind() == NAME_REF)
    }
}
