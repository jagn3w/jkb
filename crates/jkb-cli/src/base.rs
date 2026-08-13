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

/// Whether `value` is a full object id — the only form a cut point may be **stored** in.
///
/// A cut point has to mean the same commit in every clone, and only a full object id does.
/// Symbolic revisions resolve *everywhere* and to something different in each repository: storing
/// the literal `HEAD` and re-resolving it later yields whatever that repo is pointed at now, which
/// is precisely the unrelated commit that makes `is_merged` skip its freshly-cut guard and close a
/// task whose work never landed. A *foreign* full sha is harmless by comparison — it simply does
/// not resolve in the task's repo, so the readers decline to act. The dangerous values are the
/// ones that always resolve.
///
/// 40 hex digits for sha-1, 64 for a sha-256 repository.
pub(crate) fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|c| c.is_ascii_hexdigit())
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

/// Record the cut point for `branch` **if one is not already recorded for it** — the one writer,
/// and now the one **measurer**.
///
/// It used to take the value from the caller, and three callers grew three theories of what a cut
/// point is: trunk's tip, then the merge-base with trunk, then the land target's tip. Each was
/// wrong in a different situation, and the last one was wrong in this project's own primary flow —
/// a task branch cut from a *staging* branch is ahead of its merge-base with trunk before any work
/// happens, so the freshly-cut guard never fired and an empty task closed as merged.
///
/// So there is nothing left to pass. The value is the branch's **own tip, right now**, which is by
/// construction the answer to the only question the readers ask of it: *has anything happened on
/// this branch since we started tracking it?* An existing record always wins, because that
/// question is about the moment tracking began and no later run can re-derive it.
///
/// The failure mode is uniform and safe. A branch that already carried commits when tracking began
/// records `base == tip`, so it reads as "nothing to merge" and is held rather than closed — a
/// missed auto-close, which costs one command, never a false one, which buries work (D34.4).
///
/// This runs `git` inside the write transaction, which holds the writer thread for the length of
/// one `rev-parse`. That is the price of the caller being unable to supply a value at all, and it
/// is worth it: every defect this module has had was a caller computing the wrong one.
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
    repo_root: &std::path::Path,
    branch: &str,
) -> jkb_core::Result<()> {
    record_if_absent(
        conn,
        meta,
        id,
        branch,
        measure(repo_root, branch)?.as_deref(),
    )
}

/// The decision half of [`ensure_recorded`]: given a candidate, decide whether to record it.
///
/// Private, and split out only so the four adoption states can be unit-tested without a git repo.
/// Callers outside this module reach [`ensure_recorded`], which measures — the candidate is not
/// something any of them may choose.
fn record_if_absent(
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
    // Only an object id may be adopted. A bare value that is not one arrived by a route that
    // never validated it — `jkb task add "… #base=HEAD"` reaches `tag::apply` directly — and
    // promoting it to this branch's qualified cut point would launder an unchecked string into the
    // record every landing decision reads.
    let adopted = if others_exist {
        None
    } else {
        bare.iter().map(String::as_str).find(|v| is_object_id(v))
    };

    // An attributable pre-qualification value wins over the measurement: it is the real cut
    // point, and a tip measured now is not.
    if let Some(sha) = adopted.or(cut) {
        write(conn, meta, id, branch, sha)?;
    }
    for stale in &bare {
        tag::remove(conn, meta, id, FACET, stale)?;
    }
    Ok(())
}

/// The cut point now recorded for `branch`, if any — so a command can report it rather than
/// leaving a later reader to decline silently.
///
/// Returns the value, not a boolean: the human path only needs to know whether there is one, but
/// the JSON path should carry what was recorded, and two accessors would let those two answers
/// drift.
///
/// # Errors
/// Returns an error if the tags cannot be read.
pub(crate) fn recorded_for(
    db: &jkb_core::Db,
    id: ItemId,
    branch: &str,
) -> anyhow::Result<Option<String>> {
    Ok(resolve(&crate::repo::task_tags(db, id)?, branch).map(str::to_owned))
}

