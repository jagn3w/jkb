//! The details pane: a single reused Webview that renders @jkb/core detail HTML and turns
//! its `data-*` edit controls into MutationIntents applied through the CLI client.

import * as vscode from "vscode";

import { renderDetailsBody, type JkbClient, type MutationIntent, type NodeRef } from "@jkb/core";

/** Messages the webview posts back to the host. */
interface EditMessage {
  intents: MutationIntent[];
}

export class DetailsPanel {
  private static current: DetailsPanel | undefined;

  /** Open (or reuse) the details pane for `ref`. */
  static show(client: JkbClient, ref: NodeRef, onMutated: () => void): void {
    if (!DetailsPanel.current) {
      DetailsPanel.current = new DetailsPanel(client, onMutated);
    }
    void DetailsPanel.current.load(ref);
    DetailsPanel.current.panel.reveal(vscode.ViewColumn.Beside, true);
  }

  private readonly panel: vscode.WebviewPanel;
  private ref: NodeRef | undefined;

  private constructor(
    private readonly client: JkbClient,
    private readonly onMutated: () => void,
  ) {
    this.panel = vscode.window.createWebviewPanel(
      "jkb.details",
      "jkb Details",
      { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
      { enableScripts: true, retainContextWhenHidden: true },
    );
    this.panel.onDidDispose(() => {
      DetailsPanel.current = undefined;
    });
    this.panel.webview.onDidReceiveMessage((msg: EditMessage) => {
      void this.applyEdits(msg.intents);
    });
  }

  private async applyEdits(intents: MutationIntent[]): Promise<void> {
    try {
      for (const intent of intents) {
        await this.client.mutate(intent);
      }
      this.onMutated();
      if (this.ref) await this.load(this.ref);
      vscode.window.setStatusBarMessage("jkb: saved", 2000);
    } catch (e) {
      vscode.window.showErrorMessage(`jkb: ${(e as Error).message}`);
    }
  }

  private async load(ref: NodeRef): Promise<void> {
    this.ref = ref;
    try {
      const details = await this.client.getDetails(ref);
      this.panel.webview.html = this.wrap(renderDetailsBody(details));
    } catch (e) {
      this.panel.webview.html = this.wrap(
        `<p class="jkb-error">${(e as Error).message}</p>`,
      );
    }
  }

  private wrap(body: string): string {
    const nonce = nonceString();
    const csp = [
      "default-src 'none'",
      "style-src 'unsafe-inline'",
      `script-src 'nonce-${nonce}'`,
    ].join("; ");
    return `<!DOCTYPE html><html lang="en"><head>
      <meta charset="UTF-8" />
      <meta http-equiv="Content-Security-Policy" content="${csp}" />
      <style>${STYLE}</style>
    </head><body>${body}<script nonce="${nonce}">${WIRING}</script></body></html>`;
  }
}

function nonceString(): string {
  let s = "";
  const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  for (let i = 0; i < 24; i++) s += chars[Math.floor(Math.random() * chars.length)];
  return s;
}

const STYLE = `
  body { font-family: var(--vscode-font-family); color: var(--vscode-foreground); padding: 0 12px; }
  h2 { margin: 12px 0 8px; font-size: 1.15em; }
  h3 { margin: 16px 0 4px; font-size: 1em; opacity: 0.85; }
  .jkb-row { display: flex; gap: 8px; margin: 2px 0; font-size: 0.92em; }
  .jkb-key { min-width: 92px; opacity: 0.7; }
  .jkb-val { word-break: break-word; }
  .jkb-breakdown { margin: 4px 0; padding-left: 18px; }
  .jkb-empty { opacity: 0.6; }
  .jkb-tags { margin: 8px 0; display: flex; flex-wrap: wrap; gap: 6px; }
  .jkb-tag { background: var(--vscode-badge-background); color: var(--vscode-badge-foreground); border-radius: 4px; padding: 1px 6px; font-size: 0.85em; }
  .jkb-edit { margin: 14px 0; padding: 10px; border: 1px solid var(--vscode-panel-border); border-radius: 6px; display: flex; flex-direction: column; gap: 8px; }
  .jkb-edit label { display: flex; flex-direction: column; gap: 2px; font-size: 0.85em; opacity: 0.85; }
  .jkb-edit input, .jkb-edit select, .jkb-edit textarea {
    background: var(--vscode-input-background); color: var(--vscode-input-foreground);
    border: 1px solid var(--vscode-input-border, transparent); border-radius: 4px; padding: 4px 6px; font-family: inherit;
  }
  .jkb-edit textarea { resize: vertical; max-height: 60vh; line-height: 1.45; }
  .jkb-addtag { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }
  button { background: var(--vscode-button-background); color: var(--vscode-button-foreground); border: none; border-radius: 4px; padding: 5px 12px; cursor: pointer; }
  button:hover { background: var(--vscode-button-hoverBackground); }
  .jkb-preview { background: var(--vscode-textCodeBlock-background); padding: 10px; border-radius: 6px; white-space: pre-wrap; word-break: break-word; max-height: 320px; overflow: auto; font-size: 0.88em; }
  .jkb-error { color: var(--vscode-errorForeground); }
`;

// Webview-side wiring: read the @jkb/core data-* controls and post edit intents. Kept
// small and dependency-free; a web host would ship an equivalent against the same contract.
const WIRING = `
  const vscode = acquireVsCodeApi();
  const root = document.querySelector('.jkb-details');
  function value(field) {
    const el = root && root.querySelector('[data-edit="' + field + '"]');
    return el ? String(el.value).trim() : '';
  }
  function post(intents) { if (intents.length) vscode.postMessage({ intents }); }
  document.addEventListener('click', function (e) {
    const btn = e.target.closest('[data-action]');
    if (!btn || !root) return;
    const action = btn.getAttribute('data-action');
    const uid = root.getAttribute('data-uid');
    if (action === 'save') {
      const out = [];
      const status = value('status'); if (status) out.push({ type: 'setTaskStatus', uid, status });
      const pr = value('priority'); if (pr !== '') out.push({ type: 'setTaskPriority', uid, priority: Number(pr) });
      const due = value('due'); if (due) out.push({ type: 'setTaskDue', uid, due });
      const title = value('title'); if (title) out.push({ type: 'setTaskTitle', uid, title });
      post(out);
    } else if (action === 'rename') {
      const from = root.getAttribute('data-path');
      const to = value('rename-to');
      if (to && to !== from) post([{ type: 'renameNamespace', from, to }]);
    } else if (action === 'add-tag') {
      const facet = value('tag-facet');
      const val = value('tag-value');
      if (facet && val) post([{ type: 'addTaskTag', uid, facet, value: val }]);
    } else if (action === 'save-content') {
      const content = value('content');
      if (content) post([{ type: 'setItemContent', uid, content }]);
    }
  });
`;
