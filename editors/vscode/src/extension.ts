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
let graphOutput: vscode.OutputChannel | undefined;

export function activate(context: vscode.ExtensionContext): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("solsp.showReferences", showReferences),
    vscode.commands.registerCommand("solsp.showInheritanceGraph", showInheritanceGraph),
    vscode.commands.registerCommand("solsp.showFunctionCallGraph", showFunctionCallGraph),
  );

  const command = resolveServerPath(context.extensionPath, context.extensionMode);
  const serverOptions: ServerOptions = {
    run: { command, transport: TransportKind.stdio },
    debug: { command, transport: TransportKind.stdio },
  };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "solidity" }],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher("**/*.sol"),
    },
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

type InheritanceGraphResponse = {
  mermaid: string;
  nodes: GraphNode[];
  edges: GraphEdge[];
};

type FunctionCallGraphResponse = InheritanceGraphResponse;

type GraphNode = {
  id: string;
  name: string;
  uri: string;
  focus?: boolean;
};

type GraphEdge = {
  from: string;
  to: string;
  kind: string;
};

type GraphCommandPayload = {
  uri: string;
  position: { line: number; character: number };
};

async function showInheritanceGraph(payload?: GraphCommandPayload): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!payload && (!editor || editor.document.languageId !== "solidity")) {
    await vscode.window.showWarningMessage("Open a Solidity file first.");
    return;
  }
  if (!client) {
    await vscode.window.showWarningMessage("SolSP language client is not running.");
    return;
  }

  const graph = await client.sendRequest<InheritanceGraphResponse>(
    "solsp/inheritanceGraph",
    {
      textDocument: { uri: payload?.uri ?? editor?.document.uri.toString() },
      position: payload?.position ?? {
        line: editor?.selection.active.line,
        character: editor?.selection.active.character,
      },
    },
  );
  graphOutput ??= vscode.window.createOutputChannel("SolSP Inheritance Graph");
  graphOutput.clear();
  graphOutput.appendLine(graph.mermaid);
  graphOutput.appendLine("");
  graphOutput.appendLine(`nodes: ${graph.nodes.length}, edges: ${graph.edges.length}`);

  const panel = vscode.window.createWebviewPanel(
    "solspInheritanceGraph",
    "SolSP Inheritance Graph",
    vscode.ViewColumn.Beside,
    { enableScripts: false },
  );
  panel.webview.html = renderDependencyGraphHtml(
    graph,
    "Inheritance Graph",
    "Inheritance Pairs",
    true,
  );
}

async function showFunctionCallGraph(payload?: GraphCommandPayload): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!payload && (!editor || editor.document.languageId !== "solidity")) {
    await vscode.window.showWarningMessage("Open a Solidity file first.");
    return;
  }
  if (!client) {
    await vscode.window.showWarningMessage("SolSP language client is not running.");
    return;
  }

  const graph = await client.sendRequest<FunctionCallGraphResponse>(
    "solsp/functionCallGraph",
    {
      textDocument: { uri: payload?.uri ?? editor?.document.uri.toString() },
      position: payload?.position ?? {
        line: editor?.selection.active.line,
        character: editor?.selection.active.character,
      },
    },
  );

  const panel = vscode.window.createWebviewPanel(
    "solspFunctionCallGraph",
    "SolSP Function Call Graph",
    vscode.ViewColumn.Beside,
    { enableScripts: false },
  );
  panel.webview.html = renderDependencyGraphHtml(
    graph,
    "Function Call Graph",
    "Call Pairs",
    false,
  );
}

