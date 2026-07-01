//! `solsp-server` library: the LSP protocol layer (capabilities, dispatch loop,
//! handlers) over the pure `solsp-ide` features. The `solsp-server` binary is a thin
//! shim around [`run`]; integration tests drive the same code over an in-memory
//! transport (design §5, §6).

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
    CodeLens, CodeLensParams, Command, CompletionItem, CompletionItemKind, CompletionParams,
    CompletionResponse, Hover, HoverParams, Location, ParameterInformation, ParameterLabel,
    ReferenceParams, RenameParams, SignatureHelp, SignatureHelpParams, SignatureInformation,
    TextEdit, Url, WorkspaceEdit,
};

mod abi;
mod builtins;
mod capabilities;
mod code_actions;
mod completion_items;
mod contract_diagnostics;
mod diagnostics;
mod flow_diagnostics;
mod graphs;
mod import_diagnostics;
mod import_surface;
mod interaction;
mod lsp_loop;
mod mutability;
mod name_diagnostics;
mod named_args;
mod navigation;
mod perf;
mod protocol;
mod references;
pub mod state;
mod syntax_utils;
pub mod to_proto;
mod type_diagnostics;
pub mod typecheck;
mod usage_diagnostics;
mod using_for;

pub use capabilities::server_capabilities;
pub use lsp_loop::{run, run_with_root};

use builtins::{
    builtin_items, builtin_member_items, is_builtin_name, is_fixed_bytes, is_integer_type_name,
    synthetic_members, yul_builtin, yul_builtin_items,
};
use completion_items::completion_items_from;
use contract_diagnostics::{
    declaration_name, declaration_name_range, function_arity, function_has_override,
    function_label, function_name, function_name_range, function_visibility, member_visibility,
};
use import_surface::{collect_file_exports, imported_symbols, namespace_alias_items};
use named_args::{named_arg_completion, named_arg_fields, named_arg_hover};
use protocol::markup_hover;
use references::{has_reference_count_at_least, RefTarget};
use state::ServerState;
use syntax_utils::{
    indexed_type_text, named_type, node_ident, node_type_text, param_name_types, type_text,
};
use using_for::{using_member, using_member_items};

