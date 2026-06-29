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

## Install from a VSIX

Choose the VSIX that matches your platform:

```text
solsp-vscode-linux-x64.vsix
solsp-vscode-linux-arm64.vsix
solsp-vscode-win32-x64.vsix
solsp-vscode-win32-arm64.vsix
solsp-vscode-darwin-x64.vsix
solsp-vscode-darwin-arm64.vsix
```

Install it from the command line:

```sh
code --install-extension path/to/solsp-vscode-linux-x64.vsix --force
```

Or install it from Visual Studio Code:

```text
Extensions -> ... -> Install from VSIX...
```

Reload the VS Code window after installation, then open a folder containing
Solidity files.

## Server

The extension includes a platform-specific `solsp-server` binary. Advanced users
can override it with the `solsp.server.path` setting.

## Settings

| Setting              | Default | Description                                 |
| -------------------- | ------- | ------------------------------------------- |
| `solsp.server.path`  | `""`    | Absolute path to a custom server binary.    |
| `solsp.trace.server` | `off`   | Trace JSON-RPC traffic for troubleshooting. |
