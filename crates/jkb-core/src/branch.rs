//! Branch records (design B-series): the four facts about a git **branch** that git itself does
//! not own, keyed `(repo, branch)`.
//!
//! Which branch a task is on is a property of the *task* and stays a facet. Where a branch was
//! cut, which instance of that name the measurement describes, which branch it lands on, and
//! whether jkb itself merged it are properties of the *branch*, shared by every task on it — and
//! storing them as tag applications is what produced fifteen review passes of defects. See
//! `V013__branch_records.sql` for the diagnosis.
//!
//! This module owns **storage** only. Everything about *asking git* — measuring a fork point,
//! reading a reflog, deciding whether a branch has done any work — lives in `jkb-cli`'s `base`
//! and `gitrepo` modules, because core does not shell out to git.
//!
//! ## The one write, and why it has that shape
//!
//! [`record_cut_point`] is a single `INSERT … ON CONFLICT DO UPDATE` whose one update arm is the
//! staleness discard. It is not `forget` followed by an insert — not a sequence at all — because
//! every previous fix in this area was a rule a caller had to remember in the right order, and
//! this one cannot be dropped by omission or mis-sequenced: there is nothing to sequence, and
//! dropping it means writing different SQL than the design specifies.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::store::WriteMeta;
use crate::{changelog, Result};

/// The **instance anchor**: a branch's creation reflog entry, as its `new` revision and the
/// entry's own timestamp.
///
/// A branch name outlives the branch that held it — delete `task/x`, cut a fresh one under the
/// same name, and the old record still resolves and still differs from the new tip, so the
/// freshly-cut guard is skipped and a task with nothing on it closes. Nothing that *transfers
/// between clones* discriminates the two instances: a ref is a name → commit pointer with no
/// durable identity, and `creatordate` is the tip commit's committer date rather than the ref
/// event's time.
///
/// The checkout-local ref journal does discriminate, and the reason is the same fact that first
/// looked like an obstacle: **deleting a branch destroys its log**, so the recreated branch's log
/// provably starts fresh with a creation entry describing *this* instance. No verb forges one on
/// an existing branch (`branch -f` and `checkout -B` append `Reset`-class entries), and its loss
/// announces itself, because expiry removes oldest-first and only a creation entry has
/// `old = zeros`.
///
/// The pair is load-bearing: recreating a branch from the same start point yields the same `sha`,
/// so the timestamp is what separates the instances. The *message* is deliberately not part of
/// the anchor — it varies (`from main`, `from HEAD`, `from main~0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// The revision the branch was created at (the entry's `new` value).
    pub sha: String,
    /// The reflog entry's own unix timestamp — **not** the commit's.
    pub ts: i64,
}

/// A landing jkb itself performed: when, onto what, and from which tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landing {
    /// When the graft happened (ISO-8601).
    pub at: String,
    /// The branch the work was grafted onto.
    pub onto: String,
    /// The branch's **own tip** at that moment.
    ///
    /// A land does not move the branch ref (the graft rebases detached and fast-forwards the
    /// target), so `tip == head` holds until the name is re-pointed. That is what stops a
    /// namesake recreated after a landing from inheriting its predecessor's event.
    pub head: String,
}

/// One branch's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRecord {
    /// `gitrepo::key(dir)` — the repository the branch lives in.
    pub repo: String,
    /// The short branch name.
    pub branch: String,
    /// The measured fork point, or `None` for "not recorded", which both readers treat as
    /// *do not act*.
    pub cut_point: Option<String>,
    /// The instance the cut point was measured on, when the ref journal could supply one.
    pub anchor: Option<Anchor>,
    /// The branch this one lands on. `None` on an existing row means "lands on trunk / is on no
    /// batch"; a missing row means unknown.
    pub land_target: Option<String>,
    /// The landing jkb performed, if it did.
    pub landed: Option<Landing>,
}

