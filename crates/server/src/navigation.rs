//! Document navigation and token requests.

use lsp_types::{
    DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse,
    Location, SemanticTokensParams, SemanticTokensResult,
};

use super::*;

/// `textDocument/documentSymbol` -> nested outline (empty if the doc is not open).
pub(super) fn document_symbols(
    state: &ServerState,
    params: DocumentSymbolParams,
) -> DocumentSymbolResponse {
    let uri = params.text_document.uri;
    let symbols = match (state.file(&uri), state.line_index(&uri)) {
        (Some(file), Some(li)) => {
            let root = solsp_base_db::parse(state.db(), file).syntax();
            let bare = solsp_ide::document_symbols::document_symbols(&root);
            to_proto::document_symbols(&bare, li)
        }
        _ => Vec::new(),
    };
    DocumentSymbolResponse::Nested(symbols)
}

/// `textDocument/semanticTokens/full` -> delta-encoded tokens, or `None` if unopened.
pub(super) fn semantic_tokens(
    state: &ServerState,
    params: SemanticTokensParams,
) -> Option<SemanticTokensResult> {
    let uri = params.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let parse = solsp_base_db::parse(state.db(), file);
    let bare = solsp_ide::semantic_tokens::semantic_tokens(&parse.syntax());
    let tokens = to_proto::semantic_tokens(&bare, file.text(state.db()), li);
    Some(SemanticTokensResult::Tokens(tokens))
}

/// `textDocument/definition` -> the declaration's name range.
pub(super) fn goto_definition(
    state: &ServerState,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let pos = params.text_document_position_params;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();

    // Overloaded calls where argument types pick the overload must run before
    // same-file arity-only resolution.
    if let Some((turi, def)) = typed_overload_target(state, &uri, &root, offset) {
        let troot = parse_root(state, &turi)?;
        let tli = state.line_index(&turi)?;
        let range = to_proto::range(tli, def_name_range(&troot, &def));
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: turi,
            range,
        }));
    }

    if let Some(target) = solsp_ide::navigation::goto_definition(&root, offset) {
        let range = to_proto::range(li, target);
        return Some(GotoDefinitionResponse::Scalar(Location { uri, range }));
    }

    if let Some((target_uri, def)) = member_resolve(state, &uri, &root, offset) {
        let troot = parse_root(state, &target_uri)?;
        let tli = state.line_index(&target_uri)?;
        let range = to_proto::range(tli, def_name_range(&troot, &def));
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range,
        }));
    }

    if let Some((target_uri, def)) = inherited_name_at(state, &uri, &root, offset) {
        let troot = parse_root(state, &target_uri)?;
        let tli = state.line_index(&target_uri)?;
        let range = to_proto::range(tli, def_name_range(&troot, &def));
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range,
        }));
    }

    if let Some(name) = solsp_ide::navigation::name_at(&root, offset) {
        let arity = arity_at(&root, offset);
        if let Some((target_uri, range)) = cross_file_target(state, &uri, &root, &name, arity) {
            let tli = state.line_index(&target_uri)?;
            return Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range: to_proto::range(tli, range),
            }));
        }
    }

    let imp = solsp_hir::imports::import_at(&root, offset)?;
    let target_uri = state::resolve_import_uri(&uri, &imp.path)?;
    state.file(&target_uri)?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: target_uri,
        range: lsp_types::Range::default(),
    }))
}
