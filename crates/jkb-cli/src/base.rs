//! The commit a branch was cut from — the **one** module that knows how it is stored.
//!
//! ## Why this module exists
//!
//! "Branch X was cut from commit Y" is a fact about a *branch*, but the store keys tags by
//! `(item, facet)`, so the branch has to be encoded into the value: `base=<branch>:<sha>`. That
//! encoding leaked to roughly a dozen call sites, and four consecutive review passes each found a
//! different one holding a different theory of it:
//!
//! - `jkb task work` asked `resumed` — whether a *worktree* existed — and so overwrote a real cut
//!   point with the land target's current tip whenever a branch was re-worked after `abandon`.
//! - `jkb task start` treated a pre-qualification bare value as "not recorded", wrote today's
//!   trunk tip over it, and lost the actual cut point.
//! - `jkb task tag set base=…` went through the generic single-value setter, which clears the
//!   facet's other values — so recording one branch's base deleted every sibling's.
//!
//! None of those were reader bugs: given a correctly recorded base, both readers
//! (`task close-merged` and `task review record`) have always agreed. They were three writers
//! each answering *"is a base already recorded for this branch?"* from a different proxy.
//!
//! So that question has exactly one implementation here ([`ensure_recorded`]), the reader's
//! question has exactly one ([`resolve`]), they sit next to each other so the deliberate
//! asymmetry between them is visible in one screen, and [`FACET`] is **private**: no other module
//! can spell the facet, format a `<branch>:<sha>` value, or take one apart.
//!
//! ## Why a git ref was rejected
//!
//! `refs/jkb/base/<branch>` would make per-branch keying structural and put the fact where
//! branches live. It was rejected deliberately: jkb runs inside other people's professional
//! repositories, and decorating them with refs the user never asked for is a side effect the tool
//! has no licence to take. Coordination stays in the store; git is used for branches and commits.

use std::collections::BTreeMap;

use jkb_core::{tag, WriteMeta};
use jkb_types::ItemId;
use rusqlite::Connection;

use crate::repo::{facet_values, FACET_BRANCH};

/// The facet the cut point is stored under. **Private on purpose** — see the module docs.
const FACET: &str = "base";

/// The verb that writes a cut point, named in every refusal so the remedy is always reachable.
pub(crate) const VERB: &str = "jkb task base <uid> <branch> <sha>";

/// Whether `facet` is this module's, and therefore off-limits to the generic tag commands.
///
/// `jkb task tag set base=…` is not merely discouraged: [`crate::repo::set_facet`] clears the
/// facet's other values, and for this facet those are *other branches' cut points*. The remedy
/// string in an earlier error message named that exact command, and `/task-swarm` was changed to
/// run it — so the tool was instructing its users to destroy the records it then refused to act
/// without.
pub(crate) fn is_reserved_facet(facet: &str) -> bool {
    facet == FACET
}

/// The cut point recorded for `branch`, as a **reader** should resolve it.
///
/// Two forms are accepted:
///
/// 1. `<branch>:<sha>` — qualified, the only form written since. Matched exactly, so a base is
///    never lent to a branch it was not cut for. Git forbids `:` in a ref name, so splitting on
///    the first one is unambiguous.
/// 2. A bare `<sha>` — written before bases were qualified. Honoured **only** when the task
///    records at most one branch, which is the case it was written for and the only case it can
///    be attributed to. With several branches nothing says which one it describes, and guessing
///    is what closes a task whose work is still in flight.
///
/// `None` means "no cut point is recorded for this branch" — nothing more. The decision of what
/// to *do* with that lives in [`crate::repo::landed_with_base`], which refuses to act, so this
/// function never has to be read as an opinion.
pub(crate) fn resolve<'a>(
    tags: &'a BTreeMap<String, Vec<String>>,
    branch: &str,
) -> Option<&'a str> {
    let values = facet_values(tags, FACET);
    let prefix = format!("{branch}:");
    if let Some(sha) = values.iter().find_map(|v| v.strip_prefix(&prefix)) {
        return Some(sha);
    }
    if facet_values(tags, FACET_BRANCH).len() <= 1 {
        return values.iter().map(String::as_str).find(|v| !v.contains(':'));
    }
    None
}

