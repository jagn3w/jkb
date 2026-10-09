import { TABS, type TabId } from "../tabs";

/** A tab whose contents are not built yet: what it will hold, and the decision that governs it. */
export function EmptyPane({ tab }: { readonly tab: TabId }): React.JSX.Element {
  const spec = TABS.find((t) => t.id === tab);
  return (
    <div className="empty-pane">
      <h1>{spec?.label}</h1>
      <p>{spec?.summary}</p>
      <p className="muted">
        Not built yet — decided in <code>docs/code-factory.md</code> {spec?.decision}.
      </p>
    </div>
  );
}
