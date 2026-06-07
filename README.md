# solsp

Language server for Solidity, written from scratch in Rust in the
rust-analyzer style: a hand-written parser produces a lossless CST (rowan),
features are pure functions over the tree, and the server speaks LSP over stdio.

Design and roadmap live in `docs/superpowers/specs/` (gitignored — local only).

## Layout

```
crates/
  syntax/   solsp-syntax  — lexer -> parser -> CST (rowan) -> typed AST   (pure)
  ide/      solsp-ide     — diagnostics, document symbols, semantic tokens
  server/   solsp-server  — LSP over stdio (the binary)
editors/
  zed/      solsp-zed     — Zed extension (wasm; built separately)
```

## Status

M1 (MVP) in progress: syntax diagnostics + document symbols + semantic tokens.
No name resolution yet (M2), no types/completion yet (M3). See the design doc.

## Build

```sh
cargo build            # builds the workspace (syntax, ide, server)
cargo test             # parser snapshots, ide unit tests, server integration
```