/// Whether this task records any cut point at all — the fact that separates "this branch has not
/// landed" from "we cannot tell", which the review report shows differently.
pub(crate) fn any_recorded(tags: &BTreeMap<String, Vec<String>>) -> bool {
    !facet_values(tags, FACET).is_empty()
}

/// Record the cut point for `branch` **if one is not already recorded for it** — the one writer.
///
/// `cut` is where the caller believes the branch began, consulted only when nothing is on record:
/// `task start` passes trunk's tip, `task work` passes the tip of the branch the session hangs
/// off. Both are guesses that are only correct at the moment the branch is created, which is why
/// an existing record always wins — the whole class of bug this replaces was a later run
/// recomputing a cut point from a commit the branch had long since moved past.
///
/// A bare pre-qualification value is **adopted** (re-written as `<branch>:<sha>`) when this task
/// records no branch other than `branch`, since then it can only have been cut for this one. That
/// is strictly better than the previous behaviour of discarding it and substituting today's trunk
/// tip: the bare value is the real cut point, and today's tip is not.
///
/// Any bare value that is *not* adopted is removed. Leaving it would put the task back in the
/// single-branch case above the moment the branch facet is rewritten, and the reader would then
/// lend one branch's cut point to another — which is what disables the "freshly cut, nothing on
/// it yet" guard and closes a task with its work still uncommitted.
///
/// **Attribution reads the branch facet as it stands right now**, which is why
/// [`crate::repo::set_location_facets`] calls this *before* it rewrites `branch=` and is the only
/// caller. Run afterwards, the task always names exactly the branch being recorded, every bare
/// value looks attributable, and a cut point from the branch the task was on last week is adopted
/// for the one it is on today. Sequencing that correctly is not something two call sites should
/// each have to remember — this whole module exists because that kind of remembering failed four
/// times running.
///
/// # Errors
/// Returns an error if a tag read or write fails.
pub(crate) fn ensure_recorded(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    branch: &str,
    cut: Option<&str>,
) -> jkb_core::Result<()> {
    let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (facet, value) in tag::applications(conn, id)? {
        tags.entry(facet).or_default().push(value);
    }
    let tags = &tags;
    let values = facet_values(tags, FACET);
    let prefix = format!("{branch}:");
    if values.iter().any(|v| v.starts_with(&prefix)) {
        return Ok(());
    }

    let bare: Vec<String> = values
        .iter()
        .filter(|v| !v.contains(':'))
        .cloned()
        .collect();
    let others_exist = facet_values(tags, FACET_BRANCH)
        .iter()
        .any(|b| b.as_str() != branch);
    let adopted = if others_exist {
        None
    } else {
        bare.first().map(String::as_str)
    };

    if let Some(sha) = adopted.or(cut) {
        write(conn, meta, id, branch, sha)?;
    }
    for stale in &bare {
        tag::remove(conn, meta, id, FACET, stale)?;
    }
    Ok(())
}

/// Record the cut point for `branch`, replacing whatever this branch had — `jkb task base`.
///
/// Deliberately **not** [`crate::repo::set_facet`]: that clears the facet's other values, which
/// here are other branches' cut points. Only the entry whose `<branch>:` prefix matches is
/// replaced. `tag::apply` alone is wrong too — it is additive, so re-recording would accumulate
/// stale entries and [`resolve`] would return whichever came first.
///
/// # Errors
/// Returns an error if a tag write fails.
pub(crate) fn write(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    branch: &str,
    sha: &str,
) -> jkb_core::Result<()> {
    let qualified = format!("{branch}:{sha}");
    let prefix = format!("{branch}:");
    for (f, v) in tag::applications(conn, id)? {
        if f == FACET && v != qualified && v.starts_with(&prefix) {
            tag::remove(conn, meta, id, &f, &v)?;
        }
    }
    tag::apply(conn, meta, id, FACET, &qualified)
}

