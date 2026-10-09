import { useEffect, useRef } from "react";

import type { TerminalSession } from "./session";

/**
 * Where a terminal's screen is shown. The screen itself belongs to the session and is moved in
 * here, so it keeps its scrollback when the view changes (a tab switch, popover to drawer).
 */
export function TerminalView({
  session,
  visible,
}: {
  readonly session: TerminalSession | undefined;
  readonly visible: boolean;
}): React.JSX.Element {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (el === null || session === undefined) return;
    session.attach(el);
    const observer = new ResizeObserver(() => session.fit());
    observer.observe(el);
    return () => observer.disconnect();
  }, [session]);

  useEffect(() => {
    if (!visible || session === undefined) return;
    session.fit();
    session.focus();
  }, [visible, session]);

  return <div ref={ref} className="terminal-view" />;
}
