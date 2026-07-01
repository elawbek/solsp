#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const TARGETS = {
  "win32-x64": {
    rust: "x86_64-pc-windows-msvc",
    exe: "solsp-server.exe",
  },
  "win32-arm64": {
    rust: "aarch64-pc-windows-msvc",
    exe: "solsp-server.exe",
  },
  "linux-x64": {
    rust: "x86_64-unknown-linux-gnu",
    exe: "solsp-server",
  },
  "linux-arm64": {
    rust: "aarch64-unknown-linux-gnu",
    exe: "solsp-server",
  },
  "darwin-x64": {
    rust: "x86_64-apple-darwin",
    exe: "solsp-server",
  },
  "darwin-arm64": {
    rust: "aarch64-apple-darwin",
    exe: "solsp-server",
  },
};

const args = process.argv.slice(2);
const target = readOption(args, "--target") ?? args.find((arg) => !arg.startsWith("-"));
const shouldBuild = args.includes("--build");
const shouldSkipPackage = args.includes("--stage-only");

if (!target || !(target in TARGETS)) {
  const supported = Object.keys(TARGETS).join(", ");
  fail(`Usage: npm run package:platform -- --target <target> [--build] [--stage-only]\nSupported targets: ${supported}`);
}

const scriptDir = dirname(fileURLToPath(import.meta.url));
const extensionDir = resolve(scriptDir, "..");
const repoRoot = resolve(extensionDir, "..", "..");
const distDir = join(repoRoot, "dist");
const serverDir = join(extensionDir, "server");
const { rust, exe } = TARGETS[target];
const source = join(repoRoot, "target", rust, "release", exe);
const staged = join(serverDir, exe);
const backupDir = shouldSkipPackage ? undefined : mkdtempSync(join(tmpdir(), "solsp-vscode-server-"));

if (shouldBuild) {
  run("cargo", ["build", "--release", "--target", rust], repoRoot);
}

if (!existsSync(source)) {
  fail(`Missing server binary: ${source}\nBuild it first or pass --build.`);
}

mkdirSync(serverDir, { recursive: true });
saveCurrentServerFiles();

try {
  stageServerBinary();

  if (shouldSkipPackage) {
    console.log(`Staged ${target} server binary at ${staged}`);
    process.exit(0);
  }

  mkdirSync(distDir, { recursive: true });
  const out = join(distDir, `solsp-vscode-${target}.vsix`);
  run(
    "npx",
    [
      "@vscode/vsce",
      "package",
      "--target",
      target,
      "--no-rewrite-relative-links",
      "--out",
      out,
    ],
    extensionDir,
  );
  console.log(`Packaged ${out}`);
} finally {
  if (!shouldSkipPackage) {
    restoreServerFiles();
  }
}

function saveCurrentServerFiles() {
  if (!backupDir) {
    return;
  }
  for (const name of ["solsp-server", "solsp-server.exe"]) {
    const current = join(serverDir, name);
    if (existsSync(current)) {
      copyFileSync(current, join(backupDir, name));
    }
  }
}

function restoreServerFiles() {
  if (!backupDir) {
    return;
  }
  for (const name of ["solsp-server", "solsp-server.exe"]) {
    const current = join(serverDir, name);
    rmSync(current, { force: true });
    const backup = join(backupDir, name);
    if (existsSync(backup)) {
      copyFileSync(backup, current);
      if (name !== "solsp-server.exe") {
        chmodSync(current, 0o755);
      }
    }
  }
  rmSync(backupDir, { force: true, recursive: true });
}

function stageServerBinary() {
  rmSync(join(serverDir, "solsp-server"), { force: true });
  rmSync(join(serverDir, "solsp-server.exe"), { force: true });
  copyFileSync(source, staged);

  if (!target.startsWith("win32-")) {
    chmodSync(staged, 0o755);
  }
}

function readOption(values, name) {
  const index = values.indexOf(name);
  if (index === -1) {
    return undefined;
  }
  return values[index + 1];
}

function run(command, commandArgs, cwd) {
  const result = spawnSync(command, commandArgs, {
    cwd,
    stdio: "inherit",
    shell: process.platform === "win32",
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${commandArgs.join(" ")} failed`);
  }
}

function fail(message) {
  console.error(message);
  process.exit(1);
}
