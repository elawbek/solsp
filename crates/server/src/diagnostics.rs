//! LSP diagnostics publishing.

use anyhow::Result;
use lsp_server::{Connection, Message, Notification};
use lsp_types::notification::{Notification as _, PublishDiagnostics};
use lsp_types::{PublishDiagnosticsParams, Url};

use crate::state::ServerState;
use crate::to_proto;

/// Compute and publish diagnostics for a document (empty list if missing). The semantic
/// type-check (slow, cross-file) runs only when `semantic` is set — on open/save and the
/// background sweep, not on every keystroke. `budget` bounds the type-check for the
/// background sweep; an open/save pass passes `None` and runs to completion.
pub(super) fn publish_diagnostics(
    connection: &Connection,
    state: &ServerState,
    uri: &Url,
    semantic: bool,
    budget: Option<std::time::Duration>,
) -> Result<()> {
    let diagnostics = match (state.file(uri), state.line_index(uri)) {
        (Some(file), Some(li)) => {
            let parse = solsp_base_db::parse(state.db(), file);
            let mut diags =
                to_proto::diagnostics(&solsp_ide::diagnostics::diagnostics(parse.errors()), li);
            // Semantic checks only on a syntactically clean file (a broken tree mid-edit is
            // noise). A shared deadline bounds the whole semantic pass on the background
            // sweep; an open/save pass passes `None` and runs to completion.
            if semantic && parse.errors().is_empty() {
                let deadline = budget.map(|b| std::time::Instant::now() + b);
                let root = parse.syntax();
                diags.extend(super::undefined_name_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::type_check_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::assignment_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::return_type_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::cast_diagnostics(state, uri, &root, li, deadline));
                diags.extend(super::binary_op_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::comparison_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::condition_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::unreachable_diagnostics(&root, li, deadline));
                diags.extend(super::mutability_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::abstract_contract_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::invalid_import_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::unused_import_diagnostics(
                    state, uri, &root, li, deadline,
                ));
                diags.extend(super::unused_local_diagnostics(&root, li, deadline));
            }
            diags
        }
        _ => Vec::new(),
    };
    send_diagnostics(connection, uri.clone(), diagnostics)
}

/// Send a `textDocument/publishDiagnostics` notification.
pub(super) fn send_diagnostics(
    connection: &Connection,
    uri: Url,
    diagnostics: Vec<lsp_types::Diagnostic>,
) -> Result<()> {
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: None,
    };
    let not = Notification::new(PublishDiagnostics::METHOD.to_string(), params);
    connection.sender.send(Message::Notification(not))?;
    Ok(())
}
