//! Agent-claim state (design D27.1): an orthogonal, liveness-checkable hold on a
//! task, stored in its **own** `items` columns (`claimant_id`, `claimed_at`) — never
//! encoded in the `status` string.
//!
//! A claim answers *"is anyone holding this task right now?"* while `status` answers
//! *"how far along is the work?"* — two different questions, so two different fields.
//! [`claim`] is a **compare-and-swap** that succeeds only if the slot is free or held
//! by the same owner, and in the same statement advances `status` `open`→`in_progress`
//! so there is never a claimed-but-`open` window. [`release`] clears the claim (leaving
//! `status` to the lifecycle). [`reclaim_dead`] is the crash-recovery net: it NULLs
//! only claims whose owner is **not** in the caller's verified-alive set, writing only
//! the claim columns — so it can never clash with a status transition, and a live run
//! never reclaims its own in-flight work.
//!
//! Liveness is by **owner-existence**, never by a claim's age: there is no TTL and no
//! agent heartbeat, precisely so a paused-but-alive agent (e.g. blocked on a permission
//! prompt) is never reclaimed. The `claimant_id` is a liveness-checkable owner id
//! (`host:pid`+run); the probe (`kill -0`) lives at the CLI/coordinator edge — this
//! module only records who holds what. All three seams are **changelogged** (op
//! `claim`/`release`/`reclaim`) for audit; they are not auto-reverted by undo (which
//! inverts only `insert` ops).

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{Error as TypeError, ItemId};

use crate::store::WriteMeta;
use crate::{changelog, Error, Result};

/// A claim held on a task: the owning identity and when it was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimInfo {
    /// The claimed item.
    pub id: ItemId,
    /// The task's stable uid.
    pub uid: String,
    /// The liveness-checkable owner id (`host:pid`+run).
    pub owner: String,
    /// When the claim was taken (ISO-8601), if recorded.
    pub claimed_at: Option<String>,
}

/// The claim/status columns of one item, read before a mutation for the audit trail.
struct Before {
    status: Option<String>,
    claimant_id: Option<String>,
    claimed_at: Option<String>,
}

/// Read an item's `status`/`claimant_id`/`claimed_at`, or [`None`] if it does not exist.
fn read_before(conn: &Connection, item: ItemId) -> Result<Option<Before>> {
    Ok(conn
        .prepare_cached("SELECT status, claimant_id, claimed_at FROM items WHERE id = ?1")?
        .query_row([item.get()], |row| {
            Ok(Before {
                status: row.get(0)?,
                claimant_id: row.get(1)?,
                claimed_at: row.get(2)?,
            })
        })
        .optional()?)
}

/// Acquire `item`'s claim for `owner`, atomically starting the task.
///
/// A single **compare-and-swap** `UPDATE` that succeeds only if the task is currently
/// unclaimed (`claimant_id IS NULL`) or already held by the same `owner` (idempotent
/// re-claim), and in the same statement advances `status` to `in_progress` — so there
/// is never a claimed-but-`open` intermediate state. Returns `Ok(true)` if the claim
/// was acquired (the task is now claimed by `owner` and `in_progress`), `Ok(false)` if
/// a claim by a **different**, still-recorded owner blocks it.
///
/// A **terminal** task (`done`/`cancelled`) can never be claimed: the CAS would
/// otherwise flip a completed task back to `in_progress` (a claimant slot on a terminal
/// task is `NULL`, so it would look free). Both a pre-check (for a clear error) and the
/// `status NOT IN (...)` guard on the CAS reject it.
///
/// Recorded in the changelog (op `claim`) for audit when it succeeds.
///
/// # Errors
/// Returns [`jkb_types::Error::NotFound`] if `item` does not exist,
/// [`jkb_types::Error::Validation`] if the task is already `done`/`cancelled`; otherwise
/// a database error.
pub fn claim(conn: &Connection, meta: &WriteMeta, item: ItemId, owner: &str) -> Result<bool> {
    let before = read_before(conn, item)?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("task {item}"))))?;
    // A finished task must not be resurrected by a claim (design D27.7: `done`/`cancelled`
    // are terminal). Refuse with a clear error rather than silently re-opening it.
    if matches!(before.status.as_deref(), Some("done" | "cancelled")) {
        let status = before.status.as_deref().unwrap_or("terminal");
        return Err(Error::Types(TypeError::Validation(format!(
            "cannot claim task {item}: it is already {status}"
        ))));
    }
    let changed = conn
        .prepare_cached(
            "UPDATE items
                SET claimant_id = ?2,
                    claimed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    status = 'in_progress',
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1
                AND status NOT IN ('done', 'cancelled')
                AND (claimant_id IS NULL OR claimant_id = ?2)",
        )?
        .execute(params![item.get(), owner])?;
    if changed == 0 {
        // Held by a different, still-recorded owner — refuse (no state changed).
        return Ok(false);
    }
    // Read back the values the CAS just wrote so the audit trail is exact.
    let claimed_at: Option<String> = conn
        .prepare_cached("SELECT claimed_at FROM items WHERE id = ?1")?
        .query_row([item.get()], |row| row.get(0))?;
    changelog::append(
        conn,
        meta,
        "claim",
        "items",
        &item.get().to_string(),
        Some(&json!({
            "status": before.status,
            "claimant_id": before.claimant_id,
            "claimed_at": before.claimed_at,
        })),
        Some(&json!({
            "status": "in_progress",
            "claimant_id": owner,
            "claimed_at": claimed_at,
        })),
    )?;
    Ok(true)
}

