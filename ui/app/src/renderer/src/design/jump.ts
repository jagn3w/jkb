//! *Jump to context*'s arrival on the Design tab (D53.9): what a request to open a design does,
//! given the listing as it stands. Pure, so the rule is tested without a window.

import { repoOf } from "@jkb/core";

import type { Listing } from "./listing";

export type Jump =
  /** Not yet: the listing is still loading. */
  | { readonly kind: "wait" }
  /** Open `uid` in `repo`'s designs. */
  | { readonly kind: "open"; readonly repo: string; readonly uid: string }
  /** Read the listing again once (the design may be newer than it). */
  | { readonly kind: "reload" }
  /** The request is served by saying why it cannot be. */
  | { readonly kind: "refused"; readonly notice: string };

/**
 * What a request for design `uid` does. A design is opened only in its own repo's list: one outside
 * every `designs/<repo>` is refused, never answered by the active repo's first design. A listing
 * that failed is said to have failed, not taken as the design's absence.
 */
export function jumpTo(listing: Listing, uid: string, reloaded: boolean): Jump {
  if (listing.loading) return { kind: "wait" };
  const found = listing.designs?.find((d) => d.uid === uid);
  if (found !== undefined) {
    const repo = repoOf(found.namespace);
    return repo === undefined
      ? { kind: "refused", notice: `${uid} is not under designs/<repo>, so this tab cannot open it.` }
      : { kind: "open", repo, uid };
  }
  if (!reloaded) return { kind: "reload" };
  return {
    kind: "refused",
    notice:
      listing.error !== undefined
        ? `Cannot open ${uid}: the designs could not be listed (${listing.error}).`
        : `${uid} is not a design this daemon lists.`,
  };
}
