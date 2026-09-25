//! A task's review record: which review rounds were recorded against it, at which HEAD, and whether a
//! landing waived the gate (design D52.7, supersedes D38.4's facets).
//!
//! **Not tags.** These three facts decide the land gate, and a tag is content any writer may set —
//! including the sync engine applying a line of a `tasks.md` that an agent in the dev container
//! edited (`#review-waived=x` waived the gate; hole H3). `tag.rs` records why the answer is not a
//! reserved facet: that apparatus was tried once, for `base`, and six choke points did not close it.
//! The fact moves out of the facet namespace instead, and the gate reads only this table, so a
//! `reviewed=` tag is ordinary content that nothing trusts.
//!
//! **Append-only, and not changelogged**, like `task_transitions`: a review happened, and an undo of
//! the transaction that recorded it does not un-happen it. The two writers are
//! `task.review_record` ([`record`]) and a `--no-review` landing ([`waive`]).

use std::collections::HashMap;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use jkb_types::{Error as TypeError, ItemId};

use crate::store::WriteMeta;
use crate::{Error, Result};

/// What the land gate reads about a task's reviews.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewState {
    /// The HEAD the newest recorded review ran against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed: Option<String>,
    /// Every recorded round's findings namespace, in recording order. The gate unions them: a newer
    /// round never retires an older round's open must-fix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    /// The HEAD a `--no-review` landing waived the gate for, newest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waived: Option<String>,
    /// The HEAD a landing ran its gate on the host for, rather than in the dev container
    /// (`--gate-on-host`, D52.12), newest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_on_host: Option<String>,
}

impl ReviewState {
    /// Nothing recorded at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reviewed.is_none()
            && self.namespaces.is_empty()
            && self.waived.is_none()
            && self.gate_on_host.is_none()
    }
}

const MAX_TEXT: usize = 1024;

fn check(what: &str, v: &str) -> Result<()> {
    if v.is_empty() || v.len() > MAX_TEXT {
        return Err(Error::Types(TypeError::Validation(format!(
            "a {what} of 1 to {MAX_TEXT} bytes"
        ))));
    }
    Ok(())
}

/// Record that a review of `task` ran at `sha`, filing its findings under `ns`. Recording the same
/// round twice is a no-op.
///
/// # Errors
/// [`Error::Types`] for an empty or oversized value, or a database error.
pub fn record(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    ns: &str,
    sha: &str,
    actor: &str,
) -> Result<()> {
    check("findings namespace", ns)?;
    check("sha", sha)?;
    let exists: bool = conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM reviews
                            WHERE item_id = ?1 AND kind = 'recorded' AND ns = ?2 AND sha = ?3)",
        )?
        .query_row(params![task.get(), ns, sha], |r| r.get(0))?;
    if !exists {
        insert(conn, meta, task, "recorded", Some(ns), sha, actor)?;
    }
    Ok(())
}

/// Record that `task` landed at `sha` with the review gate waived (`--no-review`).
///
/// # Errors
/// [`Error::Types`] for an empty or oversized value, or a database error.
pub fn waive(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    sha: &str,
    actor: &str,
) -> Result<()> {
    check("sha", sha)?;
    insert(conn, meta, task, "waived", None, sha, actor)
}

/// Record that a landing of `task` at `sha` ran its gate on the host (`--gate-on-host`, D52.12): the
/// candidate's own, container-written code, run as the operator — by the operator's choice, visibly.
///
/// # Errors
/// [`Error::Types`] for an empty or oversized value, or a database error.
pub fn gate_on_host(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    sha: &str,
    actor: &str,
) -> Result<()> {
    check("sha", sha)?;
    insert(conn, meta, task, "gate_on_host", None, sha, actor)
}

fn insert(
    conn: &Connection,
    meta: &WriteMeta,
    task: ItemId,
    kind: &str,
    ns: Option<&str>,
    sha: &str,
    actor: &str,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO reviews (item_id, kind, ns, sha, actor, txn_id, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
    )?
    .execute(params![task.get(), kind, ns, sha, actor, meta.txn_id])?;
    Ok(())
}

/// `task`'s review state.
///
/// # Errors
/// A database error.
pub fn state(conn: &Connection, task: ItemId) -> Result<ReviewState> {
    Ok(state_for(conn, &[task])?.remove(&task).unwrap_or_default())
}

/// The review state of every task in `tasks`, in one query. A task with no record is absent.
///
/// # Errors
/// A database error.
pub fn state_for(conn: &Connection, tasks: &[ItemId]) -> Result<HashMap<ItemId, ReviewState>> {
    let mut out: HashMap<ItemId, ReviewState> = HashMap::new();
    if tasks.is_empty() {
        return Ok(out);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT item_id, kind, ns, sha FROM reviews
         WHERE item_id IN (SELECT value FROM json_each(?1)) ORDER BY id",
    )?;
    let rows = stmt.query_map(
        [crate::sql::json_ids(tasks.iter().map(|id| id.get()))],
        |r| {
            Ok((
                ItemId::new(r.get::<_, i64>(0)?),
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )?;
    for row in rows {
        let (id, kind, ns, sha) = row?;
        let s = out.entry(id).or_default();
        match kind.as_str() {
            "waived" => {
                s.waived = Some(sha);
                continue;
            }
            "gate_on_host" => {
                s.gate_on_host = Some(sha);
                continue;
            }
            _ => {}
        }
        s.reviewed = Some(sha);
        if let Some(ns) = ns {
            if !s.namespaces.contains(&ns) {
                s.namespaces.push(ns);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{record, state, waive};
    use crate::task::{create, NewTask};
    use crate::Db;

    #[test]
    fn rounds_accumulate_in_order_and_the_newest_head_is_the_reviewed_one() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .write_txn("t", |c, m| create(c, m, &NewTask::new("task:r", "R")))
            .unwrap();
        db.write_txn("t", move |c, m| {
            record(c, m, id, "reviews/2", "bbb", "reviewer@a1")?;
            record(c, m, id, "reviews/1", "aaa", "reviewer@a1")?;
            record(c, m, id, "reviews/1", "aaa", "reviewer@a1")?;
            waive(c, m, id, "ccc", "operator")?;
            super::gate_on_host(c, m, id, "ddd", "operator")
        })
        .unwrap();
        let s = db.read(move |c| state(c, id)).unwrap();
        assert_eq!(
            s.namespaces,
            vec!["reviews/2", "reviews/1"],
            "recording order, deduplicated"
        );
        assert_eq!(
            s.reviewed.as_deref(),
            Some("aaa"),
            "the newest record's head"
        );
        assert_eq!(s.waived.as_deref(), Some("ccc"));
        assert_eq!(s.gate_on_host.as_deref(), Some("ddd"));
        assert_eq!(s.reviewed.as_deref(), Some("aaa"), "neither is a review");
        assert!(db
            .write_txn("t", move |c, m| record(c, m, id, "", "x", "a"))
            .is_err());
    }
}