#[cfg(test)]
mod tests {
    use super::{any_recorded, resolve, write, FACET};
    use crate::repo::{task_tags, FACET_BRANCH};
    use jkb_core::{item::NewItem, tag, Db};
    use jkb_types::ItemId;
    use std::collections::BTreeMap;

    fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (f, v) in pairs {
            out.entry((*f).to_owned())
                .or_default()
                .push((*v).to_owned());
        }
        out
    }

    fn a_task(db: &Db, pairs: &[(&str, &str)]) -> ItemId {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(f, v)| ((*f).to_owned(), (*v).to_owned()))
            .collect();
        db.write_txn("t", move |conn, meta| {
            let id = jkb_core::item::upsert(
                conn,
                meta,
                &NewItem {
                    uid: "task:t".to_owned(),
                    kind: "task".to_owned(),
                    content: None,
                    content_hash: None,
                    mime: None,
                },
            )?;
            for (f, v) in &pairs {
                tag::apply(conn, meta, id, f, v)?;
            }
            Ok(id)
        })
        .unwrap()
    }

    /// Record a cut point the way both writers do — through `set_location_facets`, which is the
    /// only caller and which sequences this before it rewrites `branch=`.
    fn ensure(db: &Db, id: ItemId, branch: &str, cut: Option<&str>) {
        let (branch, cut) = (branch.to_owned(), cut.map(str::to_owned));
        db.write_txn("t", move |conn, meta| {
            crate::repo::set_location_facets(
                conn,
                meta,
                id,
                &crate::repo::Location {
                    branch: Some(&branch),
                    cut_from: cut.as_deref(),
                    ..crate::repo::Location::default()
                },
            )
        })
        .unwrap();
    }

    // ---- the reader's four states (see the module docs) ----

    #[test]
    fn no_base_recorded_resolves_to_none() {
        assert_eq!(resolve(&tags(&[(FACET_BRANCH, "task/a")]), "task/a"), None);
    }

    #[test]
    fn a_qualified_base_resolves_for_its_own_branch() {
        let t = tags(&[(FACET_BRANCH, "task/a"), (FACET, "task/a:aaa")]);
        assert_eq!(resolve(&t, "task/a"), Some("aaa"));
    }

    /// The regression that closed a live task: lending one branch's cut point to another disables
    /// `is_merged`'s "freshly cut, nothing on it yet" guard for the branch that borrowed it.
    #[test]
    fn a_base_is_never_lent_to_a_branch_it_was_not_cut_for() {
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET_BRANCH, "task/b"),
            (FACET, "task/a:aaa"),
        ]);
        assert_eq!(resolve(&t, "task/a"), Some("aaa"));
        assert_eq!(
            resolve(&t, "task/b"),
            None,
            "task/b was handed task/a's cut point"
        );
    }

    #[test]
    fn a_bare_legacy_base_applies_to_a_lone_branch() {
        let t = tags(&[(FACET_BRANCH, "task/a"), (FACET, "deadbeef")]);
        assert_eq!(resolve(&t, "task/a"), Some("deadbeef"));
    }

    #[test]
    fn a_bare_legacy_base_is_refused_when_several_branches_exist() {
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET_BRANCH, "task/b"),
            (FACET, "deadbeef"),
        ]);
        assert_eq!(
            resolve(&t, "task/a"),
            None,
            "an unattributable legacy base was applied to one of several branches"
        );
    }

    #[test]
    fn any_recorded_sees_both_forms_and_neither() {
        assert!(!any_recorded(&tags(&[(FACET_BRANCH, "task/a")])));
        assert!(any_recorded(&tags(&[(FACET, "deadbeef")])));
        assert!(any_recorded(&tags(&[(FACET, "task/a:aaa")])));
    }

    // ---- the writer's four states ----

    #[test]
    fn a_missing_base_is_recorded_from_the_cut() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a")]);
        ensure(&db, id, "task/a", Some("aaa"));
        assert_eq!(resolve(&task_tags(&db, id).unwrap(), "task/a"), Some("aaa"));
    }

    /// The must-fix `task work` kept re-introducing: a later run must never recompute a cut point
    /// the branch has long since moved past.
    #[test]
    fn an_existing_base_is_never_overwritten() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a"), (FACET, "task/a:original")]);
        ensure(&db, id, "task/a", Some("todays-tip"));
        assert_eq!(
            resolve(&task_tags(&db, id).unwrap(), "task/a"),
            Some("original"),
            "a live branch's cut point was replaced with the land target's current tip"
        );
    }

    /// A base recorded for a *sibling* says nothing about this branch, so this branch must still
    /// get one — treating the task as "already recorded" left the new branch with none at all.
    #[test]
    fn a_siblings_base_does_not_count_as_recorded_for_this_branch() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET_BRANCH, "task/b"),
                (FACET, "task/a:aaa"),
            ],
        );
        ensure(&db, id, "task/b", Some("bbb"));
        let t = task_tags(&db, id).unwrap();
        assert_eq!(resolve(&t, "task/a"), Some("aaa"), "task/a's base was lost");
        assert_eq!(resolve(&t, "task/b"), Some("bbb"));
    }

    /// The pass-22 concern. A bare value is the branch's *real* cut point; the caller's `cut` is
    /// today's trunk tip. Substituting the guess for the fact broke the empty-branch guard.
    #[test]
    fn a_bare_legacy_base_is_adopted_rather_than_replaced() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a"), (FACET, "real-cut-point")]);
        ensure(&db, id, "task/a", Some("todays-tip"));
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            Some("real-cut-point"),
            "the actual cut point was discarded in favour of today's tip"
        );
        assert_eq!(
            super::facet_values(&t, FACET).len(),
            1,
            "the bare value survived alongside its qualified form"
        );
    }

    /// But a bare value cannot be attributed once the task names another branch, and leaving it
    /// there would let the reader lend it out as soon as the branch facet is rewritten.
    #[test]
    fn an_unattributable_bare_base_is_dropped_not_adopted() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET_BRANCH, "task/b"),
                (FACET, "unattributable"),
            ],
        );
        ensure(&db, id, "task/b", Some("bbb"));
        let t = task_tags(&db, id).unwrap();
        assert_eq!(resolve(&t, "task/b"), Some("bbb"));
        assert_eq!(
            resolve(&t, "task/a"),
            None,
            "an unattributable legacy value was left where it could be lent out"
        );
    }

    /// The explicit verb replaces one branch's record and leaves the others alone. `set_facet`
    /// here — the command the old error message recommended — deleted every sibling's.
    #[test]
    fn the_explicit_verb_replaces_only_its_own_branch() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET_BRANCH, "task/b"),
                (FACET, "task/a:aaa"),
                (FACET, "task/b:bbb"),
            ],
        );
        db.write_txn("t", move |conn, meta| {
            write(conn, meta, id, "task/b", "new")
        })
        .unwrap();
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            Some("aaa"),
            "a sibling was clobbered"
        );
        assert_eq!(resolve(&t, "task/b"), Some("new"));
        assert_eq!(
            super::facet_values(&t, FACET).len(),
            2,
            "a stale entry survived"
        );
    }

    /// With no cut available and nothing on record, nothing is written — and the readers then
    /// refuse to act, which is the safe direction.
    #[test]
    fn no_cut_and_no_record_writes_nothing() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a")]);
        ensure(&db, id, "task/a", None);
        assert!(!any_recorded(&task_tags(&db, id).unwrap()));
    }
}
