//! `solsp-server` binary — a thin stdio shim around the [`solsp_server`] library.
//! The editor speaks JSON-RPC over our stdin/stdout; all protocol logic lives in the
//! library so it can be driven by integration tests over an in-memory transport.

use anyhow::Result;
use lsp_server::Connection;

fn main() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();

    let capabilities = serde_json::to_value(solsp_server::server_capabilities())?;
    let _init_params = connection.initialize(capabilities)?;
    solsp_server::run(&connection)?;

    // Drop the connection before joining: its `sender` feeds the stdio writer
    // thread, which only finishes once every sender clone is gone. Without this,
    // `io_threads.join()` would block forever after shutdown (a zombie server).
    drop(connection);
    io_threads.join()?;
    Ok(())
}
