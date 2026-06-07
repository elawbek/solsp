//! `solsp-server` — the LSP binary. Owns the main loop (rust-analyzer model) over
//! `lsp-server` stdio, maps `solsp-ide` data into LSP types. Document state in M1 is
//! a plain `DashMap<Url, Document>` reparsed in full on change; salsa + incremental
//! sync arrive in M2 (design §5).

use anyhow::Result;
use lsp_server::{Connection, Message};
use lsp_types::{OneOf, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind};

mod state;

fn main() -> Result<()> {
    // stdio transport: the editor speaks JSON-RPC over our stdin/stdout.
    let (connection, io_threads) = Connection::stdio();

    let capabilities = serde_json::to_value(ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        // TODO(M1 §5): advertise semanticTokensProvider with a legend matching
        // solsp_ide::semantic_tokens::TokenType.
        ..Default::default()
    })?;

    let _init_params = connection.initialize(capabilities)?;
    main_loop(&connection)?;
    io_threads.join()?;
    Ok(())
}

fn main_loop(connection: &Connection) -> Result<()> {
    let mut _state = state::ServerState::default();
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                // TODO(M1 §5): dispatch by req.method:
                //   "textDocument/documentSymbol" -> ide::document_symbols -> reply
                //   "textDocument/semanticTokens/full" -> ide::semantic_tokens -> reply
            }
            Message::Notification(_not) => {
                // TODO(M1 §5): by not.method:
                //   "textDocument/didOpen" / "didChange" -> update Document, reparse,
                //   publish "textDocument/publishDiagnostics".
            }
            Message::Response(_resp) => {}
        }
    }
    Ok(())
}