function renderDependencyGraphHtml(
  graph: InheritanceGraphResponse,
  title: string,
  pairsTitle: string,
  reversePairLabels: boolean,
): string {
  const layout = layoutDependencyGraph(graph.nodes, graph.edges, reversePairLabels);
  const edges = layout.edges
    .map(
      (edge) =>
        `<path class="edge" d="M ${edge.x1} ${edge.y1} L ${edge.x1} ${edge.midY} L ${edge.x2} ${edge.midY} L ${edge.x2} ${edge.y2}" marker-end="url(#arrow)" />`,
    )
    .join("");
  const nodes = layout.nodes
    .map(
      (node) => `<g>
        <title>${escapeHtml(node.name)} · ${escapeHtml(shortUri(node.uri))}</title>
        <rect class="node ${node.focus ? "focus" : ""}" x="${node.x}" y="${node.y}" width="${layout.nodeWidth}" height="${layout.nodeHeight}" rx="6" />
        <text class="node-title" x="${node.x + layout.nodeWidth / 2}" y="${node.y + 31}" text-anchor="middle">${escapeHtml(node.name)}</text>
      </g>`,
    )
    .join("");
  const pairs = graph.edges
    .map((edge) => {
      const from = graph.nodes.find((node) => node.id === edge.from);
      const to = graph.nodes.find((node) => node.id === edge.to);
      if (!from || !to) {
        return "";
      }
      const left = reversePairLabels ? to : from;
      const right = reversePairLabels ? from : to;
      return `<li><span>${escapeHtml(left.name)}</span><span class="arrow">→</span><span>${escapeHtml(right.name)}</span></li>`;
    })
    .filter(Boolean)
    .join("");

  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <style>
    body {
      margin: 0;
      padding: 24px;
      color: var(--vscode-foreground);
      background: var(--vscode-editor-background);
      font-family: var(--vscode-font-family);
    }
    .toolbar {
      display: flex;
      align-items: baseline;
      gap: 12px;
      margin-bottom: 16px;
    }
    h1 {
      margin: 0;
      font-size: 18px;
      font-weight: 600;
    }
    .meta {
      color: var(--vscode-descriptionForeground);
      font-size: 12px;
    }
    .canvas {
      overflow: auto;
      border: 1px solid var(--vscode-panel-border);
      background: var(--vscode-editor-background);
    }
    svg {
      display: block;
      min-width: 100%;
    }
    .node {
      fill: color-mix(in srgb, var(--vscode-editorWidget-background) 88%, var(--vscode-foreground) 12%);
      stroke: var(--vscode-panel-border);
      stroke-width: 1.4;
    }
    .node.focus {
      stroke: var(--vscode-focusBorder);
      stroke-width: 2.6;
    }
    .node-title {
      fill: var(--vscode-foreground);
      font-size: 15px;
      font-weight: 600;
    }
    .edge {
      fill: none;
      stroke: var(--vscode-descriptionForeground);
      stroke-width: 1.8;
      stroke-linejoin: round;
      stroke-linecap: round;
    }
    .pairs {
      margin-top: 18px;
      max-width: 720px;
    }
    .pairs h2 {
      margin: 0 0 8px;
      font-size: 13px;
      font-weight: 600;
      color: var(--vscode-descriptionForeground);
    }
    .pairs ul {
      list-style: none;
      margin: 0;
      padding: 0;
      display: grid;
      gap: 6px;
    }
    .pairs li {
      display: flex;
      align-items: center;
      gap: 8px;
      padding: 7px 10px;
      border: 1px solid var(--vscode-panel-border);
      background: var(--vscode-editorWidget-background);
      border-radius: 4px;
      font-size: 13px;
    }
    .pairs .arrow {
      color: var(--vscode-descriptionForeground);
    }
  </style>
</head>
<body>
  <div class="toolbar">
    <h1>${escapeHtml(title)}</h1>
    <span class="meta">${graph.nodes.length} nodes, ${graph.edges.length} edges</span>
  </div>
  <div class="canvas">
    <svg width="${layout.width}" height="${layout.height}" viewBox="0 0 ${layout.width} ${layout.height}" role="img">
      <defs>
        <marker id="arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
          <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--vscode-descriptionForeground)" />
        </marker>
      </defs>
      ${edges}
      ${nodes}
    </svg>
  </div>
  <section class="pairs">
    <h2>${escapeHtml(pairsTitle)}</h2>
    <ul>${pairs}</ul>
  </section>
