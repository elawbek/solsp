# solsp — VS Code extension

A thin VS Code client for the [`solsp`](../../) Solidity language server. All the
language intelligence (syntax diagnostics, document outline, semantic highlighting)
lives in the `solsp-server` binary; this extension only launches it over stdio and
points the `solidity` language at it.

This is an M1 development extension — run it from source via the Extension
Development Host. It is not published to the Marketplace.

## Build

```sh
# 1. build the language server (from the repo root)
cargo build -p solsp-server

# 2. install + compile the extension (from this folder)
cd editors/vscode
npm install
npm run compile
```

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
3. The bare name `solsp-server` on your `PATH`.

## Settings

| Setting              | Default | Description                                            |
| -------------------- | ------- | ------------------------------------------------------ |
| `solsp.server.path`  | `""`    | Absolute path to the `solsp-server` binary.            |
| `solsp.trace.server` | `off`   | Trace JSON-RPC traffic (`off` / `messages` / `verbose`). |
