import { useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";

import { SIDEBAR_WIDTH, SIDEBAR_WIDTH_KEY, clampSidebarWidth, parseSidebarWidth } from "./sidebarWidth";

function loadWidth(): number {
  try {
    return parseSidebarWidth(window.localStorage.getItem(SIDEBAR_WIDTH_KEY));
  } catch {
    return SIDEBAR_WIDTH.default;
  }
}

function saveWidth(width: number): void {
  try {
    window.localStorage.setItem(SIDEBAR_WIDTH_KEY, String(width));
  } catch {
    // Storage unavailable: the sidebar opens at its default width next time.
  }
}

/** The handle's pointer and double-click handlers. */
export interface SidebarHandle {
  readonly onPointerDown: (e: ReactPointerEvent<HTMLElement>) => void;
  readonly onPointerMove: (e: ReactPointerEvent<HTMLElement>) => void;
  readonly onPointerUp: () => void;
  readonly onDoubleClick: () => void;
}

/**
 * The sidebar's width and the handlers of the handle that drags it, as the terminal drawer's height:
 * clamped to the window as it resizes, saved when a drag ends, and back to the default on a double-click.
 */
export function useSidebarWidth(): { readonly width: number; readonly handle: SidebarHandle } {
  const [width, setWidth] = useState(() => clampSidebarWidth(loadWidth(), window.innerWidth));
  const drag = useRef<{ startX: number; startWidth: number } | undefined>(undefined);

  useEffect(() => {
    const onResize = (): void => setWidth((w) => clampSidebarWidth(w, window.innerWidth));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  const onPointerDown = useCallback(
    (e: ReactPointerEvent<HTMLElement>) => {
      drag.current = { startX: e.clientX, startWidth: width };
      e.currentTarget.setPointerCapture(e.pointerId);
    },
    [width],
  );
  const onPointerMove = useCallback((e: ReactPointerEvent<HTMLElement>) => {
    const d = drag.current;
    if (d !== undefined) setWidth(clampSidebarWidth(d.startWidth + (e.clientX - d.startX), window.innerWidth));
  }, []);
  const onPointerUp = useCallback(() => {
    if (drag.current === undefined) return;
    drag.current = undefined;
    setWidth((w) => {
      saveWidth(w);
      return w;
    });
  }, []);
  const onDoubleClick = useCallback(() => {
    const w = clampSidebarWidth(SIDEBAR_WIDTH.default, window.innerWidth);
    saveWidth(w);
    setWidth(w);
  }, []);
  return { width, handle: { onPointerDown, onPointerMove, onPointerUp, onDoubleClick } };
}