/// Release `item`'s claim held by `owner`, clearing the claim columns.
///
/// A CAS that NULLs `claimant_id`/`claimed_at` only `WHERE claimant_id = ?owner`, so it
/// never clears a claim held by someone else. `status` is left untouched (the lifecycle
/// owns it; on give-up the coordinator resets it to `open` separately). Returns
/// `Ok(true)` if a claim by `owner` was cleared, `Ok(false)` if there was nothing to
/// clear. Recorded in the changelog (op `release`) when it clears a claim.
///
/// # Errors
/// Returns a database error if the query fails.
pub fn release(conn: &Connection, meta: &WriteMeta, item: ItemId, owner: &str) -> Result<bool> {
    let before = read_before(conn, item)?;
    let changed = conn
        .prepare_cached(
            "UPDATE items
                SET claimant_id = NULL,
                    claimed_at = NULL,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1 AND claimant_id = ?2",
        )?
        .execute(params![item.get(), owner])?;
    // `changed == 1` implies the row existed (so `before` is `Some`); the `if let`
    // keeps this panic-free while recording the before/after for the audit trail.
    if let (1, Some(before)) = (changed, before) {
        changelog::append(
            conn,
            meta,
            "release",
            "items",
            &item.get().to_string(),
            Some(&json!({
                "claimant_id": before.claimant_id,
                "claimed_at": before.claimed_at,
            })),
            Some(&json!({ "claimant_id": null, "claimed_at": null })),
        )?;
        return Ok(true);
    }
    Ok(false)
}

