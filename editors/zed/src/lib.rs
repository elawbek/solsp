//! Zed extension: registers `solsp` as the language server for Solidity. Compiled
//! to wasm and loaded by Zed; the thin job here is to tell Zed how to launch the
//! `solsp-server` binary (design §5). Install locally via Zed's "install dev
//! extension" pointing at this folder.

use zed_extension_api::{self as zed, Result};

struct SolspExtension;

impl zed::Extension for SolspExtension {
    fn new() -> Self {
        SolspExtension
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        // TODO(M1 §5): also support a bundled/downloaded binary. For dev, require
        // `solsp-server` on PATH (e.g. `cargo install --path crates/server`).
        let command = worktree
            .which("solsp-server")
            .ok_or_else(|| "`solsp-server` not found in PATH".to_string())?;
        Ok(zed::Command {
            command,
            args: vec![],
            env: vec![],
        })
    }
}

zed::register_extension!(SolspExtension);
