// Thin VS Code client for solsp. All language intelligence lives in the
// `solsp-server` binary; this extension only launches it over stdio and wires the
// `solidity` document selector to it.

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
  context.subscriptions.push(
    vscode.commands.registerCommand("solsp.showReferences", showReferences),
  );
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

type ShowReferencesPayload = {
  uri: string;
  position: { line: number; character: number };
  locations: Array<{
    uri: string;
    range: {
      start: { line: number; character: number };
      end: { line: number; character: number };
    };
  }>;
};

async function showReferences(payload: ShowReferencesPayload): Promise<void> {
  const uri = vscode.Uri.parse(payload.uri);
  const position = new vscode.Position(
    payload.position.line,
    payload.position.character,
  );
  const locations = payload.locations.map(
    (loc) =>
      new vscode.Location(
        vscode.Uri.parse(loc.uri),
        new vscode.Range(
          loc.range.start.line,
          loc.range.start.character,
          loc.range.end.line,
          loc.range.end.character,
        ),
      ),
  );
  await vscode.commands.executeCommand(
    "editor.action.showReferences",
    uri,
    position,
    locations,
  );
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}

/// Locate the `solsp-server` binary: an explicit setting wins; Extension Development
/// Host can pin `SOLSP_SERVER_PATH`; otherwise use a bundled binary from the installed
/// extension. Dev builds then probe Cargo outputs so F5 works no matter which folder
/// the Extension Development Host opened. Finally fall back to the bare name on `PATH`.
function resolveServerPath(extensionPath: string): string {
  const configured = vscode.workspace
    .getConfiguration("solsp")
    .get<string>("server.path");
  if (configured && configured.trim().length > 0) {
    return configured.trim();
  }

  const envServerPath = process.env.SOLSP_SERVER_PATH;
  if (envServerPath && envServerPath.trim().length > 0) {
    return envServerPath.trim();
  }

  const exe = process.platform === "win32" ? "solsp-server.exe" : "solsp-server";
  const bundled = path.join(extensionPath, "server", exe);
  if (fs.existsSync(bundled)) {
    return bundled;
  }

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
