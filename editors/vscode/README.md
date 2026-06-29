# SolSP

SolSP provides Solidity language support for Visual Studio Code using the
bundled solsp language server.

## Features

- Syntax diagnostics for Solidity files.
- Document outline and breadcrumbs for contracts, functions, structs, events,
  and related Solidity declarations.
- Semantic highlighting for Solidity symbols.
- Hover, go-to-definition, completion, signature help, and selected semantic
  diagnostics.

## Server

The extension includes a platform-specific `solsp-server` binary. Advanced users
can override it with the `solsp.server.path` setting.

## Settings

| Setting              | Default | Description                                 |
| -------------------- | ------- | ------------------------------------------- |
| `solsp.server.path`  | `""`    | Absolute path to a custom server binary.    |
| `solsp.trace.server` | `off`   | Trace JSON-RPC traffic for troubleshooting. |