/// The branch's own tip, or `None` if this repo does not have the branch.
///
/// Resolved through `branch_ref` so a branch that exists only on the remote still answers, and
/// through `rev_commit` so the result is a real commit rather than something `rev-parse` merely
/// managed to parse. `None` records nothing, and both readers then decline to act.
fn measure(repo_root: &std::path::Path, branch: &str) -> jkb_core::Result<Option<String>> {
    let resolved = crate::gitrepo::branch_ref(repo_root, branch, crate::gitrepo::Prefer::Local)
        .and_then(|r| match r {
            Some(reference) => crate::gitrepo::rev_commit(repo_root, &reference),
            None => Ok(None),
        });
    resolved.map_err(|e| jkb_types::Error::Validation(e.to_string()).into())
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
    // The form check lives HERE rather than at the CLI verb, because the verb is not the only way
    // in: `ensure_recorded` writes too, and a caller-side rule is one more thing every present and
    // future writer has to remember — the failure this module exists to end.
    if !is_object_id(sha) {
        return Err(jkb_types::Error::Validation(format!(
            "`{sha}` is not a full commit id, and a cut point must be one: a symbolic revision \
             resolves to a different commit in every clone, so recording one silently closes the \
             task against work it never named. Pass the full 40-character id."
        ))
        .into());
    }
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
    use super::{record_if_absent, resolve, write, FACET};
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
            record_if_absent(conn, meta, id, &branch, cut.as_deref())
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
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET, "task/a:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ]);
        assert_eq!(
            resolve(&t, "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    /// The regression that closed a live task: lending one branch's cut point to another disables
    /// `is_merged`'s "freshly cut, nothing on it yet" guard for the branch that borrowed it.
    #[test]
    fn a_base_is_never_lent_to_a_branch_it_was_not_cut_for() {
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET_BRANCH, "task/b"),
            (FACET, "task/a:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ]);
        assert_eq!(
            resolve(&t, "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            resolve(&t, "task/b"),
            None,
            "task/b was handed task/a's cut point"
        );
    }

    #[test]
    fn a_bare_legacy_base_applies_to_a_lone_branch() {
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
        ]);
        assert_eq!(
            resolve(&t, "task/a"),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
    }

    #[test]
    fn a_bare_legacy_base_is_refused_when_several_branches_exist() {
        let t = tags(&[
            (FACET_BRANCH, "task/a"),
            (FACET_BRANCH, "task/b"),
            (FACET, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
        ]);
        assert_eq!(
            resolve(&t, "task/a"),
            None,
            "an unattributable legacy base was applied to one of several branches"
        );
    }

    // ---- the writer's four states ----

    #[test]
    fn a_missing_base_is_recorded_from_the_cut() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a")]);
        ensure(
            &db,
            id,
            "task/a",
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        );
        assert_eq!(
            resolve(&task_tags(&db, id).unwrap(), "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    /// The must-fix `task work` kept re-introducing: a later run must never recompute a cut point
    /// the branch has long since moved past.
    #[test]
    fn an_existing_base_is_never_overwritten() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET, "task/a:4444444444444444444444444444444444444444"),
            ],
        );
        ensure(
            &db,
            id,
            "task/a",
            Some("2222222222222222222222222222222222222222"),
        );
        assert_eq!(
            resolve(&task_tags(&db, id).unwrap(), "task/a"),
            Some("4444444444444444444444444444444444444444"),
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
                (FACET, "task/a:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ],
        );
        ensure(
            &db,
            id,
            "task/b",
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        );
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "task/a's base was lost"
        );
        assert_eq!(
            resolve(&t, "task/b"),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    /// The pass-22 concern. A bare value is the branch's *real* cut point; the caller's `cut` is
    /// today's trunk tip. Substituting the guess for the fact broke the empty-branch guard.
    #[test]
    fn a_bare_legacy_base_is_adopted_rather_than_replaced() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(
            &db,
            &[
                (FACET_BRANCH, "task/a"),
                (FACET, "1111111111111111111111111111111111111111"),
            ],
        );
        ensure(
            &db,
            id,
            "task/a",
            Some("2222222222222222222222222222222222222222"),
        );
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            Some("1111111111111111111111111111111111111111"),
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
                (FACET, "3333333333333333333333333333333333333333"),
            ],
        );
        ensure(
            &db,
            id,
            "task/b",
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        );
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/b"),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
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
                (FACET, "task/a:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                (FACET, "task/b:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ],
        );
        db.write_txn("t", move |conn, meta| {
            write(
                conn,
                meta,
                id,
                "task/b",
                "cccccccccccccccccccccccccccccccccccccccc",
            )
        })
        .unwrap();
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "a sibling was clobbered"
        );
        assert_eq!(
            resolve(&t, "task/b"),
            Some("cccccccccccccccccccccccccccccccccccccccc")
        );
        assert_eq!(
            super::facet_values(&t, FACET).len(),
            2,
            "a stale entry survived"
        );
    }

    /// A symbolic revision must never be stored. It resolves in every clone, to whatever that
    /// repository is pointed at now, so `is_merged` compares the branch tip against an unrelated
    /// commit, skips its freshly-cut guard, and closes a task whose work never landed. A foreign
    /// *object id* is harmless by comparison — it simply does not resolve, and the readers decline
    /// to act — so form, not reachability, is what has to be refused at the write.
    #[test]
    fn a_symbolic_revision_is_never_stored_as_a_cut_point() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a")]);
        for symbolic in ["HEAD", "main", "@", "origin/main", "1111111"] {
            let sym = symbolic.to_owned();
            let err = db
                .write_txn("t", move |conn, meta| write(conn, meta, id, "task/a", &sym))
                .expect_err("a symbolic revision must be refused, not recorded");
            assert!(
                err.to_string().contains("full commit id"),
                "the refusal must say why: {err}"
            );
        }
        assert_eq!(resolve(&task_tags(&db, id).unwrap(), "task/a"), None);
    }

    /// And it is not laundered in through adoption either — `jkb task add "… #base=HEAD"` reaches
    /// `tag::apply` without passing any check, so the bare value must be dropped rather than
    /// promoted to this branch's qualified cut point.
    #[test]
    fn a_bare_value_that_is_not_an_object_id_is_dropped_not_adopted() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a"), (FACET, "HEAD")]);
        ensure(&db, id, "task/a", None);
        let t = task_tags(&db, id).unwrap();
        assert_eq!(
            resolve(&t, "task/a"),
            None,
            "`HEAD` was adopted as a cut point"
        );
        assert!(
            super::facet_values(&t, FACET).is_empty(),
            "the unusable bare value was left where a later run could adopt it"
        );
    }

    /// With no cut available and nothing on record, nothing is written — and the readers then
    /// refuse to act, which is the safe direction.
    #[test]
    fn no_cut_and_no_record_writes_nothing() {
        let db = Db::open_in_memory().unwrap();
        let id = a_task(&db, &[(FACET_BRANCH, "task/a")]);
        ensure(&db, id, "task/a", None);
        assert_eq!(resolve(&task_tags(&db, id).unwrap(), "task/a"), None);
    }
}