/// A cut point as **evidence**, not as a value a caller chose.
///
/// The distinction is the whole safety argument of [`record_cut_point`]'s update arm. An
/// untouched branch forked at its own tip — provably, by definition — so that tip is the one
/// value whose disagreement with a stored record proves the record belongs to a different
/// instance of the name. A fork point measured on a branch that has done work proves nothing of
/// the sort, and may only fill a gap.
///
/// Only `jkb-cli`'s `base::measure` constructs these, from `untouched_tip`'s answer. A writer
/// that fabricated `UntouchedTip` would defeat the arm — but that is an explicit lie in the code
/// rather than a step someone forgot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cut {
    /// The branch has no commits of its own, so this tip **is** its fork point.
    UntouchedTip(String),
    /// The branch has diverged and this is where. Never its tip.
    Fork(String),
}

impl Cut {
    /// The commit id, whichever kind of evidence produced it.
    #[must_use]
    pub fn sha(&self) -> &str {
        match self {
            Self::UntouchedTip(s) | Self::Fork(s) => s,
        }
    }
}

/// The two independent proofs that a stored record describes a branch that no longer exists.
///
/// Both act only in the direction of replacing a record that is provably not this branch's.
/// Neither can write anything but the measurement handed to [`record_cut_point`] in the same
/// call, and with both false the statement can only fill a `NULL`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Supersede {
    /// The branch has no commits of its own **and** the retain-license did not fire.
    ///
    /// An untouched branch forked at its tip, so a stored value that is anything else belongs to
    /// whatever held the name before. The known false positive — a branch whose work was merged
    /// away also looks untouched — is what the retain-license (`jkb-cli`'s `base`) exists to
    /// narrow; where it cannot verify, this degrades to discard-and-hold, which costs a missed
    /// close rather than a false one.
    pub untouched: bool,
    /// The branch's current creation reflog entry differs from the stored anchor — positive proof
    /// of recycling, which unlike the untouched signature works even when the namesake already
    /// carries commits.
    pub anchor_mismatch: bool,
}

impl Supersede {
    /// Whether either proof holds.
    fn fires(self) -> bool {
        self.untouched || self.anchor_mismatch
    }
}

/// Read one branch's record.
///
/// # Errors
/// Returns an error if the query fails.
pub fn get(conn: &Connection, repo: &str, branch: &str) -> Result<Option<BranchRecord>> {
    Ok(conn
        .prepare_cached(
            "SELECT repo, branch, cut_point, anchor_sha, anchor_ts, land_target,
                    landed_at, landed_onto, landed_head
             FROM branch_records WHERE repo = ?1 AND branch = ?2",
        )?
        .query_row(params![repo, branch], row_to_record)
        .optional()?)
}

/// Every branch record for one repository, keyed by branch name — **one** query.
///
/// The staging view redraws on every database write and reads one row per task, so the branch
/// facts it needs must arrive in a single read rather than a lookup per task (design risk 2).
///
/// # Errors
/// Returns an error if the query fails.
pub fn for_repo(conn: &Connection, repo: &str) -> Result<BTreeMap<String, BranchRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT repo, branch, cut_point, anchor_sha, anchor_ts, land_target,
                landed_at, landed_onto, landed_head
         FROM branch_records WHERE repo = ?1 ORDER BY branch",
    )?;
    let rows = stmt.query_map(params![repo], row_to_record)?;
    let mut out = BTreeMap::new();
    for row in rows {
        let record = row?;
        out.insert(record.branch.clone(), record);
    }
    Ok(out)
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<BranchRecord> {
    let anchor_sha: Option<String> = row.get(3)?;
    let anchor_ts: Option<i64> = row.get(4)?;
    let landed_at: Option<String> = row.get(6)?;
    let landed_onto: Option<String> = row.get(7)?;
    let landed_head: Option<String> = row.get(8)?;
    Ok(BranchRecord {
        repo: row.get(0)?,
        branch: row.get(1)?,
        cut_point: row.get(2)?,
        // Paired by a CHECK, so `zip` cannot silently drop half of a well-formed row.
        anchor: anchor_sha
            .zip(anchor_ts)
            .map(|(sha, ts)| Anchor { sha, ts }),
        land_target: row.get(5)?,
        landed: landed_at
            .zip(landed_onto)
            .zip(landed_head)
            .map(|((at, onto), head)| Landing { at, onto, head }),
    })
}

