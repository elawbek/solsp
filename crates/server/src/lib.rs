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
mod call_resolution;
mod capabilities;
mod code_actions;
mod completion_items;
mod contract_diagnostics;
mod diagnostics;
mod flow_diagnostics;
mod graphs;
mod import_diagnostics;
mod import_resolution;
mod import_surface;
mod inheritance;
mod interaction;
mod lsp_loop;
mod member_resolution;
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
mod type_inference;
pub mod typecheck;
mod usage_diagnostics;
mod using_for;

pub use capabilities::server_capabilities;
pub use lsp_loop::{run, run_with_root};

use builtins::{
    builtin_items, builtin_member_items, is_builtin_name, is_fixed_bytes, is_integer_type_name,
    synthetic_members, yul_builtin, yul_builtin_items,
};
use call_resolution::{
    callee_display_name, is_cheatcode_receiver, resolve_named_callee, signature_candidates,
    typed_overload_target,
};
use completion_items::completion_items_from;
use contract_diagnostics::{
    declaration_name, declaration_name_range, function_arity, function_has_override,
    function_label, function_name, function_name_range, function_visibility, member_visibility,
};
use import_resolution::{cross_file_definition, cross_file_target};
use import_surface::{
    collect_file_exports, import_path_items, imported_symbols, namespace_alias_items,
};
use inheritance::{collect_base_members, collect_inherited_members, is_subtype};
use member_resolution::{
    arg_count, dotted_receiver, enclosing_contract, inherited_member, inherited_name_at,
    is_instance_receiver, is_library_node, is_storage_decl, is_super_receiver, is_type_kind,
    member_lookup, member_name, member_resolve, namespace_target_uri, receiver_decl,
    receiver_value_info, resolve_base, resolve_callee, resolve_receiver_def,
    resolve_receiver_def_target, resolve_receiver_type, resolve_type_by_name, type_expr_members,
    value_type_builtin_members,
};
use named_args::{named_arg_completion, named_arg_fields, named_arg_hover};
use protocol::markup_hover;
use references::{has_reference_count_at_least, RefTarget};
use state::ServerState;
use syntax_utils::{
    arity_at, indexed_type_text, named_type, nameref_text, node_ident, node_type_text,
    param_name_types, type_text,
};
use type_inference::{arg_text, infer_arg_ty, ty_label, type_mismatch};
use using_for::{using_member, using_member_items};

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
