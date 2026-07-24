//! Row colouring for the tree: maps @jkb/core's semantic colour keys to VS Code
//! ThemeColors, colouring both the item icon and (via a FileDecorationProvider) the label,
//! and adding a priority badge. Tasks are coloured by importance (p1 red → p3 yellow).

import * as vscode from "vscode";

import { nodeDecoration, type TreeChild } from "@jkb/core";

const SCHEME = "jkb-node";

/** Semantic colour key (from @jkb/core) → VS Code ThemeColor id. */
const COLOR: Record<string, string> = {
  "priority-1": "charts.red",
  "priority-2": "charts.orange",
  "priority-3": "charts.yellow",
  task: "charts.blue",
  muted: "descriptionForeground",
  "kind-namespace": "charts.foreground",
  "kind-document": "charts.green",
  "kind-chunk": "descriptionForeground",
  "kind-text": "charts.blue",
  "kind-note": "charts.blue",
  "kind-view": "charts.purple",
};

function themeColor(
  kind: string,
  status: string | null | undefined,
  priority: number | null | undefined,
): vscode.ThemeColor | undefined {
  const id = COLOR[nodeDecoration(kind, status, priority).colorKey];
  return id ? new vscode.ThemeColor(id) : undefined;
}

/** The (coloured) icon for a node. */
export function nodeIcon(
  kind: string,
  iconId: string,
  status: string | null | undefined,
  priority: number | null | undefined,
): vscode.ThemeIcon {
  const color = themeColor(kind, status, priority);
  return color ? new vscode.ThemeIcon(iconId, color) : new vscode.ThemeIcon(iconId);
}

/** A synthetic resource URI encoding a node's kind/status/priority for decoration. */
export function nodeUri(child: TreeChild): vscode.Uri {
  const kind = child.ref.kind === "namespace" ? "namespace" : child.ref.itemKind;
  const id = child.ref.kind === "namespace" ? child.ref.path : child.ref.uid;
  const q: string[] = [];
  if (child.priority != null) q.push(`p=${child.priority}`);
  if (child.status) q.push(`s=${encodeURIComponent(child.status)}`);
  return vscode.Uri.from({
    scheme: SCHEME,
    authority: kind,
    path: `/${encodeURIComponent(id)}`,
    query: q.join("&"),
  });
}

/** Colours tree labels and adds a priority badge, from the node URI's encoded metadata. */
export class JkbDecorationProvider implements vscode.FileDecorationProvider {
  private readonly emitter = new vscode.EventEmitter<undefined>();
  readonly onDidChangeFileDecorations = this.emitter.event;

  refresh(): void {
    this.emitter.fire(undefined);
  }

  provideFileDecoration(uri: vscode.Uri): vscode.FileDecoration | undefined {
    if (uri.scheme !== SCHEME) return undefined;
    const kind = uri.authority;
    const params = new URLSearchParams(uri.query);
    const priority = params.has("p") ? Number(params.get("p")) : null;
    const status = params.get("s");
    const deco = nodeDecoration(kind, status, priority);
    const colorId = COLOR[deco.colorKey];
    return new vscode.FileDecoration(
      deco.badge,
      undefined,
      colorId ? new vscode.ThemeColor(colorId) : undefined,
    );
  }
}
