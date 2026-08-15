// VS Code cannot talk to an arbitrary LSP binary on its own, so this exists to
// spawn jabar and point it at Java files. It deliberately does almost nothing
// else: everything a user sees comes from the server, and logic that lives here
// is logic no other editor gets.

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

/// Where to look for the binary, in order, before falling back to PATH.
///
/// Running from a source checkout is the normal case for now, and requiring an
/// absolute path in settings to get past a first launch is a poor trade for a
/// few lines of searching. `release` precedes `debug` because a debug build is
/// usually a leftover rather than a choice.
function locateServer(context: vscode.ExtensionContext): string | undefined {
  const configured = vscode.workspace.getConfiguration("jabar").get<string>("server.path")?.trim();
  if (configured) {
    // Taken at its word: if someone set it and it is wrong, saying so beats
    // silently using a different binary than they asked for.
    return configured;
  }

  const roots = [
    // The extension lives at <repo>/editors/vscode, so the build output is two
    // levels up. This is what makes `--extensionDevelopmentPath` just work.
    path.resolve(context.extensionPath, "..", ".."),
    // Or the user opened the jabar repo itself.
    ...(vscode.workspace.workspaceFolders ?? []).map((folder) => folder.uri.fsPath),
  ];

  for (const root of roots) {
    for (const profile of ["release", "debug"]) {
      const candidate = path.join(root, "target", profile, "jabar");
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return undefined;
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const config = vscode.workspace.getConfiguration("jabar");
  const located = locateServer(context);
  const command = located ?? "jabar";

  const serverOptions: ServerOptions = {
    command,
    transport: TransportKind.stdio,
    options: {
      env: {
        ...process.env,
        // The server logs to stderr; stdout carries the protocol.
        JABAR_LOG: config.get<string>("server.log") || "info",
      },
    },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "java" }],
    // jabar reads its index from bazel-bin, so a rebuild is a server-side
    // event. It watches for that itself; this tells VS Code not to also stream
    // us file events we would only discard.
    synchronize: {},
    outputChannel: vscode.window.createOutputChannel("jabar"),
  };

  client = new LanguageClient("jabar", "jabar", serverOptions, clientOptions);

  context.subscriptions.push(
    vscode.commands.registerCommand("jabar.status", () => showStatus()),
    vscode.commands.registerCommand("jabar.reloadIndex", () => reloadIndex()),
  );

  try {
    await client.start();
  } catch (error) {
    // `spawn jabar ENOENT` on its own tells a user nothing actionable, so name
    // what was tried and what would fix it.
    const tried = located
      ? `Tried \`${command}\`.`
      : `No built binary was found near the extension or the workspace, so \`jabar\` ` +
        `was looked up on PATH and is not there.`;
    const choice = await vscode.window.showErrorMessage(
      `jabar could not start. ${tried} Build it with \`cargo build --release\`, ` +
        `or set \`jabar.server.path\` to the binary.`,
      "Open settings",
    );
    if (choice === "Open settings") {
      await vscode.commands.executeCommand("workbench.action.openSettings", "jabar.server.path");
    }
    console.error("jabar failed to start", error);
  }
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}

/// `jabar/status` reports what the server currently believes about itself,
/// including whether an index is loaded. Without it, a server that is running
/// but has no index is indistinguishable from one that is working.
async function showStatus(): Promise<void> {
  if (!client) {
    vscode.window.showWarningMessage("jabar is not running.");
    return;
  }
  try {
    const status = await client.sendRequest<JabarStatus>("jabar/status", null);
    const lines = [
      `workspace: ${status.workspaceRoot ?? "(none)"}`,
      `index: ${status.indexLoaded ? `${status.indexedDefinitions} definitions` : "not loaded"}`,
      `watching for rebuilds: ${status.watching}`,
      `position encoding: ${status.positionEncoding}`,
      `open documents: ${status.openDocuments}`,
    ];
    const concerns = status.health?.concerns ?? [];
    if (concerns.length > 0) {
      lines.push(`concerns: ${concerns.map((c) => c.kind).join(", ")}`);
    }
    vscode.window.showInformationMessage(lines.join(" · "), { modal: false });
  } catch (error) {
    vscode.window.showErrorMessage(`jabar/status failed: ${error}`);
  }
}

/// Re-reads the shards without restarting the server.
///
/// The server watches `bazel-bin` and reloads on its own, so this is for when
/// an index was produced somewhere else, or the watcher could not start.
async function reloadIndex(): Promise<void> {
  if (!client) {
    vscode.window.showWarningMessage("jabar is not running.");
    return;
  }
  const folder = vscode.workspace.workspaceFolders?.[0];
  if (!folder) {
    vscode.window.showWarningMessage("jabar: no workspace folder is open.");
    return;
  }
  const suggested = vscode.Uri.joinPath(folder.uri, "bazel-bin").fsPath;
  // Not named `path`: that would shadow the module imported above, which is a
  // trap for the next edit rather than a problem today.
  const dir = await vscode.window.showInputBox({
    prompt: "Directory holding the SCIP shards",
    value: suggested,
  });
  if (!dir) {
    return;
  }
  try {
    const result = await client.sendRequest<LoadIndexResult>("jabar/loadIndex", { path: dir });
    vscode.window.showInformationMessage(
      `jabar: loaded ${result.definitions} definitions from ${result.shards} shards.`,
    );
  } catch (error) {
    vscode.window.showErrorMessage(`jabar: could not load an index from ${dir}. ${error}`);
  }
}

interface JabarStatus {
  workspaceRoot: string | null;
  indexLoaded: boolean;
  indexedDefinitions: number;
  watching: boolean;
  positionEncoding: string;
  openDocuments: number;
  health?: { concerns: { kind: string }[] };
}

interface LoadIndexResult {
  shards: number;
  definitions: number;
}
