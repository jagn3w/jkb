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
// Notices are the registry's own list, kept until each is dismissed — not the tab's one notice slot,
// which every *Discuss* clears: an unread report that edits were lost must not be erased by the next
// click. "Still being saved" is withdrawn by itself once the edits land or the design is shown again.
//
// No React and no `window`: the bridge is passed in, so this is tested with a stand-in daemon.

import { DesignSession, type DesignSessionOptions, type SessionBridge } from "./session";

/**
 * What the registry tells the tab, about a design no pane may be showing: its edits are still being
 * saved (withdrawn when they are), they were not saved, or a *Discuss* of it was not started.
 */
export interface RegistryNotice {
  readonly id: number;
  readonly uid: string;
  readonly kind: "saving" | "lost" | "closed";
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
  readonly #listeners = new Set<() => void>();
  #notices: readonly RegistryNotice[] = [];
  #nextNotice = 1;

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
      // Shown again: its status says how saving goes.
      this.#withdraw(design.uid, "saving");
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
    if (session.unsent) this.notify(session.uid, "saving", `Edits to ${entry.title} are still being saved in the background.`);
    this.#settle(session.uid);
  }

  /** The notices not yet dismissed, oldest first. */
  get notices(): readonly RegistryNotice[] {
    return this.#notices;
  }

  /** Hear every change to the notices. Returns the unsubscribe. */
  onNotices(listener: () => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  /** Add a notice about `uid`; one of the same kind and words is not repeated. */
  notify(uid: string, kind: RegistryNotice["kind"], message: string): void {
    if (this.#notices.some((n) => n.uid === uid && n.kind === kind && n.message === message)) return;
    this.#notices = [...this.#notices, { id: this.#nextNotice++, uid, kind, message }];
    this.#noticesChanged();
  }

  dismiss(id: number): void {
    const kept = this.#notices.filter((n) => n.id !== id);
    if (kept.length === this.#notices.length) return;
    this.#notices = kept;
    this.#noticesChanged();
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
      this.#withdraw(uid, "saving");
      this.notify(uid, "lost", `Edits to ${entry.title} were not saved: ${session.status.message}`);
      this.#dispose(uid, entry);
    } else if (!session.unsent) {
      this.#withdraw(uid, "saving");
      this.#dispose(uid, entry);
    }
  }

  #withdraw(uid: string, kind: RegistryNotice["kind"]): void {
    const kept = this.#notices.filter((n) => n.uid !== uid || n.kind !== kind);
    if (kept.length === this.#notices.length) return;
    this.#notices = kept;
    this.#noticesChanged();
  }

  #noticesChanged(): void {
    for (const l of this.#listeners) l();
  }

  #dispose(uid: string, entry: Entry): void {
    this.#entries.delete(uid);
    entry.off();
    entry.session.dispose();
  }

}
