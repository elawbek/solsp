# solsp

Language server for Solidity, written from scratch in Rust in the
rust-analyzer style: a hand-written parser produces a lossless CST (rowan),
analysis is layered over the tree and a salsa-backed source database, and the
server speaks LSP over stdio.

Design and roadmap live in `docs/superpowers/specs/` (gitignored — local only).

## Layout

```
crates/
  syntax/   solsp-syntax  — lexer -> parser -> CST (rowan) -> typed AST (pure)
  base-db/  solsp-base-db — salsa inputs and tracked parse query
  hir/      solsp-hir     — item model, imports, scopes, name resolution
  ide/      solsp-ide     — diagnostics, document symbols, semantic tokens, navigation
  server/   solsp-server  — LSP over stdio, project state, cross-file features
editors/
  vscode/   solsp-vscode  — VS Code client that launches solsp-server
```

`crates/server` is split into small protocol/feature helpers around the main LSP
loop:

- `capabilities`, `protocol`, `to_proto` — LSP wire-level helpers
- `state` — tracked document store, salsa database, import loading
- `builtins`, `completion_items`, `named_args`, `using_for`, `import_surface` —
  completion/navigation support code
- `diagnostics`, `typecheck`, `syntax_utils` — diagnostics plumbing and shared
  semantic helpers

## Status

Prototype language server with:

- syntax diagnostics, document symbols, semantic tokens
- go-to-definition and hover for same-file and imported symbols
- scope and member completion, including selected Solidity builtins
- signature help and overload selection for common call forms
- cross-file imports, remappings, namespace imports, re-exports, and inheritance
- conservative semantic diagnostics for undefined names, type mismatches, returns,
  casts, comparisons, unreachable code, mutability, and unused imports/locals

The implementation is still intentionally conservative: unknown or unmodeled types
are generally skipped rather than reported, to avoid noisy false positives.

## Build

```sh
cargo build            # builds the workspace (syntax, ide, server)
cargo test             # parser snapshots, ide unit tests, server integration
```

## Development checks

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
npm run compile --prefix editors/vscode
```
