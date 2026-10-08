//! The typed bridge between the renderer and the main process (D53.1).
//
// The renderer has no Node: it reaches the outside world only through `window.jkb`, which the
// preload script builds from this contract and the main process serves. Everything that crosses
// is plain data — an op in, an `Outcome` out — so the daemon's token, the HTTP client and (later)
// the PTYs stay in main. A capability the renderer needs is added here, in the preload and in
// main's handlers together; nothing reaches the renderer any other way.

import type { Hello, OpRequest, OpResponse, Outcome } from "@jkb/core";

/** The IPC channels the bridge uses. One name per call, never built at runtime. */
export const BRIDGE_CHANNELS = {
  hello: "jkb:hello",
  op: "jkb:op",
  info: "jkb:info",
} as const;

/** What the renderer may know about where it runs. Nothing secret: no token, no token path. */
export interface AppInfo {
  /** The `jkb serve` URL the main process talks to. */
  readonly daemonUrl: string;
  readonly platform: string;
  readonly versions: { readonly electron: string; readonly chrome: string; readonly node: string };
}

/** `window.jkb`: everything the renderer can ask of the main process. */
export interface JkbBridge {
  /** `GET /v1/hello`: whether the daemon is reachable, and which schema and ops it serves. */
  hello(): Promise<Outcome<Hello>>;
  /** `POST /v1/op`: one op of `jkb serve`'s set, authenticated by main. */
  op(request: OpRequest): Promise<Outcome<OpResponse>>;
  /** Where the app is running. */
  info(): Promise<AppInfo>;
}
