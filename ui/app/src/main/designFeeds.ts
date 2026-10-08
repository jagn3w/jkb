//! Live design updates, in the main process (D53.1, D53.4): one long-poll per open design's
//! `design/<uid>` topic, shared by every window showing it, forwarded to those windows. The loop is
//! `topicFeeds.ts`'s; this says which topics are designs and what their messages mean.

import { isDesignTopic, parseAnnouncement, parsePromptAnnouncement } from "@jkb/core";

import type { DesignFeedEvent } from "../shared/bridge";
import { APP_GROUP, TopicFeeds, type FeedKind, type FeedOp, type FeedOptions, type FeedStatusEvent } from "./topicFeeds";

export type { FeedOp } from "./topicFeeds";

/** The consumer group the app reads design topics with: the app's one group. */
export const DESIGN_GROUP = APP_GROUP;

export type DesignFeedsOptions = FeedOptions;

type DesignMessageEvent = Exclude<DesignFeedEvent, FeedStatusEvent>;

/** Design topics: an `update` merges, a `prompt` re-reads the Prompts pane (D53.6). */
const DESIGN_FEED: FeedKind<DesignMessageEvent> = {
  accepts: isDesignTopic,
  refusal: "not a design topic",
  events(topic, kind, payload) {
    const announced = kind === "update" ? parseAnnouncement(payload) : undefined;
    if (announced !== undefined) return [{ topic, kind: "update", ...announced }];
    const prompt = kind === "prompt" ? parsePromptAnnouncement(payload) : undefined;
    if (prompt !== undefined) return [{ topic, kind: "prompt", ...prompt }];
    return [];
  },
};

/** The live-update feeds for every window's open designs. */
export class DesignFeeds extends TopicFeeds<DesignMessageEvent> {
  constructor(op: FeedOp, deliver: (owner: number, event: DesignFeedEvent) => void, options: DesignFeedsOptions = {}) {
    super(op, deliver, DESIGN_FEED, options);
  }
}