/// Record `branch`'s cut point — the **one** writer, and the one place the staleness rule lives.
///
/// One statement. Its only `DO UPDATE` arm fires when the stored value is `NULL` (a gap to fill)
/// or when `supersede` proves the stored value describes a different instance of the name — and
/// in that case it clears the predecessor's landing event in the same statement, because a row
/// that provably describes a branch that no longer exists must not hand its namesake a landing it
/// never had.
///
/// What it can write is bounded by its inputs, not by discipline: with `supersede` all-false the
/// statement can only fill a `NULL`, and the untouched arm can write nothing but the value that
/// *proves* the row stale — the untouched branch's own tip.
///
/// Returns whether a row was written, so a caller can report the difference between "recorded"
/// and "an existing record stood".
///
/// # Errors
/// Returns an error if the statement or the changelog append fails — including a `cut_point` that
/// is not a full lowercase object id, which the schema refuses.
pub fn record_cut_point(
    conn: &Connection,
    meta: &WriteMeta,
    repo: &str,
    branch: &str,
    cut: &Cut,
    anchor: Option<&Anchor>,
    supersede: Supersede,
) -> Result<bool> {
    let before = get(conn, repo, branch)?;
    // Lowercased here, at the door, rather than at any caller: `base::is_object_id` accepts
    // uppercase hex and the CHECK does not, and two guards that disagree about the same value are
    // how a legal write becomes a constraint violation nobody can read.
    let sha = cut.sha().to_ascii_lowercase();
    let (anchor_sha, anchor_ts) = match anchor {
        Some(a) => (Some(a.sha.as_str()), Some(a.ts)),
        None => (None, None),
    };
    let written: Option<i64> = conn
        .prepare_cached(
            "INSERT INTO branch_records
                 (repo, branch, cut_point, anchor_sha, anchor_ts, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT (repo, branch) DO UPDATE SET
                 cut_point = excluded.cut_point,
                 anchor_sha = excluded.anchor_sha,
                 anchor_ts = excluded.anchor_ts,
                 landed_at = NULL, landed_onto = NULL, landed_head = NULL
             WHERE branch_records.cut_point IS NULL
                OR (?6 AND branch_records.cut_point <> excluded.cut_point)
             RETURNING id",
        )?
        .query_row(
            params![repo, branch, sha, anchor_sha, anchor_ts, supersede.fires()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = written else {
        return Ok(false);
    };
    changelog::append(
        conn,
        meta,
        // `insert` only when there was no row, so `undo`'s `DELETE … WHERE rowid = ?` inverts
        // exactly what happened. Superseding an existing row is an update and is deliberately not
        // invertible by `undo` — the same treatment claims get.
        if before.is_some() { "update" } else { "insert" },
        "branch_records",
        &id.to_string(),
        before.as_ref().map(record_json).as_ref(),
        Some(&json!({
            "repo": repo, "branch": branch, "cut_point": sha,
            "anchor_sha": anchor_sha, "anchor_ts": anchor_ts,
        })),
    )?;
    Ok(true)
}

/// Set (or clear) the branch this one lands on.
///
/// Clearing is meaningful: `None` on an existing row says "lands on trunk / is on no batch",
/// which is what `jkb task abandon` and a `--onto trunk` leave behind. A *missing* row still
/// means unknown.
///
/// # Errors
/// Returns an error if the statement or the changelog append fails.
pub fn set_land_target(
    conn: &Connection,
    meta: &WriteMeta,
    repo: &str,
    branch: &str,
    target: Option<&str>,
) -> Result<()> {
    let before = get(conn, repo, branch)?;
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO branch_records (repo, branch, land_target, created_at)
             VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT (repo, branch) DO UPDATE SET land_target = excluded.land_target
             RETURNING id",
        )?
        .query_row(params![repo, branch, target], |row| row.get(0))?;
    changelog::append(
        conn,
        meta,
        if before.is_some() { "update" } else { "insert" },
        "branch_records",
        &id.to_string(),
        before.as_ref().map(record_json).as_ref(),
        Some(&json!({ "repo": repo, "branch": branch, "land_target": target })),
    )?;
    Ok(())
}

/// Record that jkb grafted `branch` onto `onto`, from tip `head`.
///
/// Only the two places jkb performs the merge itself write this — `jkb task land` after its gate
/// is green, and the merge queue's verb after a genuine fast-forward. `head` is the branch's own
/// tip at that moment, and the event is credited later **only** while the branch still points
/// there (or is gone): the row is keyed by name, and a name outlives its branch.
///
/// # Errors
/// Returns an error if the statement or the changelog append fails.
pub fn record_landing(
    conn: &Connection,
    meta: &WriteMeta,
    repo: &str,
    branch: &str,
    onto: &str,
    head: &str,
) -> Result<()> {
    let before = get(conn, repo, branch)?;
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO branch_records
                 (repo, branch, landed_at, landed_onto, landed_head, created_at)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3, ?4,
                     strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT (repo, branch) DO UPDATE SET
                 landed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 landed_onto = excluded.landed_onto,
                 landed_head = excluded.landed_head
             RETURNING id",
        )?
        .query_row(params![repo, branch, onto, head], |row| row.get(0))?;
    changelog::append(
        conn,
        meta,
        if before.is_some() { "update" } else { "insert" },
        "branch_records",
        &id.to_string(),
        before.as_ref().map(record_json).as_ref(),
        Some(&json!({
            "repo": repo, "branch": branch, "landed_onto": onto, "landed_head": head,
        })),
    )?;
    Ok(())
}

/// Drop only the **cut point** and the instance anchor, leaving the branch's row otherwise
/// intact — `jkb task base --forget`.
///
/// Distinct from [`forget`], and the distinction is what the two verbs mean. This one repairs a
/// measurement for a branch that still exists: where it lands and whether jkb landed it are
/// unaffected facts, and taking them out with the cut point would silently drop the task out of
/// `jkb staging ls` with no way back but re-stating `--onto`. [`forget`] is for a branch that is
/// **gone**, where every fact about it goes with it.
///
/// Returns whether a cut point was removed.
///
/// # Errors
/// Returns an error if the statement or the changelog append fails.
pub fn forget_cut_point(
    conn: &Connection,
    meta: &WriteMeta,
    repo: &str,
    branch: &str,
) -> Result<bool> {
    let Some(before) = get(conn, repo, branch)? else {
        return Ok(false);
    };
    if before.cut_point.is_none() {
        return Ok(false);
    }
    let id: i64 = conn
        .prepare_cached(
            "UPDATE branch_records
                SET cut_point = NULL, anchor_sha = NULL, anchor_ts = NULL
              WHERE repo = ?1 AND branch = ?2
             RETURNING id",
        )?
        .query_row(params![repo, branch], |row| row.get(0))?;
    changelog::append(
        conn,
        meta,
        "update",
        "branch_records",
        &id.to_string(),
        Some(&record_json(&before)),
        Some(&json!({ "repo": repo, "branch": branch, "cut_point": null })),
    )?;
    Ok(true)
}

/// Drop `branch`'s record entirely — because the branch itself is gone.
///
/// The verb of `jkb task abandon --delete-branch` and of `jkb task base --forget`, and **not** a
/// step in any write: abandoning frees the branch *name* while leaving the task live, so the next
/// `jkb task work` cuts a new branch under it, and a surviving record would still resolve, still
/// differ from the new tip, and skip the freshly-cut guard.
///
/// Returns whether a row was removed.
///
/// # Errors
/// Returns an error if the statement or the changelog append fails.
pub fn forget(conn: &Connection, meta: &WriteMeta, repo: &str, branch: &str) -> Result<bool> {
    let Some(before) = get(conn, repo, branch)? else {
        return Ok(false);
    };
    let removed = conn
        .prepare_cached("DELETE FROM branch_records WHERE repo = ?1 AND branch = ?2")?
        .execute(params![repo, branch])?;
    if removed == 0 {
        return Ok(false);
    }
    changelog::append(
        conn,
        meta,
        "delete",
        "branch_records",
        &format!("{repo}:{branch}"),
        Some(&record_json(&before)),
        None,
    )?;
    Ok(true)
}

/// A record as changelog JSON — every column, so an audit reader can see what was replaced.
fn record_json(r: &BranchRecord) -> serde_json::Value {
    json!({
        "repo": r.repo,
        "branch": r.branch,
        "cut_point": r.cut_point,
        "anchor_sha": r.anchor.as_ref().map(|a| a.sha.clone()),
        "anchor_ts": r.anchor.as_ref().map(|a| a.ts),
        "land_target": r.land_target,
        "landed_at": r.landed.as_ref().map(|l| l.at.clone()),
        "landed_onto": r.landed.as_ref().map(|l| l.onto.clone()),
        "landed_head": r.landed.as_ref().map(|l| l.head.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        forget, get, record_cut_point, record_landing, set_land_target, Anchor, BranchRecord, Cut,
        Supersede,
    };
    use crate::Db;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    /// `record_cut_point` through the writer-actor, returning whether it wrote.
    fn record(db: &Db, branch: &str, cut: &Cut, sup: Supersede) -> bool {
        let (branch, cut) = (branch.to_owned(), cut.clone());
        db.write_txn("t", move |conn, meta| {
            record_cut_point(conn, meta, "jkb", &branch, &cut, None, sup)
        })
        .unwrap()
    }

    fn record_with_anchor(db: &Db, branch: &str, cut: &Cut, anchor: &Anchor, sup: Supersede) {
        let (branch, cut, anchor) = (branch.to_owned(), cut.clone(), anchor.clone());
        db.write_txn("t", move |conn, meta| {
            record_cut_point(conn, meta, "jkb", &branch, &cut, Some(&anchor), sup)
        })
        .unwrap();
    }

    fn row(db: &Db, branch: &str) -> Option<BranchRecord> {
        let branch = branch.to_owned();
        db.read(move |conn| get(conn, "jkb", &branch)).unwrap()
    }

    fn cut_of(db: &Db, branch: &str) -> Option<String> {
        row(db, branch).and_then(|r| r.cut_point)
    }

    /// The key, kept as a cheap regression: the `<branch>:<sha>` encoding existed only because
    /// tags are item-keyed, and lending one branch's cut point to another is what disabled the
    /// freshly-cut guard for the branch that borrowed it.
    #[test]
    fn a_cut_point_is_never_returned_for_another_branch() {
        let db = Db::open_in_memory().unwrap();
        record(
            &db,
            "task/a",
            &Cut::Fork(A.to_owned()),
            Supersede::default(),
        );
        assert_eq!(cut_of(&db, "task/a").as_deref(), Some(A));
        assert_eq!(
            cut_of(&db, "task/b"),
            None,
            "task/b was lent task/a's record"
        );
        // …nor across repositories, which is why `repo` is in the key rather than a fact each
        // caller had to remember to compare.
        let other = db
            .read(|conn| get(conn, "other-checkout", "task/a"))
            .unwrap();
        assert_eq!(other, None, "a namesake branch elsewhere shares the record");
    }

    /// Only the first observation can know a cut point a later rebase has moved past. A caller
    /// that runs again — `jkb task work` on a resumed session, `/task-swarm` re-tagging a group
    /// on every pass — must not replace it.
    #[test]
    fn an_existing_record_is_never_overwritten() {
        let db = Db::open_in_memory().unwrap();
        record(
            &db,
            "task/a",
            &Cut::Fork(A.to_owned()),
            Supersede::default(),
        );
        let wrote = record(
            &db,
            "task/a",
            &Cut::Fork(B.to_owned()),
            Supersede::default(),
        );
        assert!(!wrote, "the second write reported success");
        assert_eq!(cut_of(&db, "task/a").as_deref(), Some(A));
    }

    /// The staleness discard, as the statement's own shape. Delete a branch, cut a fresh one
    /// under the same name, and the old value still resolves and still differs from the new tip —
    /// which skips the freshly-cut guard and closes a task with nothing written on it. An
    /// untouched branch forked at its own tip, so a stored value that is anything else belongs to
    /// whatever held the name before.
    #[test]
    fn a_recycled_names_stale_record_is_superseded_by_the_untouched_tip() {
        let db = Db::open_in_memory().unwrap();
        record(
            &db,
            "task/x",
            &Cut::Fork(A.to_owned()),
            Supersede::default(),
        );
        let wrote = record(
            &db,
            "task/x",
            &Cut::UntouchedTip(B.to_owned()),
            Supersede {
                untouched: true,
                anchor_mismatch: false,
            },
        );
        assert!(wrote, "the stale record was not superseded");
        assert_eq!(cut_of(&db, "task/x").as_deref(), Some(B));
    }

    /// The other half, and the one a weakened WHERE clause would break silently: a fork point
    /// measured on a branch that has done work proves nothing about which instance of the name it
    /// belongs to, so it may only fill a gap. Both proofs are false here — an anchor mismatch is
    /// an independent trigger and is tested by the read side.
    #[test]
    fn a_fork_measurement_cannot_overwrite_an_existing_record() {
        let db = Db::open_in_memory().unwrap();
        record(
            &db,
            "task/x",
            &Cut::Fork(A.to_owned()),
            Supersede::default(),
        );
        // Even asserting untouched-ness cannot smuggle a `Fork` in, because the arm compares
        // against `excluded.cut_point` and the flag is only ever set for a measured tip.
        let wrote = record(
            &db,
            "task/x",
            &Cut::Fork(B.to_owned()),
            Supersede {
                untouched: false,
                anchor_mismatch: false,
            },
        );
        assert!(!wrote);
        assert_eq!(cut_of(&db, "task/x").as_deref(), Some(A));
    }

    /// A verified anchor mismatch is positive proof of recycling and supersedes on its own —
    /// unlike the untouched signature it works when the namesake already carries commits, which
    /// repairs the record before anything reads it.
    #[test]
    fn a_verified_anchor_mismatch_supersedes_on_its_own() {
        let db = Db::open_in_memory().unwrap();
        let old = Anchor {
            sha: A.to_owned(),
            ts: 100,
        };
        record_with_anchor(
            &db,
            "task/x",
            &Cut::Fork(A.to_owned()),
            &old,
            Supersede::default(),
        );
        let fresh = Anchor {
            sha: A.to_owned(),
            ts: 200,
        };
        record_with_anchor(
            &db,
            "task/x",
            &Cut::Fork(B.to_owned()),
            &fresh,
            Supersede {
                untouched: false,
                anchor_mismatch: true,
            },
        );
        assert_eq!(cut_of(&db, "task/x").as_deref(), Some(B));
        assert_eq!(
            row(&db, "task/x").and_then(|r| r.anchor),
            Some(fresh),
            "the fresh anchor was not stored with what it recorded"
        );
    }

    /// Superseding clears the predecessor's landing in the **same statement**. The row provably
    /// describes a branch that no longer exists, so leaving `landed_at` would hand the namesake a
    /// landing it never had — the same name-staleness one column over.
    #[test]
    fn superseding_clears_the_predecessors_landing() {
        let db = Db::open_in_memory().unwrap();
        record(
            &db,
            "task/x",
            &Cut::Fork(A.to_owned()),
            Supersede::default(),
        );
        db.write_txn("t", |conn, meta| {
            record_landing(conn, meta, "jkb", "task/x", "batch", C)
        })
        .unwrap();
        assert!(row(&db, "task/x").unwrap().landed.is_some());
        record(
            &db,
            "task/x",
            &Cut::UntouchedTip(B.to_owned()),
            Supersede {
                untouched: true,
                anchor_mismatch: false,
            },
        );
        let r = row(&db, "task/x").unwrap();
        assert_eq!(r.cut_point.as_deref(), Some(B));
        assert_eq!(
            r.landed, None,
            "the recreated branch inherited its predecessor's landing event"
        );
    }

    /// The pairing of `base::is_object_id` and the schema CHECK: the guard accepts uppercase hex
    /// (git never emits it, a hand-written value can), so the writer lowercases and the two
    /// cannot disagree about the same value.
    #[test]
    fn an_uppercase_object_id_is_stored_lowercased_rather_than_refused() {
        let db = Db::open_in_memory().unwrap();
        let upper = "DEADBEEF".repeat(5);
        record(
            &db,
            "task/x",
            &Cut::Fork(upper.clone()),
            Supersede::default(),
        );
        assert_eq!(
            cut_of(&db, "task/x").as_deref(),
            Some(upper.to_lowercase().as_str())
        );
    }

    /// The invariant is the **schema's**, not the guard's. `base::is_object_id` exists to produce
    /// a sentence rather than a constraint violation; bypass it and the store still refuses, which
    /// is what makes "a symbolic revision is never stored" true however the value arrives.
    #[test]
    fn the_schema_refuses_a_symbolic_revision_even_with_the_guard_bypassed() {
        let db = Db::open_in_memory().unwrap();
        for bad in ["HEAD", "main", "@", "1111111", &"z".repeat(40)] {
            let planted = (*bad).to_owned();
            let err = db.write_txn("t", move |conn, meta| {
                record_cut_point(
                    conn,
                    meta,
                    "jkb",
                    "task/x",
                    &Cut::Fork(planted),
                    None,
                    Supersede::default(),
                )
            });
            assert!(err.is_err(), "`{bad}` was stored as a cut point");
        }
        assert_eq!(cut_of(&db, "task/x"), None);
    }

    /// A land target is separable from a cut point, and `None` on an existing row is a real state
    /// — "lands on trunk / is on no batch" — distinct from a missing row, which means unknown.
    #[test]
    fn a_land_target_is_set_and_cleared_without_touching_the_cut_point() {
        let db = Db::open_in_memory().unwrap();
        record(
            &db,
            "task/x",
            &Cut::Fork(A.to_owned()),
            Supersede::default(),
        );
        db.write_txn("t", |conn, meta| {
            set_land_target(conn, meta, "jkb", "task/x", Some("batch"))
        })
        .unwrap();
        assert_eq!(
            row(&db, "task/x").unwrap().land_target.as_deref(),
            Some("batch")
        );
        db.write_txn("t", |conn, meta| {
            set_land_target(conn, meta, "jkb", "task/x", None)
        })
        .unwrap();
        let r = row(&db, "task/x").unwrap();
        assert_eq!(r.land_target, None);
        assert_eq!(
            r.cut_point.as_deref(),
            Some(A),
            "clearing the target lost the cut point"
        );
    }

    /// Forgetting is keyed too: it drops exactly the branch named and leaves every sibling's
    /// record alone. The facet form could not — the documented repair cleared the facet's other
    /// values, which were other branches' cut points.
    #[test]
    fn forgetting_drops_one_branchs_record_and_no_others() {
        let db = Db::open_in_memory().unwrap();
        record(
            &db,
            "task/a",
            &Cut::Fork(A.to_owned()),
            Supersede::default(),
        );
        record(
            &db,
            "task/b",
            &Cut::Fork(B.to_owned()),
            Supersede::default(),
        );
        let dropped = db
            .write_txn("t", |conn, meta| forget(conn, meta, "jkb", "task/a"))
            .unwrap();
        assert!(dropped);
        assert_eq!(cut_of(&db, "task/a"), None);
        assert_eq!(cut_of(&db, "task/b").as_deref(), Some(B));
        // Idempotent: forgetting a branch with no record is not an error.
        assert!(!db
            .write_txn("t", |conn, meta| forget(conn, meta, "jkb", "task/a"))
            .unwrap());
    }
}
