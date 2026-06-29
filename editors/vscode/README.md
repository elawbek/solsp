# solsp — VS Code extension

A thin VS Code client for the [`solsp`](../../) Solidity language server. All the
language intelligence (syntax diagnostics, document outline, semantic highlighting)
lives in the `solsp-server` binary; this extension only launches it over stdio and
points the `solidity` language at it.

The extension starts a bundled or configured `solsp-server` binary and connects
to it over stdio.

## Build

```sh
# 1. build the language server (from the repo root)
cargo build -p solsp-server

# 2. install + compile the extension (from this folder)
cd editors/vscode
npm install
npm run compile
```

## Package a platform VSIX

Build a release server for the matching Rust target, then package the VS Code
extension for the matching VS Code target:

```sh
# from repo root
cargo build --release --target x86_64-unknown-linux-gnu
npm run package:platform --prefix editors/vscode -- --target linux-x64
```

The packaging script stages exactly one server binary under `server/` before it
runs `vsce package --target`. Pass `--build` to let the script run the matching
`cargo build --release --target ...` first.

## Publish without a token

The Marketplace publisher page can upload the platform VSIX files manually. Use
the GitHub Actions `Build VS Code VSIX` workflow to build all platform artifacts,
download them, then upload each `.vsix` under the same extension version via:

```text
New extension -> Visual Studio Code
```

Upload all generated packages:

```text
solsp-vscode-linux-x64.vsix
solsp-vscode-linux-arm64.vsix
solsp-vscode-win32-x64.vsix
solsp-vscode-win32-arm64.vsix
solsp-vscode-darwin-x64.vsix
solsp-vscode-darwin-arm64.vsix
```

Do not upload a non-targeted universal VSIX when publishing platform packages.
Each file must be created by `vsce package --target ...`, which is what
`npm run package:platform` and the GitHub Actions workflow do.

## Run (Extension Development Host)

1. Open **this folder** (`editors/vscode/`) in VS Code.
2. Press **F5** ("Run solsp Extension"). A second VS Code window opens — the
   Extension Development Host with the extension loaded.
3. In that window, open the **solsp repo root** (or any folder whose
   `target/debug/solsp-server` exists), then open a `.sol` file.

You should see:

- **Diagnostics** — red squiggles on syntax errors (live as you type).
- **Outline** — the contract/function/struct tree in the Outline view and
  breadcrumbs.
- **Semantic highlighting** — identifiers colored by role (type, function,
  parameter, …) on top of your theme.

## Server binary resolution

The extension finds `solsp-server` in this order:

1. The `solsp.server.path` setting, if set (absolute path).
2. Each workspace folder's `target/debug/solsp-server`, then
   `target/release/solsp-server`.
3. A bundled `server/solsp-server` or `server/solsp-server.exe` next to the
   installed extension.
4. The bare name `solsp-server` on your `PATH`.

## Settings

| Setting              | Default | Description                                            |
| -------------------- | ------- | ------------------------------------------------------ |
| `solsp.server.path`  | `""`    | Absolute path to the `solsp-server` binary.            |
| `solsp.trace.server` | `off`   | Trace JSON-RPC traffic (`off` / `messages` / `verbose`). |
