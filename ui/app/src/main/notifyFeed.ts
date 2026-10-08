//! The needs-input feed, in the main process (D53.9): the app's own consumer group on
//! `claude/notify`, the stream the macOS notifier reads, forwarded to every window that asked.
//
// One topic, so one feed, shared by every window. A message only says that a session's notification
// moved; the window re-reads `notify.open_sessions` for what it became, so the record — where the
// state is derived (`jkb_core::notify`) — stays the one answer, and a message heard twice or out of
// order costs a re-read and nothing else.
//
// What the app's group changes on the daemon: `claude/notify` is written to only while it has a
// group (`notify::observe`), so with the app open a machine without a notifier now has its posts and
// withdrawals sent — read here, and reaped like any consumed message. Closed, the group goes idle and
// the queue removes it; reopened, the feed joins from now and re-reads the records, which hold
// everything still on screen.

import { NOTIFY_TOPIC, parseNotifyMessage } from "@jkb/core";

import type { NotifyFeedEvent } from "../shared/bridge";
import { TopicFeeds, type FeedKind, type FeedOp, type FeedOptions, type FeedStatusEvent } from "./topicFeeds";

type NotifyMessageEvent = Exclude<NotifyFeedEvent, FeedStatusEvent>;

const NOTIFY_FEED: FeedKind<NotifyMessageEvent> = {
  accepts: (topic: unknown): topic is string => topic === NOTIFY_TOPIC,
  refusal: `only ${NOTIFY_TOPIC} is a notification topic`,
  events(topic, kind, payload) {
    const moved = parseNotifyMessage(kind, payload);
    return moved === undefined ? [] : [{ topic, kind: "changed", session: moved.session }];
  },
};

/** The `claude/notify` feed for every window that asked. */
export class NotifyFeed extends TopicFeeds<NotifyMessageEvent> {
  constructor(op: FeedOp, deliver: (owner: number, event: NotifyFeedEvent) => void, options: FeedOptions = {}) {
    super(op, deliver, NOTIFY_FEED, options);
  }

  /** Have `owner` hear `claude/notify`. Resolves once the app's group is on it. */
  join(owner: number): ReturnType<TopicFeeds<NotifyMessageEvent>["subscribe"]> {
    return this.subscribe(owner, NOTIFY_TOPIC);
  }

  leave(owner: number): void {
    this.unsubscribe(owner, NOTIFY_TOPIC);
  }
}
