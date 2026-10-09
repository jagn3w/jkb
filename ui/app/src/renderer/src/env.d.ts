/// <reference types="vite/client" />

import type { JkbBridge } from "../../shared/bridge";

declare global {
  interface Window {
    /** The preload's bridge to the main process: the renderer's only way out (D53.1). */
    readonly jkb: JkbBridge;
  }
}
