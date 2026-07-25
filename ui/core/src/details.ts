//! Detail-pane rendering: a {@link NodeDetails} → an HTML body string.
//
// Portable and DOM-free (pure string building), so the same output drives a VS Code
// Webview and a future web page. Editable controls carry a small `data-*` contract that a
// host wires to `MutationIntent`s (see WIRING below); the host owns the transport.
//
// WIRING CONTRACT (host attaches listeners, builds intents, and posts them):
//   root:            <div class="jkb-details" data-node-kind data-uid data-path>
//   editable field:  data-edit="status|priority|due|title"   (select / input / textarea)
//   save button:     data-action="save"       -> one intent per changed data-edit field
//   rename input:    data-edit="rename-to"     + data-action="rename"
//   add-tag inputs:  data-edit="tag-facet" / "tag-value" + data-action="add-tag"

import type { ItemDetails, NamespaceDetails, NodeDetails } from "./model.js";
import { TASK_STATUSES } from "./model.js";
import { allowsEdit, kindInfo } from "./registry.js";

/** Escape text for safe interpolation into HTML. */
export function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Render the details-pane body for any node kind. */
export function renderDetailsBody(d: NodeDetails): string {
  return d.kind === "namespace" ? renderNamespace(d) : renderItem(d);
}

function row(label: string, value: string | number | null | undefined): string {
  if (value === null || value === undefined || value === "") return "";
  return `<div class="jkb-row"><span class="jkb-key">${escapeHtml(label)}</span>` +
    `<span class="jkb-val">${escapeHtml(String(value))}</span></div>`;
}

function renderNamespace(d: NamespaceDetails): string {
  const info = kindInfo("namespace");
  const breakdown = Object.entries(d.breakdown)
    .sort((a, b) => a[0].localeCompare(b[0]))
    .map(([k, n]) => `<li>${escapeHtml(k)}: ${n}</li>`)
    .join("");
  return `<div class="jkb-details" data-node-kind="namespace" data-path="${escapeHtml(d.path)}">
    <h2>${escapeHtml(info.label)}</h2>
    ${row("path", d.path)}
    ${row("children", d.childCount)}
    ${breakdown ? `<ul class="jkb-breakdown">${breakdown}</ul>` : `<p class="jkb-empty">No children.</p>`}
    <div class="jkb-edit">
      <label>Rename / move to
        <input type="text" data-edit="rename-to" value="${escapeHtml(d.path)}" />
      </label>
      <button data-action="rename">Rename</button>
    </div>
  </div>`;
}

function renderItem(d: ItemDetails): string {
  const info = kindInfo(d.itemKind);
  const meta =
    row("uid", d.uid) +
    row("kind", d.itemKind) +
    row("namespace", d.namespace) +
    row("mime", d.mime) +
    row("binding", d.binding) +
    row("size", `${d.contentChars} chars${d.previewTruncated ? " (preview truncated)" : ""}`) +
    row("updated", d.updatedAt);

  const tags = d.tags.length
    ? `<div class="jkb-tags">${d.tags
        .map((t) => `<span class="jkb-tag">${escapeHtml(t.facet)}=${escapeHtml(t.value)}</span>`)
        .join("")}</div>`
    : "";

  // Task → its edit form (which includes the title/content). Content-editable non-task
  // items with an untruncated preview → an editable content box. Everything else →
  // a read-only preview.
  let body: string;
  if (d.itemKind === "task") {
    body = renderTaskEdit(d);
  } else if (allowsEdit(d.itemKind, "setItemContent") && !d.previewTruncated) {
    body = renderContentEdit(d);
  } else {
    body = previewBlock(d);
  }

  return `<div class="jkb-details" data-node-kind="${escapeHtml(d.itemKind)}" data-uid="${escapeHtml(d.uid)}">
    <h2>${escapeHtml(info.label)}</h2>
    ${meta}
    ${tags}
    ${body}
  </div>`;
}

// Rows to size a content textarea so short items stay compact and long ones are readable
// without dominating the pane. Accounts for both explicit newlines and soft-wrapped long
// lines (~80 cols), clamped to [3, 24]; the CSS caps height and lets it scroll past that.
function contentRows(text: string): number {
  const wrapped = text.split("\n").reduce((n, line) => n + Math.max(1, Math.ceil(line.length / 80)), 0);
  return Math.min(24, Math.max(3, wrapped));
}

function previewBlock(d: ItemDetails): string {
  if (!d.preview) return "";
  return `<h3>Preview</h3><pre class="jkb-preview">${escapeHtml(d.preview)}${
    d.previewTruncated ? "\n…" : ""
  }</pre>`;
}

function renderContentEdit(d: ItemDetails): string {
  return `<div class="jkb-edit">
    <label>Content
      <textarea data-edit="content" rows="${contentRows(d.preview)}">${escapeHtml(d.preview)}</textarea>
    </label>
    <button data-action="save-content">Save content</button>
  </div>`;
}

function renderTaskEdit(d: ItemDetails): string {
  const status = d.status ?? "open";
  const options = TASK_STATUSES.map(
    (s) => `<option value="${s}"${s === status ? " selected" : ""}>${s}</option>`,
  ).join("");
  // A task's content is its title plus any body (notes/design). Show it in full in a
  // field sized to the content (scrollable past ~24 rows). Editing + saving replaces the
  // whole content via `task edit`. If the content was truncated (unusual for a task),
  // fall back to a read-only preview so we never silently drop the tail.
  const titleField = d.previewTruncated
    ? previewBlock(d)
    : `<label>Title / notes
        <textarea data-edit="title" rows="${contentRows(d.preview)}">${escapeHtml(d.preview)}</textarea>
      </label>`;
  return `<div class="jkb-edit">
    <label>Status
      <select data-edit="status">${options}</select>
    </label>
    <label>Priority
      <input type="number" data-edit="priority" value="${d.priority ?? ""}" min="0" />
    </label>
    <label>Due
      <input type="text" data-edit="due" value="${escapeHtml(d.due ?? "")}" placeholder="2026-07-15" />
    </label>
    ${titleField}
    <button data-action="save">Save changes</button>
    <div class="jkb-addtag">
      <input type="text" data-edit="tag-facet" placeholder="facet" />
      <input type="text" data-edit="tag-value" placeholder="value" />
      <button data-action="add-tag">Add tag</button>
    </div>
  </div>`;
}
