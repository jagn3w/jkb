//! Live feeds of `mq` topics, in the main process (D53.1): one long-poll per topic, shared by every
//! window hearing it, forwarded to those windows. Design topics (`designFeeds.ts`, D53.4) and
//! `claude/notify` (`notifyFeed.ts`, D53.9) are the two kinds; each says which topics it accepts and
//! what a message on one means, and this file is the one loop both run.
//
// A subscription, never a poll loop on a timer: each feed holds an `mq.poll` with `wait_ms` open on
// `jkb serve`, which answers the moment a message is sent. The app reads with its own consumer
// group, `code-factory` — one name, so a closed app leaves at most one group per topic behind
// (removed by the queue once idle), never one per launch. A kind that says `leaves` removes its group
// when its feed ends and when the app quits (`leaveAll`): `claude/notify` does, because a group left
// there holds every later notification unreapable, and at the topic's cap `notify.event` is refused
// (D53.9). Delivery is at-least-once, and every message the app
// reads is idempotent to hear twice (an update merges, a notification change re-reads); a message
// *missed* (a group the queue removed while the app was away) is reported as a `gap`, and the window
// re-reads the state.
//
// No Electron import: the feeds are given the op call and a delivery callback, so they are tested
// with a stand-in daemon.

import type { OpRequest, OpResponse, Outcome } from "@jkb/core";

/** The consumer group the app reads every topic with. */
export const APP_GROUP = "code-factory";

/** One op on the daemon, with the long-poll wait for `mq.poll`. */
export type FeedOp = (request: OpRequest, options?: { readonly waitMs?: number }) => Promise<Outcome<OpResponse>>;

/**
 * How a feed stands, told to its windows beside its messages' own events.
 * - `gap`: messages may have been missed; re-read the state.
 * - `error`: the feed cannot reach the daemon and is retrying; informational.
 * - `live`: the feed is back after an `error`; nothing was missed (its place was kept).
 */
export type FeedStatusEvent =
  | { readonly topic: string; readonly kind: "gap"; readonly message: string }
  | { readonly topic: string; readonly kind: "error"; readonly message: string }
  | { readonly topic: string; readonly kind: "live" };

/** What one kind of feed is: the topics it may be asked for, and what a message on one means. */
export interface FeedKind<E> {
  /** Whether `topic` is one this kind serves. Anything else is refused before any call is made. */
  accepts(topic: unknown): topic is string;
  /** What a refused topic is told. */
  readonly refusal: string;
  /** The events a message makes (none for a message the windows need not hear). */
  events(topic: string, kind: unknown, payload: unknown): readonly E[];
  /**
   * Remove the app's group (`mq.group_delete`) when the feed ends — its last window gone — and on
   * quit, rather than leave it holding the topic until the queue removes it as idle.
   */
  readonly leaves?: boolean;
}

export interface FeedOptions {
  /** How long a poll is held open waiting for a message. */
  readonly waitMs?: number;
  /** At most this many messages per poll. */
  readonly max?: number;
  /** The first retry delay after a failed call; doubles to `maxBackoffMs`. */
  readonly minBackoffMs?: number;
  readonly maxBackoffMs?: number;
  /** How to wait between retries; `setTimeout` by default. */
  readonly sleep?: (ms: number) => Promise<void>;
  /**
   * Told when a group removal (`leaves`) did not land — refused (a daemon older than
   * `mq.group_delete` answers `bad_request`), or unreachable — so the group left behind is said
   * somewhere rather than nowhere. Nothing by default.
   */
  readonly log?: (message: string) => void;
}

type Owner = number;

interface Feed {
  readonly topic: string;
  readonly owners: Set<Owner>;
  /** Subscribers waiting for the group to be joined. */
  waiters: ((outcome: Outcome<null>) => void)[];
  joined: boolean;
  /** The app's group was removed on quit: nothing more is sent for this feed. */
  left: boolean;
}

const defaultSleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/** The live feeds of one kind of topic, for every window. */
export class TopicFeeds<E> {
  readonly #op: FeedOp;
  readonly #deliver: (owner: Owner, event: E | FeedStatusEvent) => void;
  readonly #kind: FeedKind<E>;
  readonly #feeds = new Map<string, Feed>();
  /** A topic's group removal in flight: a feed starting on it waits, so the removal cannot land after its join. */
  readonly #leaving = new Map<string, Promise<unknown>>();
  /** A topic's `mq.group_create` in flight: quitting waits for it and removes what it created. */
  readonly #joining = new Map<string, Promise<Outcome<OpResponse>>>();
  readonly #waitMs: number;
  readonly #max: number;
  readonly #minBackoff: number;
  readonly #maxBackoff: number;
  readonly #sleep: (ms: number) => Promise<void>;
  readonly #log: (message: string) => void;
  /** Set by `leaveAll`: the app is quitting, and no feed joins again. */
  #closed = false;

