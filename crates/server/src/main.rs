//! `solsp-server` binary — a thin stdio shim around the [`solsp_server`] library.
//! The editor speaks JSON-RPC over our stdin/stdout; all protocol logic lives in the
//! library so it can be driven by integration tests over an in-memory transport.

use anyhow::Result;
use lsp_server::Connection;

fn main() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();

    let capabilities = serde_json::to_value(solsp_server::server_capabilities())?;
    let init_params = connection.initialize(capabilities)?;
    // The editor's workspace root, so the whole project can be pre-parsed up front.
    let workspace_root = serde_json::from_value::<lsp_types::InitializeParams>(init_params)
        .ok()
        .and_then(|p| {
            p.workspace_folders
                .and_then(|folders| folders.into_iter().next())
                .and_then(|f| f.uri.to_file_path().ok())
                .or_else(|| {
                    #[allow(deprecated)]
                    p.root_uri.and_then(|u| u.to_file_path().ok())
                })
        });
    solsp_server::run_with_root(&connection, workspace_root)?;

    // Drop the connection before joining: its `sender` feeds the stdio writer
    // thread, which only finishes once every sender clone is gone. Without this,
    // `io_threads.join()` would block forever after shutdown (a zombie server).
    drop(connection);
    io_threads.join()?;
    Ok(())
}