/// Every task that currently holds a claim (`claimant_id IS NOT NULL`).
///
/// Used by `doctor` and the coordinator's reclaim scan to enumerate holders before
/// probing owner liveness.
///
/// # Errors
/// Returns a database error if the query fails.
pub fn claimed(conn: &Connection) -> Result<Vec<ClaimInfo>> {
    let rows = conn
        .prepare_cached(
            "SELECT id, uid, claimant_id, claimed_at FROM items
              WHERE claimant_id IS NOT NULL
              ORDER BY id",
        )?
        .query_map([], |row| {
            Ok(ClaimInfo {
                id: ItemId::new(row.get(0)?),
                uid: row.get(1)?,
                owner: row.get(2)?,
                claimed_at: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Reclaim every claim whose owner is not alive: NULL the claim columns (only) of any
/// task held by an owner that is neither in `keep` nor passes the `is_alive` probe.
///
/// This is the deterministic crash-recovery net (design D27.1/D27.2). Liveness is
/// evaluated **inside the write transaction** against the freshly-read claim set — the
/// `is_alive` predicate is the caller's owner-existence probe (e.g. `kill -0`), and
/// `keep` holds owners that are alive by fiat (a live coordinator passes **its own**
/// owner id in `keep` so it never reclaims its own in-flight work). Evaluating liveness
/// here — rather than against a snapshot read before the txn — closes the race where a
/// claim acquired concurrently by a live owner would be reclaimed from a stale set. Each
/// distinct owner is probed at most once. The reclaim writes **only**
/// `claimant_id`/`claimed_at` — never `status` — so it cannot clash with a status
/// transition (e.g. the merge queue setting `done`), and the writer-actor serializes it
/// against every other write. Each reclaim is recorded in the changelog (op `reclaim`).
/// Returns the claims that were cleared.
///
/// # Errors
/// Returns a database error if a query fails.
pub fn reclaim_dead(
    conn: &Connection,
    meta: &WriteMeta,
    keep: &[String],
    is_alive: impl Fn(&str) -> bool,
) -> Result<Vec<ClaimInfo>> {
    let held = claimed(conn)?;
    // Probe each *distinct* owner's liveness at most once; owners in `keep` are alive by
    // fiat and never probed.
    let mut alive: HashMap<String, bool> = HashMap::new();
    for c in &held {
        if !alive.contains_key(&c.owner) {
            let live = keep.iter().any(|o| o == &c.owner) || is_alive(&c.owner);
            alive.insert(c.owner.clone(), live);
        }
    }
    let orphaned: Vec<ClaimInfo> = held.into_iter().filter(|c| !alive[&c.owner]).collect();
    for c in &orphaned {
        conn.prepare_cached(
            "UPDATE items
                SET claimant_id = NULL,
                    claimed_at = NULL,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
              WHERE id = ?1",
        )?
        .execute([c.id.get()])?;
        changelog::append(
            conn,
            meta,
            "reclaim",
            "items",
            &c.id.get().to_string(),
            Some(&json!({ "claimant_id": c.owner, "claimed_at": c.claimed_at })),
            Some(&json!({ "claimant_id": null, "claimed_at": null })),
        )?;
    }
    Ok(orphaned)
}

#[cfg(test)]
mod tests {
    use super::{claim, claimed, reclaim_dead, release};
    use crate::query::Scope;
    use crate::task::{create, ready, NewTask};
    use crate::Db;

    /// Count changelog rows for a given op against the items table.
    fn changelog_count(db: &Db, op: &str) -> i64 {
        let op = op.to_owned();
        db.read(move |conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM changelog WHERE op = ?1 AND entity_type = 'items'",
                [op],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .unwrap()
    }

    fn status_of(db: &Db, id: jkb_types::ItemId) -> String {
        db.read(move |conn| {
            Ok(
                conn.query_row("SELECT status FROM items WHERE id = ?1", [id.get()], |r| {
                    r.get::<_, String>(0)
                })?,
            )
        })
        .unwrap()
    }

    fn claimant_of(db: &Db, id: jkb_types::ItemId) -> Option<String> {
        db.read(move |conn| {
            Ok(conn.query_row(
                "SELECT claimant_id FROM items WHERE id = ?1",
                [id.get()],
                |r| r.get::<_, Option<String>>(0),
            )?)
        })
        .unwrap()
    }

    fn make_task(db: &Db, uid: &str) -> jkb_types::ItemId {
        let uid = uid.to_owned();
        db.write_txn("t", move |conn, meta| {
            create(conn, meta, &NewTask::new(uid.as_str(), "a task"))
        })
        .unwrap()
    }

    #[test]
    fn claim_is_cas_and_flips_status_to_in_progress() {
        let db = Db::open_in_memory().unwrap();
        let id = make_task(&db, "task:a");
        assert_eq!(status_of(&db, id), "open");

        // First owner acquires and the task atomically becomes in_progress.
        let ok = db
            .write_txn("t", move |conn, meta| claim(conn, meta, id, "host:1"))
            .unwrap();
        assert!(ok);
        assert_eq!(claimant_of(&db, id).as_deref(), Some("host:1"));
        assert_eq!(status_of(&db, id), "in_progress");

        // No claimed-but-open window: the claim and the transition happened together.
        // A different owner is refused; the first owner retains the claim.
        let refused = db
            .write_txn("t", move |conn, meta| claim(conn, meta, id, "host:2"))
            .unwrap();
        assert!(!refused);
        assert_eq!(claimant_of(&db, id).as_deref(), Some("host:1"));

        // The same owner re-claiming is idempotent (still true, still held).
        let again = db
            .write_txn("t", move |conn, meta| claim(conn, meta, id, "host:1"))
            .unwrap();
        assert!(again);
        assert_eq!(claimant_of(&db, id).as_deref(), Some("host:1"));
    }

    #[test]
    fn release_clears_the_claim_but_not_status() {
        let db = Db::open_in_memory().unwrap();
        let id = make_task(&db, "task:a");
        db.write_txn("t", move |conn, meta| claim(conn, meta, id, "host:1"))
            .unwrap();

        // Wrong owner can't release.
        let wrong = db
            .write_txn("t", move |conn, meta| release(conn, meta, id, "host:2"))
            .unwrap();
        assert!(!wrong);
        assert_eq!(claimant_of(&db, id).as_deref(), Some("host:1"));

        // Correct owner releases; the claim clears but status stays in_progress.
        let ok = db
            .write_txn("t", move |conn, meta| release(conn, meta, id, "host:1"))
            .unwrap();
        assert!(ok);
        assert_eq!(claimant_of(&db, id), None);
        assert_eq!(status_of(&db, id), "in_progress");
    }

    #[test]
    fn reclaim_clears_dead_owners_and_keeps_live_ones() {
        let db = Db::open_in_memory().unwrap();
        let live = make_task(&db, "task:live");
        let dead = make_task(&db, "task:dead");
        db.write_txn("t", move |conn, meta| {
            claim(conn, meta, live, "host:live")?;
            claim(conn, meta, dead, "host:dead")?;
            Ok(())
        })
        .unwrap();

        // Only host:live is verified alive. The dead owner's claim is cleared; the live
        // owner's claim is preserved; status is never written by reclaim.
        let reclaimed = db
            .write_txn("t", move |conn, meta| {
                reclaim_dead(conn, meta, &["host:live".to_owned()], |_| false)
            })
            .unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].owner, "host:dead");
        assert_eq!(claimant_of(&db, live).as_deref(), Some("host:live"));
        assert_eq!(claimant_of(&db, dead), None);
        assert_eq!(status_of(&db, live), "in_progress");
        assert_eq!(status_of(&db, dead), "in_progress"); // reclaim never touched status
    }

    #[test]
    fn claim_refuses_a_terminal_task() {
        use crate::task::set_status;
        use jkb_types::TaskStatus;
        let db = Db::open_in_memory().unwrap();
        for terminal in [TaskStatus::Done, TaskStatus::Cancelled] {
            let id = make_task(&db, &format!("task:{}", terminal.as_str()));
            db.write_txn("t", move |conn, meta| set_status(conn, meta, id, terminal))
                .unwrap();
            // Claiming a finished task is rejected (never silently re-opened).
            let err = db.write_txn("t", move |conn, meta| claim(conn, meta, id, "host:1"));
            assert!(err.is_err(), "claim of {terminal:?} task should error");
            assert_eq!(status_of(&db, id), terminal.as_str());
            assert_eq!(claimant_of(&db, id), None);
        }
    }

    #[test]
    fn reclaim_keeps_owners_the_predicate_reports_alive_and_probes_each_once() {
        use std::sync::{Arc, Mutex};
        let db = Db::open_in_memory().unwrap();
        let a = make_task(&db, "task:a");
        let b = make_task(&db, "task:b");
        let c = make_task(&db, "task:c");
        // a and b share owner host:live; c is held by host:dead.
        db.write_txn("t", move |conn, meta| {
            claim(conn, meta, a, "host:live")?;
            claim(conn, meta, b, "host:live")?;
            claim(conn, meta, c, "host:dead")?;
            Ok(())
        })
        .unwrap();

        // The predicate reports host:live alive, host:dead not. Record every probe to
        // prove each *distinct* owner is checked at most once (two tasks, one owner).
        let probed = Arc::new(Mutex::new(Vec::<String>::new()));
        let probed2 = Arc::clone(&probed);
        let reclaimed = db
            .write_txn("t", move |conn, meta| {
                reclaim_dead(conn, meta, &[], |o| {
                    probed2.lock().unwrap().push(o.to_owned());
                    o == "host:live"
                })
            })
            .unwrap();

        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].owner, "host:dead");
        assert_eq!(claimant_of(&db, a).as_deref(), Some("host:live"));
        assert_eq!(claimant_of(&db, b).as_deref(), Some("host:live"));
        assert_eq!(claimant_of(&db, c), None);
        let mut seen = probed.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, vec!["host:dead".to_owned(), "host:live".to_owned()]);
    }

    #[test]
    fn claim_excludes_from_ready_and_reclaim_restores() {
        let db = Db::open_in_memory().unwrap();
        let id = make_task(&db, "task:a");

        // Unclaimed → ready.
        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        assert_eq!(frontier.len(), 1);

        // Claimed → excluded from the frontier.
        db.write_txn("t", move |conn, meta| claim(conn, meta, id, "host:dead"))
            .unwrap();
        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        assert!(frontier.is_empty());

        // A live scan that does NOT include the owner reclaims it → back in the frontier.
        db.write_txn("t", |conn, meta| reclaim_dead(conn, meta, &[], |_| false))
            .unwrap();
        let frontier = db.read(|conn| ready(conn, Scope::All, &[])).unwrap();
        assert_eq!(frontier.len(), 1);
    }

    #[test]
    fn claim_release_reclaim_each_append_a_changelog_row() {
        let db = Db::open_in_memory().unwrap();
        let id = make_task(&db, "task:a");

        db.write_txn("t", move |conn, meta| claim(conn, meta, id, "host:x"))
            .unwrap();
        assert_eq!(changelog_count(&db, "claim"), 1);

        db.write_txn("t", move |conn, meta| release(conn, meta, id, "host:x"))
            .unwrap();
        assert_eq!(changelog_count(&db, "release"), 1);

        // Re-claim with a dead owner, then reclaim it.
        db.write_txn("t", move |conn, meta| claim(conn, meta, id, "host:dead"))
            .unwrap();
        db.write_txn("t", |conn, meta| reclaim_dead(conn, meta, &[], |_| false))
            .unwrap();
        assert_eq!(changelog_count(&db, "reclaim"), 1);
    }

    // ---- proptest: the CAS invariant under an arbitrary claim/release interleaving ----

    use proptest::prelude::*;

    /// One step against a single task: some owner claims, or the current holder releases.
    #[derive(Debug, Clone)]
    enum Step {
        Claim(u8),
        Release(u8),
    }

    fn steps() -> impl Strategy<Value = Vec<Step>> {
        let owner = 0u8..4; // a small owner pool so collisions are frequent
        let step = prop_oneof![
            owner.clone().prop_map(Step::Claim),
            owner.prop_map(Step::Release),
        ];
        prop::collection::vec(step, 0..24)
    }

    proptest! {
        #[test]
        fn claim_cas_invariant_holds_under_interleaving(steps in steps()) {
            let db = Db::open_in_memory().unwrap();
            let id = make_task(&db, "task:cas");

            // A shadow model of the single-holder invariant: at most one owner ever holds
            // the claim, and a claim succeeds exactly when the slot is free or same-owner.
            let mut held: Option<u8> = None;
            for step in steps {
                match step {
                    Step::Claim(o) => {
                        let owner = format!("host:{o}");
                        let ok = db
                            .write_txn("t", move |conn, meta| claim(conn, meta, id, &owner))
                            .unwrap();
                        let expected = held.is_none() || held == Some(o);
                        prop_assert_eq!(ok, expected);
                        if ok {
                            held = Some(o);
                        }
                        // The DB claimant must match the model exactly.
                        prop_assert_eq!(
                            claimant_of(&db, id),
                            held.map(|h| format!("host:{h}"))
                        );
                    }
                    Step::Release(o) => {
                        let owner = format!("host:{o}");
                        let ok = db
                            .write_txn("t", move |conn, meta| release(conn, meta, id, &owner))
                            .unwrap();
                        let expected = held == Some(o);
                        prop_assert_eq!(ok, expected);
                        if ok {
                            held = None;
                        }
                        prop_assert_eq!(
                            claimant_of(&db, id),
                            held.map(|h| format!("host:{h}"))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn claimed_lists_only_held_tasks() {
        let db = Db::open_in_memory().unwrap();
        let a = make_task(&db, "task:a");
        let _b = make_task(&db, "task:b");
        db.write_txn("t", move |conn, meta| claim(conn, meta, a, "host:1"))
            .unwrap();

        let held = db.read(claimed).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].uid, "task:a");
        assert_eq!(held[0].owner, "host:1");
    }
}