  constructor(
    op: FeedOp,
    deliver: (owner: Owner, event: E | FeedStatusEvent) => void,
    kind: FeedKind<E>,
    options: FeedOptions = {},
  ) {
    this.#op = op;
    this.#deliver = deliver;
    this.#kind = kind;
    this.#waitMs = options.waitMs ?? 20_000;
    this.#max = options.max ?? 64;
    this.#minBackoff = options.minBackoffMs ?? 500;
    this.#maxBackoff = options.maxBackoffMs ?? 30_000;
    this.#sleep = options.sleep ?? defaultSleep;
    this.#log = options.log ?? (() => {});
  }

  /** The topics with a feed running, for tests and diagnostics. */
  get topics(): string[] {
    return [...this.#feeds.keys()];
  }

  /**
   * Have `owner` hear `topic`'s messages. Resolves once the app's group is on the topic, so a message
   * sent after that is delivered: read the state *after* this resolves, and nothing falls between
   * the read and the feed.
   */
  subscribe(owner: Owner, topic: unknown): Promise<Outcome<null>> {
    if (!this.#kind.accepts(topic)) {
      return Promise.resolve({ ok: false, error: { code: "bad_request", message: this.#kind.refusal } });
    }
    if (this.#closed) return Promise.resolve({ ok: false, error: { code: "unavailable", message: "the app is quitting" } });
    const running = this.#feeds.get(topic);
    if (running !== undefined) {
      running.owners.add(owner);
      if (running.joined) return Promise.resolve({ ok: true, value: null });
      return new Promise((resolve) => running.waiters.push(resolve));
    }
    // The owner and the waiter are in place before the loop starts: it runs synchronously up to its
    // first call, and a feed with no owner ends there.
    const feed: Feed = { topic, owners: new Set([owner]), waiters: [], joined: false, left: false };
    this.#feeds.set(topic, feed);
    const joined = new Promise<Outcome<null>>((resolve) => feed.waiters.push(resolve));
    void this.#run(feed);
    return joined;
  }

  /** Stop delivering `topic` to `owner`. The feed ends when its last owner goes. */
  unsubscribe(owner: Owner, topic: unknown): void {
    if (typeof topic === "string") this.#feeds.get(topic)?.owners.delete(owner);
  }

  /** Stop delivering anything to `owner` (its window closed or reloaded), or to anyone. */
  closeAll(owner?: Owner): void {
    for (const feed of this.#feeds.values()) {
      if (owner === undefined) feed.owners.clear();
      else feed.owners.delete(owner);
    }
  }

  /**
   * On quit: stop delivering, and remove the app's group from every topic of a kind that `leaves`,
   * waiting at most `timeoutMs` for the daemon. Never rejects; a removal that does not land in time
   * leaves the group to the queue's idle removal, as a crash would.
   */
  async leaveAll(timeoutMs: number): Promise<void> {
    this.#closed = true;
    this.closeAll();
    if (this.#kind.leaves !== true) return;
    // Removals already in flight are waited for too: a feed whose last window just closed is
    // removing its group, and a feed waiting on that removal to rejoin finds no owner after it. A join
    // in flight may yet create the group, so it is waited for and what it created is removed.
    const removals = [
      ...this.#leaving.values(),
      ...[...this.#joining].map(([topic, create]) => {
        const feed = this.#feeds.get(topic);
        if (feed !== undefined) feed.left = true;
        return create.then((created) => (created.ok ? this.#leave(topic) : undefined));
      }),
      ...[...this.#feeds.values()]
        .filter((feed) => feed.joined && !feed.left)
        .map((feed) => {
          feed.left = true;
          return this.#leave(feed.topic);
        }),
    ];
    if (removals.length === 0) return;
    let timer: ReturnType<typeof setTimeout> | undefined;
    await Promise.race([
      Promise.allSettled(removals),
      new Promise<void>((resolve) => {
        timer = setTimeout(resolve, timeoutMs);
      }),
    ]);
    if (timer !== undefined) clearTimeout(timer);
  }

  #leave(topic: string): Promise<unknown> {
    const removal = this.#op({ op: "mq.group_delete", topic, group: APP_GROUP }).then(
      (answer) => {
        if (!answer.ok) this.#log(`could not take the app's group off ${topic}, so it holds the topic until the queue removes it as idle: ${answer.error.message}`);
      },
      (e: unknown) => this.#log(`could not take the app's group off ${topic}: ${e instanceof Error ? e.message : String(e)}`),
    );
    const done = removal.finally(() => {
      if (this.#leaving.get(topic) === done) this.#leaving.delete(topic);
    });
    this.#leaving.set(topic, done);
    return done;
  }

  #emit(feed: Feed, event: E | FeedStatusEvent): void {
    for (const owner of feed.owners) this.#deliver(owner, event);
  }

  #settle(feed: Feed, outcome: Outcome<null>): void {
    const waiters = feed.waiters;
    feed.waiters = [];
    for (const w of waiters) w(outcome);
  }

  async #run(feed: Feed): Promise<void> {
    const { topic } = feed;
    let backoff = this.#minBackoff;
    let rejoin = false;
    let troubled = false;
    const recovered = (): void => {
      backoff = this.#minBackoff;
      if (troubled) this.#emit(feed, { topic, kind: "live" });
      troubled = false;
    };
    const retry = async (message: string): Promise<void> => {
      troubled = true;
      this.#emit(feed, { topic, kind: "error", message });
      await this.#sleep(backoff);
      backoff = Math.min(backoff * 2, this.#maxBackoff);
    };
    for (;;) {
      // Checked and removed in one synchronous step, so a subscribe can never join a feed that has
      // already decided to end.
      if (feed.owners.size === 0) {
        this.#feeds.delete(topic);
        this.#settle(feed, { ok: false, error: { code: "unavailable", message: "unsubscribed" } });
        if (this.#kind.leaves === true && feed.joined && !feed.left) {
          feed.left = true;
          void this.#leave(topic);
        }
        return;
      }
      if (!feed.joined) {
        // A removal of this topic's group still in flight lands first, never after this join.
        const leaving = this.#leaving.get(topic);
        if (leaving !== undefined) {
          await leaving;
          // Its owners may have gone meanwhile (the app quitting): ask again before joining.
          continue;
        }
        const create = this.#op({ op: "mq.group_create", topic, group: APP_GROUP });
        this.#joining.set(topic, create);
        const joined = await create.finally(() => {
          if (this.#joining.get(topic) === create) this.#joining.delete(topic);
        });
        if (!joined.ok) {
          // A topic that does not exist is not transient: nothing this daemon holds sends on it.
          if (joined.error.code === "no_such_topic") {
            this.#feeds.delete(topic);
            feed.owners.clear();
            this.#settle(feed, joined);
            return;
          }
          // Anything else (the daemon down or busy) is waited out; the subscribers keep waiting,
          // and hear why as an error event meanwhile.
          await retry(`joining ${topic}: ${joined.error.message}`);
          continue;
        }
        feed.joined = true;
        // A join that lands after quit was already claimed by `leaveAll`, which removes it.
        if (!this.#closed) feed.left = false;
        this.#settle(feed, { ok: true, value: null });
        if (rejoin) {
          // A gap says everything `live` would, and more: the window re-reads.
          troubled = false;
          backoff = this.#minBackoff;
          this.#emit(feed, { topic, kind: "gap", message: "the app's place on the topic was lost; re-reading" });
        } else {
          recovered();
        }
        rejoin = false;
        continue;
      }
      const polled = await this.#op(
        { op: "mq.poll", topic, group: APP_GROUP, max: this.#max },
        { waitMs: this.#waitMs },
      );
      if (!polled.ok) {
        if (polled.error.code === "no_such_group") {
          // The queue removed the group (idle while the app was away): rejoin from now, and say
          // that anything in between was not delivered.
          feed.joined = false;
          rejoin = true;
          continue;
        }
        await retry(`reading ${topic}: ${polled.error.message}`);
        continue;
      }
      recovered();
      const messages = Array.isArray(polled.value["messages"]) ? (polled.value["messages"] as unknown[]) : [];
      let last: number | undefined;
      for (const m of messages) {
        if (typeof m !== "object" || m === null) continue;
        const { seq, kind, payload } = m as { seq?: unknown; kind?: unknown; payload?: unknown };
        if (typeof seq === "number") last = seq;
        for (const event of this.#kind.events(topic, kind, payload)) this.#emit(feed, event);
      }
      if (last !== undefined) {
        // A failed ack only means the same messages come again, and hearing them twice is a no-op.
        await this.#op({ op: "mq.ack", topic, group: APP_GROUP, seq: last });
      }
    }
  }
}
