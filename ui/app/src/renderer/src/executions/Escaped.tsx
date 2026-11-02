import { escapeSegments, type Segment } from "@jkb/core";

import { diffLineKinds } from "./queue";

/**
 * Text that came from a session or a repository, shown as text — never as HTML: each segment is a
 * React text child, and an escape (a control, bidi or invisible character) is drawn apart, so it is
 * never mistaken for the text around it.
 */
export function Segments({ segments }: { readonly segments: readonly Segment[] }): React.JSX.Element {
  return (
    <>
      {segments.map((s, i) =>
        s.escaped ? (
          <span key={i} className="esc" title="a character that may not show as itself">
            {s.text}
          </span>
        ) : (
          <span key={i}>{s.text}</span>
        ),
      )}
    </>
  );
}

/** A block of untrusted text in a `<pre>`, escaped. */
export function EscapedPre({ text, className }: { readonly text: string; readonly className?: string }): React.JSX.Element {
  return (
    <pre className={className ?? "exec-pre"}>
      <Segments segments={escapeSegments(text)} />
    </pre>
  );
}

/** A unified diff, escaped line by line and coloured by what each line is. */
export function DiffPre({ patch }: { readonly patch: string }): React.JSX.Element {
  const lines = patch.split("\n");
  const kinds = diffLineKinds(lines);
  return (
    <pre className="exec-pre exec-diff">
      {lines.map((line, i) => (
        <div key={i} className={`diff-${kinds[i] ?? "ctx"}`}>
          <Segments segments={escapeSegments(line)} />
          {"\n"}
        </div>
      ))}
    </pre>
  );
}
