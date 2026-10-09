//! The Design tab's list of designs (D53.4), as pure transitions so a refresh is tested without React.
//
// A refresh keeps the designs it already has while it loads, and when it fails: the open design is
// picked from this list, and a list emptied for a moment would close the open design — and, before
// `DesignSession.close`, drop the edits it had not yet sent.

import type { Design } from "@jkb/core";

export interface Listing {
  /** The last list read, or `undefined` before one was. */
  readonly designs: readonly Design[] | undefined;
  readonly loading: boolean;
  /** Why the last read failed, while the list shown is an older one (or none). */
  readonly error: string | undefined;
}

export const INITIAL_LISTING: Listing = { designs: undefined, loading: true, error: undefined };

/** A read started: the list shown stays. */
export const listingLoading = (prev: Listing): Listing => ({ ...prev, loading: true });

/** A read answered. */
export const listingLoaded = (designs: readonly Design[]): Listing => ({ designs, loading: false, error: undefined });

/** A read failed: the list shown stays, and why is said beside it. */
export const listingFailed = (prev: Listing, error: string): Listing => ({ ...prev, loading: false, error });