/// Resolve a receiver expression to the declaration it names — a bare name (`MyError`,
/// `myFunc`) or a qualified one (`Lib.MyError`). For looking up what kind of thing a
/// receiver is, not its type.
fn resolve_receiver_def(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<solsp_hir::resolve::Definition> {
    resolve_receiver_def_target(state, uri, root, receiver).map(|(_, _, def)| def)
}

fn resolve_receiver_def_target(
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
        // `A.B` → resolve the member `B` at its own offset.
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
fn type_expr_members(
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
fn value_type_builtin_members(
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
    // a dynamic array or `bytes` — `.length` always; `.push`/`.pop` only in storage.
    if ty.ends_with("[]") || ty == "bytes" {
        let mut m: Vec<(&str, &str, bool)> = vec![("length", "uint256", false)];
        if is_storage {
            m.push(("push", "", true));
            m.push(("pop", "", true));
        }
        return Some(synthetic_members(&m));
    }
    // a fixed-size array `T[N]` or `bytesN` — `.length` only.
    if ty.ends_with(']') || is_fixed_bytes(ty) {
        return Some(synthetic_members(&[("length", "uint256", false)]));
    }
    None
}

/// The `(type text, lives in storage)` of a receiver value: simple/cross-file variables,
/// member accesses, address casts (`address(x)`/`payable(x)`), and the builtin
/// address-returning members (`msg.sender`, `tx.origin`, `block.coinbase`).
fn receiver_value_info(
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
        // a function call → its return type. Library helpers may return storage refs.
        let (duri, def) = resolve_named_callee(state, uri, root, &callee)?;
        let droot = parse_root(state, &duri)?;
        let ret = function_return_param(&def.full_ptr.to_node(&droot))?;
        return Some((type_text(&ret)?, is_storage_decl(&ret)));
    }
    if receiver.kind() == INDEX_EXPR {
        // `base[i]` → the array element / mapping value type; storage follows the base.
        let base = receiver.first_child()?;
        // a declared array/mapping → its element/value type (a nested mapping value stays
        // a mapping, which is reportable when a struct is expected).
        if let Some(base_decl) = receiver_decl(state, uri, root, &base) {
            if let Some(t) = indexed_type_text(&base_decl) {
                return Some((t, is_storage_decl(&base_decl)));
            }
        }
        // a nested index / call base → strip one array level from its type text.
        let (base_ty, storage) = receiver_value_info(state, uri, root, &base)?;
        return Some((base_ty.strip_suffix("[]")?.trim().to_string(), storage));
    }
    if receiver.kind() == MEMBER_EXPR {
        // a builtin global member (`msg.sender`, `msg.data`, `tx.origin`, `block.coinbase`)
        // → its declared type, so chains like `msg.data.length` resolve.
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
fn is_storage_decl(decl: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{STATE_VAR_DEF, STORAGE_KW};
    decl.kind() == STATE_VAR_DEF
        || decl
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == STORAGE_KW)
}

/// The declaration node a receiver value refers to: a simple/cross-file variable or a
/// member access.
fn receiver_decl(
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
fn is_library_node(c: &solsp_syntax::SyntaxNode) -> bool {
    c.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == solsp_syntax::SyntaxKind::LIBRARY_KW)
}

/// Whether a `CONTRACT_DEF` node is an `interface`.
fn is_interface_node(c: &solsp_syntax::SyntaxNode) -> bool {
    c.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == solsp_syntax::SyntaxKind::INTERFACE_KW)
}

fn is_super_receiver(receiver: &solsp_syntax::SyntaxNode) -> bool {
    solsp_hir::resolve::receiver_name(receiver).as_deref() == Some("super")
}

/// Whether a receiver is a *value* (a contract instance) rather than a bare type name —
/// i.e. `instance.member` (external access) vs `Type.member` (static).
fn is_instance_receiver(
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

/// The display name of a call's callee: `f` / `S` / `obj.method` / `new T`.
fn callee_display_name(callee: &solsp_syntax::SyntaxNode) -> Option<String> {
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
fn signature_candidates(
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

/// Resolve a named-call callee to its declaration: `new T(...)` → the type `T`, else a
/// function/struct/contract name or `obj.method`.
fn resolve_named_callee(
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

/// The receiver expression of a `receiver.member` access at `offset`, when the cursor is
/// on the member side (after the `.`). `None` otherwise.
fn dotted_receiver(
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

/// All members inherited by `contract` from its base contracts across files (BFS,
/// diamond-safe). Each contract contributes its own direct members.
fn collect_inherited_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    // external access (`instance.member`) → only `public`/`external` members.
    external_only: bool,
) -> Vec<solsp_hir::resolve::Definition> {
    collect_inherited_members_impl(state, uri, root, contract, external_only, true)
}

/// All members reachable through `super`: direct and transitive base contracts only.
fn collect_base_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    external_only: bool,
) -> Vec<solsp_hir::resolve::Definition> {
    collect_inherited_members_impl(state, uri, root, contract, external_only, false)
}

fn collect_inherited_members_impl(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    external_only: bool,
    include_self: bool,
) -> Vec<solsp_hir::resolve::Definition> {
    use std::collections::{HashSet, VecDeque};
    let mut visited: HashSet<(Url, String)> = HashSet::new();
    // (uri, root, contract, is_base) — a base's `private` members are not inherited.
    let mut queue: VecDeque<(
        Url,
        solsp_syntax::SyntaxNode,
        solsp_syntax::SyntaxNode,
        bool,
    )> = VecDeque::new();
    let mut out = Vec::new();
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
            continue;
        }
        for def in solsp_hir::resolve::contract_members(&c) {
            let node = def.full_ptr.to_node(&r);
            if external_only {
                if !solsp_hir::resolve::is_externally_visible(&node) {
                    continue;
                }
            } else if is_base && solsp_hir::resolve::is_private(&node) {
                continue;
            }
            out.push(def);
        }
        for base in solsp_hir::resolve::base_names(&c) {
            if let Some((bu, br, bn)) = resolve_base(state, &u, &r, &base) {
                queue.push_back((bu, br, bn, true));
            }
        }
    }
    out
}

/// When the cursor is on the callee of an overloaded call, pick the overload by argument
/// types (not just count) — returns the matching overload only when exactly one accepts the
/// arguments, so ambiguous/un-inferrable cases fall back to the default arity resolution.
fn typed_overload_target(
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
    // the cursor must be on the callee, not inside an argument.
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
        return None; // not overloaded — nothing to disambiguate
    }
    use solsp_syntax::SyntaxKind::NAMED_ARG_LIST;
    // arguments, positional (`key = None`) or named (`key = Some`).
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
            // a named arg matches its parameter by key; a positional one by position.
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
        return None; // ambiguous (e.g. un-inferrable args accept several) — fall back
    }
    let def = solsp_hir::resolve::definition(node)?;
    Some((def_uri, def))
}

/// A readable Solidity name for a type in a diagnostic message.
fn ty_label(ty: &typecheck::Ty) -> String {
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

/// The identifier text of a `NAME_REF`.
fn nameref_text(nr: &solsp_syntax::SyntaxNode) -> Option<String> {
    nr.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == solsp_syntax::SyntaxKind::IDENT)
        .map(|t| t.text().to_string())
}

/// Whether `name` (used as a value at `nr`) resolves to any declaration — a builtin,
/// something in scope, a same-file top-level, a cross-file import, or an inherited member.
fn name_defined(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    nr: &solsp_syntax::SyntaxNode,
    name: &str,
) -> bool {
    if is_builtin_name(name) {
        return true;
    }
    if solsp_hir::resolve::resolve(nr).is_some()
        || solsp_hir::resolve::top_level_definition(root, name, None).is_some()
    {
        return true;
    }
    if let Some(c) = enclosing_contract(nr) {
        if inherited_member(state, uri, root, &c, name, None).is_some() {
            return true;
        }
    }
    cross_file_definition(state, uri, root, name, None).is_some()
}

/// A callable's overloads, each as its parameter `(name, type)` list.
type Overloads = Vec<Vec<(String, String)>>;

/// Type-check the positional call arguments in `root`: an argument whose inferred type is
/// not implicitly convertible to the parameter type yields a diagnostic. Conservative —
/// anything un-inferrable is left alone (see [`crate::typecheck`]).
fn type_check_diagnostics(
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
    // per-run caches: the same callee text resolves to the same overloads, and the same
    // (subtype, base) pair has a stable answer. Without this, a big forge-std-heavy test
    // file re-walked huge cheatcode files once per call and took tens of seconds.
    let mut callee_cache: HashMap<String, Option<Overloads>> = HashMap::new();
    let subtype_memo: RefCell<HashMap<(String, String), bool>> = RefCell::new(HashMap::new());

    for call in root.descendants().filter(|n| n.kind() == CALL_EXPR) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break; // background budget spent — the file's open/save pass will finish it
        }
        // the arguments: positional (`key = None`) or named (`key = Some`).
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
        // skip cheatcode / logging calls (`vm.*`, `console.*`): resolving them walks huge
        // forge-std files for no benefit, and they dominate test files.
        if is_cheatcode_receiver(&callee) {
            continue;
        }
        // every overload's parameter list, resolved once per distinct callee text.
        let key = callee.text().to_string();
        if !callee_cache.contains_key(&key) {
            let v = resolve_callee_overloads(state, uri, root, &callee);
            callee_cache.insert(key.clone(), v);
        }
        let Some(all_overloads) = callee_cache.get(&key).and_then(|v| v.as_ref()) else {
            continue;
        };
        // those of the matching arity (a small set; cloning keeps the rest simple).
        let overloads: Vec<Vec<(String, String)>> = all_overloads
            .iter()
            .filter(|params| params.len() == args.len())
            .cloned()
            .collect();
        if overloads.is_empty() {
            // no overload takes this many arguments — an arity error.
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
        // infer the argument types once; `Unknown` args never contribute a mismatch.
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
        // the parameter type an argument targets — by name for a named arg, else by
        // position. `None` if a named key doesn't match any parameter.
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
        // a call is valid if SOME overload accepts every argument (Solidity resolves
        // overloads by argument type, which we approximate this way).
        if overloads.iter().any(|p| accepts(p)) {
            continue;
        }
        if overloads.len() == 1 {
            // unambiguous: flag each argument the single signature rejects.
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
            // overloaded and none matched → one diagnostic on the call.
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

/// Whether a callee is a member call on a forge-std cheatcode / logging handle
/// (`vm.*`, `console.*`, `console2.*`) — cheap to detect and not worth type-checking.
fn is_cheatcode_receiver(callee: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::MEMBER_EXPR;
    if callee.kind() != MEMBER_EXPR {
        return false;
    }
    callee
        .first_child()
        .and_then(|recv| solsp_hir::resolve::receiver_name(&recv))
        .is_some_and(|n| matches!(n.as_str(), "vm" | "console" | "console2"))
}

/// Every overload's parameter list (`(name, type)` pairs) for a call's callee, resolved
/// once per distinct callee. `None` for casts/types/builtins/unresolved/non-callables.
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

/// The `(key, value)` pairs of a `NAMED_ARG_LIST` (`{ a: x, b: y }`).
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

fn arg_text(arg: &solsp_syntax::SyntaxNode) -> String {
    arg.text()
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn type_mismatch(
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
fn infer_arg_ty(
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
                // a hex literal (`0x…`) may also be an address / fixed-bytes value.
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
            // `new T[](n)` / `new T(...)` → the constructed type (the node after `new`).
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
            // an elementary cast: `uint8(x)`, `address(x)`, `bytes32(x)`.
            if !matches!(parsed, typecheck::Ty::User(_)) {
                return parsed;
            }
            // a user name: a contract/struct cast, or a function call (use its return type).
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

/// Whether user type `a` is `b` or has `b` somewhere in its inheritance (bases /
/// implemented interfaces). Resolves `a` from the caller's file; `true` when `a` can't be
/// resolved (conservative).
fn is_subtype(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    a: &str,
    b: &str,
) -> bool {
    use std::collections::{HashSet, VecDeque};
    if a == b {
        return true;
    }
    let Some((auri, anode)) = resolve_type_by_name(state, uri, root, a, None) else {
        return true; // unknown type — never flag
    };
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue = VecDeque::from([(auri, anode)]);
    while let Some((u, c)) = queue.pop_front() {
        let Some(cr) = parse_root(state, &u) else {
            continue;
        };
        for base in solsp_hir::resolve::base_names(&c) {
            if base == b {
                return true;
            }
            if visited.insert(base.clone()) {
                if let Some((buri, _, bnode)) = resolve_base(state, &u, &cr, &base) {
                    queue.push_back((buri, bnode));
                }
            }
        }
    }
    false
}

/// The argument count of the call whose callee is the identifier at `offset` (for
/// overload resolution), or `None` if the cursor is not on a callee.
fn arity_at(root: &solsp_syntax::SyntaxNode, offset: rowan::TextSize) -> Option<usize> {
    let token = root
        .token_at_offset(offset)
        .find(|t| t.kind() == solsp_syntax::SyntaxKind::IDENT)?;
    let name_ref = token.parent()?;
    solsp_hir::resolve::call_arity(&name_ref)
}

/// Find an imported top-level symbol `name` referenced in `root` (following re-exports
/// transitively): the target file URI and the byte range of the declaration's name.
fn cross_file_target(
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

/// The target-file export name a local `name` refers to under an import's binding, or
/// `None` if this import does not bind it. Namespace imports (`* as N`) are skipped —
/// `N.member` access needs member resolution (a later step).
fn exported_name(kind: &solsp_hir::imports::ImportKind, name: &str) -> Option<String> {
    use solsp_hir::imports::ImportKind;
    match kind {
        ImportKind::Glob => Some(name.to_string()),
        ImportKind::Named(list) => list
            .iter()
            .find(|n| n.local() == name)
            .map(|n| n.name.clone()),
        ImportKind::Namespace(_) => None,
    }
}

/// Resolve a member access `receiver.member` at `offset`: returns the target file URI
/// and the member's [`Definition`]. Handles a receiver that is a type name
/// (contract/library/interface/struct/enum) or a variable (following its declared
/// type), same-file or imported.
fn member_resolve(
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
    // `obj.method(args)` — pick the overload matching the call's argument count.
    let arity = solsp_hir::resolve::call_arity(&member_ref);

    // `N.member` where `N` is an `import * as N` namespace alias → the imported file's
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
    // `using L for T` — a library function attached to the receiver's type.
    using_member(state, uri, root, &receiver, &member, arity)
}

/// The file a `* as N` namespace import aliases, if `receiver` is that bare alias `N`.
fn namespace_target_uri(
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

/// Resolve `N.member` where `N` is a `* as N` namespace alias → the imported file's
/// top-level symbol (following re-exports).
fn namespace_member(
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
fn resolve_receiver_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    receiver_type(state, uri, root, receiver, false)
}

/// The type definition of an expression (structural, recursive). With `element`, the
/// array element type (for an indexed expression). Handles names, member access, calls
/// (→ the function's return type), indexing, and parentheses — so a chain like
/// `a.b().c[d].e` resolves segment by segment.
fn receiver_type(
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
            // `N.Type` where `N` is an `import * as N` namespace alias → the imported type.
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
            // `this` / `super` → the enclosing contract's type.
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

/// The result type of a call expression: the callee's return type, or — for a cast /
/// constructor `Type(x)` — the type itself.
fn call_result_type(
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
fn resolve_callee(
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
fn member_value_type(
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
fn member_name(member_expr: &solsp_syntax::SyntaxNode) -> Option<String> {
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
fn arg_count(call: &solsp_syntax::SyntaxNode) -> Option<usize> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, NAMED_ARG_LIST};
    let args = call
        .children()
        .find(|n| matches!(n.kind(), ARG_LIST | NAMED_ARG_LIST))?;
    Some(args.children().count())
}

/// The first `PARAM` of a function's return list (its second `PARAM_LIST`).
fn function_return_param(func: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{PARAM, PARAM_LIST};
    let returns = func.children().filter(|n| n.kind() == PARAM_LIST).nth(1)?;
    returns.children().find(|n| n.kind() == PARAM)
}

/// Resolve a receiver to a type def. With `element`, take the array element type
/// (the receiver was indexed). A bare type name resolves to itself; a variable follows
/// its declared type. Same-file lexical resolution first, then imported symbols.
fn resolve_value_type(
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
fn enclosing_contract(node: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    node.ancestors()
        .find(|n| n.kind() == solsp_syntax::SyntaxKind::CONTRACT_DEF)
}

/// Resolve a base contract name to its definition node and file — same-file first, then
/// an imported base.
fn resolve_base(
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
fn inherited_member(
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
fn inherited_base_member(
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
    // (uri, root, contract, is_base) — a base's `private` member is not accessible here.
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
            // a private base member — not accessible from here; keep searching.
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
fn inherited_name_at(
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
fn resolve_path_type(
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
fn resolve_type_by_name(
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
    //    cross-file bases) — these shadow file scope.
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

fn is_type_kind(kind: solsp_hir::resolve::DefKind) -> bool {
    use solsp_hir::resolve::DefKind::*;
    matches!(
        kind,
        Contract | Interface | Library | Struct | Enum | UserType
    )
}

/// Resolve a symbol `name` referenced in `root` to its declaration via the import graph,
/// following re-exports transitively to full depth (cycle-safe). A glob `import "X"`
/// re-exports everything `X` itself imports, so a symbol can sit several files away from
/// where it is used. Returns the file + [`Definition`].
fn cross_file_definition(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    let mut visited = std::collections::HashSet::new();
    cross_file_rec(state, uri, root, name, arity, &mut visited)
}

fn cross_file_rec(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
    // keyed by (file, name): the same file may be searched for different names across a
    // file's imports (e.g. a glob import probes it for `U`, an alias for `Utils`).
    visited: &mut std::collections::HashSet<(Url, String)>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    if !visited.insert((uri.clone(), name.to_string())) {
        return None; // already searched this file for this name (import cycle)
    }
    let _ = root; // imports now come from the cached index, not a fresh tree walk
    let index = state.file_index(uri)?;
    for imp in &index.imports {
        let Some(export) = exported_name(&imp.kind, name) else {
            continue;
        };
        let Some(target_uri) = imp.target.clone() else {
            continue;
        };
        let Some(tindex) = state.file_index(&target_uri) else {
            continue;
        };
        let troot = parse_root(state, &target_uri)?;
        // a top-level declaration in the imported file…
        if let Some(def) = solsp_hir::resolve::select_named(&tindex.defs, &export, arity, &troot) {
            return Some((target_uri, def));
        }
        // …or one the imported file itself re-exports (transitively).
        if let Some(found) = cross_file_rec(state, &target_uri, &troot, &export, arity, visited) {
            return Some(found);
        }
    }
    None
}

/// Look up `member` in a type, caching a contract's member list to avoid re-walking its
/// body and same-file C3 bases on every access (the dominant member-resolution cost on
/// big types). `type_uri` is the file `type_def` lives in. Only the common arity-free
/// contract lookup is cached; struct/enum and overload-by-arity take the direct path,
/// which preserves exact base-precedence semantics.
fn member_lookup(
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

/// The `NAME_REF` node of a receiver expression (`PATH_EXPR` → `NAME_REF`, or a bare
/// `NAME_REF`).
fn receiver_name_ref(receiver: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::NAME_REF;
    if receiver.kind() == NAME_REF {
        Some(receiver.clone())
    } else {
        receiver.children().find(|n| n.kind() == NAME_REF)
    }
}

/// Parse the current tree of a tracked file.
fn parse_root(state: &ServerState, uri: &Url) -> Option<solsp_syntax::SyntaxNode> {
    let file = state.file(uri)?;
    Some(solsp_base_db::parse(state.db(), file).syntax())
}

fn reference_target_at(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<RefTarget> {
    if let Some((turi, def)) = typed_overload_target(state, uri, root, offset) {
        return definition_target(state, turi, &def);
    }
    if let Some(def) = solsp_hir::resolve::definition_at(root, offset) {
        return Some(RefTarget {
            uri: uri.clone(),
            range: def_name_range(root, &def),
        });
    }
    if let Some((turi, def)) = member_resolve(state, uri, root, offset) {
        return definition_target(state, turi, &def);
    }
    if let Some((turi, def)) = inherited_name_at(state, uri, root, offset) {
        return definition_target(state, turi, &def);
    }
    let name = solsp_ide::navigation::name_at(root, offset)?;
    let arity = arity_at(root, offset);
    let (turi, def) = cross_file_definition(state, uri, root, &name, arity)?;
    definition_target(state, turi, &def)
}

fn definition_target(
    state: &ServerState,
    uri: Url,
    def: &solsp_hir::resolve::Definition,
) -> Option<RefTarget> {
    let root = parse_root(state, &uri)?;
    Some(RefTarget {
        range: def_name_range(&root, def),
        uri,
    })
}

/// The byte range of a definition's name identifier within `root`.
fn def_name_range(
    root: &solsp_syntax::SyntaxNode,
    def: &solsp_hir::resolve::Definition,
) -> rowan::TextRange {
    use solsp_syntax::SyntaxKind::IDENT;
    let name_node = def.name_ptr.to_node(root);
    name_node
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == IDENT)
        .map(|t| t.text_range())
        .unwrap_or_else(|| name_node.text_range())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unused_import_respects_forge_lint_disable_next_line() {
        let uri = Url::parse("file:///Base.sol").unwrap();
        let src = "/// forge-lint: disable-next-line(unused-import)\n\
                   import { TransferHelper } from \"./Helper.sol\";\n\
                   import { Other } from \"./Helper.sol\";\n\
                   contract Base {}\n";
        let mut state = ServerState::default();
        state.set(&uri, src.to_string());
        let file = state.file(&uri).unwrap();
        let root = solsp_base_db::parse(state.db(), file).syntax();
        let li = state.line_index(&uri).unwrap();

        let diags = import_diagnostics::unused_import_diagnostics(&state, &uri, &root, li, None);
        let messages: Vec<_> = diags.iter().map(|diag| diag.message.as_str()).collect();
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("TransferHelper")),
            "{messages:?}"
        );
        assert!(
            messages.iter().any(|message| message.contains("Other")),
            "{messages:?}"
        );
    }
}
