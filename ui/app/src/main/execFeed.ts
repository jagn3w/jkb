//! The run-request feed, in the main process (the Executions design): the app's own consumer group
//! on `exec/requests`, the index on which jkb announces every request made and every move, forwarded
//! to each window showing the request queue.
//
// A message says only that request `id` is now in `state`; the window re-reads `exec.list`, so the
// record stays the one answer and a hint heard twice or out of order costs a re-read. The topic is
// reserved (`jkb_core::exec::INDEX_TOPIC`): only jkb sends on it, and only the operator's
// credential — main's — may keep a group there. jkb writes it only while a group reads it, so the
// group is taken off when the last window stops listening and on quit (`leaves`), as for
// `claude/notify`.

import { EXEC_INDEX_TOPIC, execId } from "@jkb/core";

import type { ExecFeedEvent } from "../shared/bridge";
import { TopicFeeds, type FeedKind, type FeedOp, type FeedOptions, type FeedStatusEvent } from "./topicFeeds";

type ExecMessageEvent = Exclude<ExecFeedEvent, FeedStatusEvent>;

/** `exec.changed {id, state}`: the one kind on the index (`jkb_core::exec::INDEX_KIND`). */
export const EXEC_INDEX_KIND = "exec.changed";

export const EXEC_FEED: FeedKind<ExecMessageEvent> = {
  accepts: (topic: unknown): topic is string => topic === EXEC_INDEX_TOPIC,
  refusal: `only ${EXEC_INDEX_TOPIC} is the run-request index`,
  leaves: true,
  events(topic, kind, payload) {
    if (kind !== EXEC_INDEX_KIND || typeof payload !== "object" || payload === null) return [];
    const { id, state } = payload as { id?: unknown; state?: unknown };
    const known = execId(id);
    if (known === undefined || typeof state !== "string") return [];
    return [{ topic, kind: "changed", id: known, state }];
  },
};

/** The `exec/requests` feed for every window that asked. */
export class ExecFeed extends TopicFeeds<ExecMessageEvent> {
  constructor(op: FeedOp, deliver: (owner: number, event: ExecFeedEvent) => void, options: FeedOptions = {}) {
    super(op, deliver, EXEC_FEED, options);
  }

  join(owner: number): ReturnType<TopicFeeds<ExecMessageEvent>["subscribe"]> {
    return this.subscribe(owner, EXEC_INDEX_TOPIC);
  }

  leave(owner: number): void {
    this.unsubscribe(owner, EXEC_INDEX_TOPIC);
  }
}
