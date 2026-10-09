//! The window's open designs (D53.4): at most one `DesignSession` per design, shared by every pane
//! that shows it and kept after the last pane leaves until what it holds is sent.
//
// Why one per design. A session owns the design's feed (it subscribes when it opens and unsubscribes
// when it is disposed), and main keeps one feed owner per window and topic, with no count. Two
// sessions on one design in one window — a pane reopened while the previous one was still sending —
// meant the older one's late unsubscribe ended the feed under the newer one, which went silently
// deaf. Reusing the session makes that overlap impossible rather than guarded against: the feed
// follows the session, never the pane.
//
// A pane `attach`es and `detach`es. A session no pane shows keeps sending (with bounded retries) and
// is disposed once nothing is unsent, or once it stops; a stop while no pane showed it is reported as
// a notice naming the design, since nobody saw its status.
//
// No React and no `window`: the bridge is passed in, so this is tested with a stand-in daemon.

import { DesignSession, type DesignSessionOptions, type SessionBridge } from "./session";

/** What the registry tells the tab, about a design no pane may be showing. */
export interface RegistryNotice {
  readonly uid: string;
  readonly message: string;
}

/** The design a pane opens: its uid, its topic and the title notices name it by. */
export interface OpenDesign {
  readonly uid: string;
  readonly topic: string;
  readonly title: string;
}

interface Entry {
  readonly session: DesignSession;
  views: number;
  title: string;
  readonly off: () => void;
}

export class SessionRegistry {
  readonly #bridge: SessionBridge;
  readonly #options: DesignSessionOptions;
  readonly #entries = new Map<string, Entry>();
  readonly #listeners = new Set<(notice: RegistryNotice) => void>();
  /** Notices raised while no tab was listening, delivered to the next listener. */
  #queued: RegistryNotice[] = [];

  constructor(bridge: SessionBridge, options: DesignSessionOptions = {}) {
    this.#bridge = bridge;
    this.#options = options;
  }

  /** The design's session, the one already open if there is one (and it has not stopped). */
  attach(design: OpenDesign): DesignSession {
    const entry = this.#entries.get(design.uid);
    if (entry !== undefined && entry.session.status.kind !== "failed") {
      entry.views += 1;
      entry.title = design.title;
      entry.session.setAttached(true);
      return entry.session;
    }
    // A stopped session sends nothing more ("reopen the design"): reopening is a fresh one.
    if (entry !== undefined) this.#dispose(design.uid, entry);
    const session = new DesignSession(this.#bridge, design.uid, design.topic, this.#options);
    const off = session.onChange(() => this.#settle(design.uid));
    this.#entries.set(design.uid, { session, views: 1, title: design.title, off });
    void session.open();
    return session;
  }

  /** A pane stopped showing `session`. */
  detach(session: DesignSession): void {
    const entry = this.#entries.get(session.uid);
    if (entry === undefined || entry.session !== session || entry.views === 0) return;
    entry.views -= 1;
    if (entry.views > 0) return;
    session.setAttached(false);
    if (session.status.kind === "failed") {
      // Stopped while shown: the pane already said so.
      this.#dispose(session.uid, entry);
      return;
    }
    if (session.unsent) this.#notify({ uid: session.uid, message: `Edits to ${entry.title} are still being saved in the background.` });
    this.#settle(session.uid);
  }

  /** Hear notices; those raised while nobody listened arrive at once. Returns the unsubscribe. */
  onNotice(listener: (notice: RegistryNotice) => void): () => void {
    this.#listeners.add(listener);
    const queued = this.#queued;
    this.#queued = [];
    for (const n of queued) listener(n);
    return () => this.#listeners.delete(listener);
  }

  /** The open session of `uid`, if any (shown or still sending). */
  session(uid: string): DesignSession | undefined {
    return this.#entries.get(uid)?.session;
  }

  #settle(uid: string): void {
    const entry = this.#entries.get(uid);
    if (entry === undefined || entry.views > 0) return;
    const { session } = entry;
    if (session.status.kind === "failed") {
      this.#notify({ uid, message: `Edits to ${entry.title} were not saved: ${session.status.message}` });
      this.#dispose(uid, entry);
    } else if (!session.unsent) {
      this.#dispose(uid, entry);
    }
  }

  #dispose(uid: string, entry: Entry): void {
    this.#entries.delete(uid);
    entry.off();
    entry.session.dispose();
  }

  #notify(notice: RegistryNotice): void {
    if (this.#listeners.size === 0) this.#queued.push(notice);
    for (const l of this.#listeners) l(notice);
  }
}
