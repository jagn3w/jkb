//! The typed bridge between the renderer and the main process (D53.1).
//
// The renderer has no Node: it reaches the outside world only through `window.jkb`, which the
// preload script builds from this contract and the main process serves. Everything that crosses
// is plain data — an op in, an `Outcome` out; a terminal spec in, its output out — so the daemon's
// token, the HTTP client and the PTYs stay in main. A capability the renderer needs is added here,
// in the preload and in main's handlers together; nothing reaches the renderer any other way.

import type {
  ContainerAction,
  ContainerResult,
  ContainerStatus,
  DesignAnnouncement,
  GitPlace,
  Hello,
  OpRequest,
  OpResponse,
  Outcome,
  PromptAnnouncement,
} from "@jkb/core";

import type { TerminalEvent, TerminalInfo, TerminalResult, TerminalRoots, TerminalSpec } from "./terminal";

/** The IPC channels the bridge uses. One name per call, never built at runtime. */
export const BRIDGE_CHANNELS = {
  hello: "jkb:hello",
  op: "jkb:op",
  info: "jkb:info",
  terminalOpen: "jkb:terminal:open",
  terminalWrite: "jkb:terminal:write",
  terminalResize: "jkb:terminal:resize",
  terminalClose: "jkb:terminal:close",
  /** main → renderer: a terminal's output or exit. */
  terminalEvent: "jkb:terminal:event",
  designSubscribe: "jkb:design:subscribe",
  designUnsubscribe: "jkb:design:unsubscribe",
  /** main → renderer: a design topic's update, gap or error. */
  designEvent: "jkb:design:event",
  containerStatus: "jkb:container:status",
  containerSpec: "jkb:container:spec",
  notifySubscribe: "jkb:notify:subscribe",
  notifyUnsubscribe: "jkb:notify:unsubscribe",
  /** main → renderer: a session's notification moved, or the feed's standing changed. */
  notifyEvent: "jkb:notify:event",
  sessionsPlace: "jkb:sessions:place",
} as const;

/** What the renderer may know about where it runs. Nothing secret: no token, no token path. */
export interface AppInfo {
  /** The `jkb serve` URL the main process talks to. */
  readonly daemonUrl: string;
  readonly platform: string;
  readonly versions: { readonly electron: string; readonly chrome: string; readonly node: string };
  /** Which container a container terminal enters, and where each side's working trees are. */
  readonly terminal: TerminalRoots;
}

/** `window.jkb.terminal`: the PTYs main runs for this window (D53.10). */
export interface TerminalBridge {
  /** Start a terminal from `spec` at an initial size. Main validates the spec; nothing is trusted. */
  open(spec: TerminalSpec, cols: number, rows: number): Promise<TerminalResult<TerminalInfo>>;
  /** Send keystrokes or a paste (at most `MAX_WRITE_CHARS` at a time). */
  write(id: number, data: string): void;
  resize(id: number, cols: number, rows: number): void;
  /** End the terminal's process; its exit arrives as an event. */
  close(id: number): void;
  /** Hear every terminal event for this window. Returns the unsubscribe. */
  onEvent(listener: (event: TerminalEvent) => void): () => void;
}

/**
 * What main tells a window about a design topic it subscribed to (D53.4).
 * - `update`: an update was announced; merge it (or, with `update: null`, too large to carry,
 *   re-read the state).
 * - `gap`: updates may have been missed; re-read the state.
 * - `error`: the feed cannot reach the daemon and is retrying; informational.
 * - `live`: the feed is back after an `error`; nothing was missed (its place was kept).
 * - `prompt`: a launch recorded (or moved) one of the design's prompts; re-read the Prompts pane (D53.6).
 */
export type DesignFeedEvent =
  | ({ readonly topic: string; readonly kind: "update" } & DesignAnnouncement)
  | ({ readonly topic: string; readonly kind: "prompt" } & PromptAnnouncement)
  | { readonly topic: string; readonly kind: "gap"; readonly message: string }
  | { readonly topic: string; readonly kind: "error"; readonly message: string }
  | { readonly topic: string; readonly kind: "live" };

/** `window.jkb.design`: live updates of the designs a window has open (D53.4). */
export interface DesignBridge {
  /**
   * Hear `topic` (a design's `design/<uid>`). Resolves once main's consumer group is on the topic:
   * read the design's state after that, and no update falls between the read and the feed.
   */
  subscribe(topic: string): Promise<Outcome<null>>;
  unsubscribe(topic: string): void;
  /** Hear every design event for this window. Returns the unsubscribe. */
  onEvent(listener: (event: DesignFeedEvent) => void): () => void;
}

/**
 * `window.jkb.container`: the dev container, through the installed kit's `run.sh` (D53.8). Main
 * finds the kit and checks it; the renderer only names what it wants done.
 */
export interface ContainerBridge {
  /** `run.sh --status`: what the container is, its image's labels and its drift. */
  status(): Promise<ContainerResult<ContainerStatus>>;
  /**
   * The host terminal that runs `action` (the kit's `run.sh` with that action's one flag), for the
   * renderer to open in the integrated terminal so its output streams there.
   */
  spec(action: ContainerAction): Promise<ContainerResult<TerminalSpec>>;
}

/**
 * What main tells a window about `claude/notify` (D53.9).
 * - `changed`: a session's notification was posted or withdrawn; re-read `notify.open_sessions`.
 * - `gap`, `error`, `live`: the feed's standing, as for a design topic.
 */
export type NotifyFeedEvent =
  | { readonly topic: string; readonly kind: "changed"; readonly session: string }
  | { readonly topic: string; readonly kind: "gap"; readonly message: string }
  | { readonly topic: string; readonly kind: "error"; readonly message: string }
  | { readonly topic: string; readonly kind: "live" };

/** `window.jkb.notify`: the needs-input feed, the app's own consumer group on `claude/notify` (D53.9). */
export interface NotifyBridge {
  /**
   * Hear `claude/notify`. Resolves once main's group is on the topic: read the records after that,
   * and no change falls between the read and the feed.
   */
  subscribe(): Promise<Outcome<null>>;
  unsubscribe(): void;
  /** Hear every notify event for this window. Returns the unsubscribe. */
  onEvent(listener: (event: NotifyFeedEvent) => void): () => void;
}

/** `window.jkb.sessions`: what the Sessions tab needs of the host beyond the op set (D53.9). */
export interface SessionsBridge {
  /**
   * The checkout a session's working directory is in — its root (on the host), repo key and branch —
   * read from git's files by main. Only directories under the repos mount are looked at.
   */
  place(cwd: string): Promise<TerminalResult<GitPlace>>;
}

/** `window.jkb`: everything the renderer can ask of the main process. */
export interface JkbBridge {
  /** `GET /v1/hello`: whether the daemon is reachable, and which schema and ops it serves. */
  hello(): Promise<Outcome<Hello>>;
  /** `POST /v1/op`: one op of `jkb serve`'s set, authenticated by main. */
  op(request: OpRequest): Promise<Outcome<OpResponse>>;
  /** Where the app is running. */
  info(): Promise<AppInfo>;
  /** The integrated terminal's PTYs. */
  readonly terminal: TerminalBridge;
  /** Live design updates. */
  readonly design: DesignBridge;
  /** The dev container, through the kit. */
  readonly container: ContainerBridge;
  /** The needs-input feed. */
  readonly notify: NotifyBridge;
  /** Where sessions run. */
  readonly sessions: SessionsBridge;
}
