// Thin VS Code client for solsp. All language intelligence lives in the
// `solsp-server` binary; this extension only launches it over stdio and wires the
// `solidity` document selector to it (design §5). Run locally via the Extension
// Development Host (F5) — no Marketplace publish in M1.

import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const command = resolveServerPath(context.extensionPath);
  const serverOptions: ServerOptions = {
    run: { command, transport: TransportKind.stdio },
    debug: { command, transport: TransportKind.stdio },
  };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "solidity" }],
  };

  // The client id `solsp` also keys the `solsp.trace.server` setting, so the
  // built-in JSON-RPC trace channel just works.
  client = new LanguageClient(
    "solsp",
    "solsp Solidity Language Server",
    serverOptions,
    clientOptions,
  );

  // `start()` launches the server and registers the providers; stop it on unload.
  client.start();
  context.subscriptions.push({
    dispose: () => {
      void client?.stop();
    },
  });
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}

/// Locate the `solsp-server` binary: an explicit setting wins; otherwise probe each
/// open workspace folder's Cargo output AND the repo's `target/` relative to this
/// extension (`editors/vscode` → `../../target`), so F5 works no matter which folder
/// the Extension Development Host opened. Finally fall back to the bare name on `PATH`.
function resolveServerPath(extensionPath: string): string {
  const configured = vscode.workspace
    .getConfiguration("solsp")
    .get<string>("server.path");
  if (configured && configured.trim().length > 0) {
    return configured.trim();
  }

  const exe = process.platform === "win32" ? "solsp-server.exe" : "solsp-server";

  // Roots to probe for `target/{debug,release}/<exe>`: open folders first, then the
  // repo root inferred from the extension's own location.
  const roots = [
    ...(vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath),
    path.resolve(extensionPath, "..", ".."),
  ];
  for (const root of roots) {
    for (const profile of ["debug", "release"]) {
      const candidate = path.join(root, "target", profile, exe);
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return exe;
}