</body>
</html>`;
}

function layoutDependencyGraph(
  nodes: GraphNode[],
  edges: GraphEdge[],
  reverseEdgesForLevels: boolean,
) {
  const nodeWidth = 230;
  const nodeHeight = 52;
  const xGap = 72;
  const yGap = 128;
  const margin = 42;
  const nodeById = new Map(nodes.map((node) => [node.id, node]));
  const depsByNode = new Map<string, string[]>();

  for (const edge of edges) {
    if (!nodeById.has(edge.from) || !nodeById.has(edge.to)) {
      continue;
    }
    const key = reverseEdgesForLevels ? edge.from : edge.to;
    const dep = reverseEdgesForLevels ? edge.to : edge.from;
    const deps = depsByNode.get(key) ?? [];
    deps.push(dep);
    depsByNode.set(key, deps);
  }

  const levelMemo = new Map<string, number>();
  const levelOf = (id: string, stack: Set<string>): number => {
    const cached = levelMemo.get(id);
    if (cached !== undefined) {
      return cached;
    }
    if (stack.has(id)) {
      return 0;
    }
    stack.add(id);
    const bases = depsByNode.get(id) ?? [];
    const level =
      bases.length === 0
        ? 0
        : Math.max(...bases.map((base) => levelOf(base, stack))) + 1;
    stack.delete(id);
    levelMemo.set(id, level);
    return level;
  };

  for (const node of nodes) {
    levelOf(node.id, new Set());
  }

  const levels = new Map<number, GraphNode[]>();
  for (const node of nodes) {
    const level = levelMemo.get(node.id) ?? 0;
    const bucket = levels.get(level) ?? [];
    bucket.push(node);
    levels.set(level, bucket);
  }
  for (const bucket of levels.values()) {
    bucket.sort((a, b) => a.name.localeCompare(b.name));
  }

  const positions = new Map<string, GraphNode & { x: number; y: number }>();
  let width = 360;
  let height = margin * 2;
  for (const [level, bucket] of levels.entries()) {
    const rowWidth =
      margin * 2 + bucket.length * nodeWidth + Math.max(0, bucket.length - 1) * xGap;
    width = Math.max(width, rowWidth);
    height = Math.max(height, margin * 2 + (level + 1) * nodeHeight + level * yGap);
  }

  for (const [level, bucket] of levels.entries()) {
    const rowWidth = bucket.length * nodeWidth + Math.max(0, bucket.length - 1) * xGap;
    let x = Math.max(margin, (width - rowWidth) / 2);
    const y = margin + level * (nodeHeight + yGap);
    for (const node of bucket) {
      positions.set(node.id, { ...node, x, y });
      x += nodeWidth + xGap;
    }
  }

  const laidOutEdges = edges.flatMap((edge) => {
    const derived = positions.get(edge.from);
    const base = positions.get(edge.to);
    if (!derived || !base) {
      return [];
    }
    return [
      {
        x1: base.x + nodeWidth / 2,
        y1: base.y + nodeHeight,
        x2: derived.x + nodeWidth / 2,
        y2: derived.y,
        midY: base.y + nodeHeight + (derived.y - base.y - nodeHeight) / 2,
      },
    ];
  });

  return {
    width,
    height,
    nodeWidth,
    nodeHeight,
    nodes: [...positions.values()],
    edges: laidOutEdges,
  };
}

function shortUri(uri: string): string {
  const parsed = vscode.Uri.parse(uri);
  return path.basename(parsed.fsPath || parsed.path);
}

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
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
function resolveServerPath(
  extensionPath: string,
  extensionMode: vscode.ExtensionMode,
): string {
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
  const devServer = resolveCargoServerPath(extensionPath, exe);
  if (extensionMode === vscode.ExtensionMode.Development && devServer) {
    return devServer;
  }

  const bundled = path.join(extensionPath, "server", exe);
  if (fs.existsSync(bundled)) {
    return bundled;
  }

  return devServer ?? exe;
}

function resolveCargoServerPath(extensionPath: string, exe: string): string | undefined {
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
  return undefined;
}
