// VS Code cannot talk to an arbitrary LSP binary on its own, so this exists to
// spawn jabar and point it at Java files. It deliberately does almost nothing
// else: everything a user sees comes from the server, and logic that lives here
// is logic no other editor gets.

import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const config = vscode.workspace.getConfiguration("jabar");
  const command = config.get<string>("server.path")?.trim() || "jabar";

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
    // The overwhelmingly likely cause is that the binary is not on PATH, so
    // say that rather than surfacing a spawn errno.
    vscode.window.showErrorMessage(
      `jabar failed to start (\`${command}\`). Set \`jabar.server.path\`, or ` +
        `build it with \`cargo build --release\`. Details: ${error}`,
    );
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
  const path = await vscode.window.showInputBox({
    prompt: "Directory holding the SCIP shards",
    value: suggested,
  });
  if (!path) {
    return;
  }
  try {
    const result = await client.sendRequest<LoadIndexResult>("jabar/loadIndex", { path });
    vscode.window.showInformationMessage(
      `jabar: loaded ${result.definitions} definitions from ${result.shards} shards.`,
    );
  } catch (error) {
    vscode.window.showErrorMessage(`jabar: could not load an index from ${path}. ${error}`);
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
