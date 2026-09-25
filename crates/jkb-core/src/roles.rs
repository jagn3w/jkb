//! Roles, grants and the agent-type map (design D52.2, D52.3, D52.9;
//! `openspec/changes/jkb-rbac-workflows/`).
//!
//! **Who is asking** is answered here and nowhere else: a bearer token resolves to a [`GrantRow`] (a
//! role, optionally scoped to one task), and an attested subagent's `agent_type` resolves to a role
//! through the operator-owned map ([`role_for_agent_type`]). What each role may *do* is not here —
//! the op table lives with the ops (`jkb-api`) and the workflow events' table with the strategies
//! ([`crate::workflow`]) — because a permission is a statement about an action, and the action's
//! module is where a new one is added.
//!
//! **Only hashes are stored.** A token is 256 random bits handed out once; the table keeps its
//! blake3 hash. A copy of the database is then not a copy of anybody's credentials.
//!
//! **Not changelogged**, like `claude_sessions`: `jkb undo` reviving a revoked grant would re-arm a
//! credential, and undoing a mint would strand a worker mid-task with no audit of why. Revocation
//! is its own op, and it is recursive — a grant takes everything it minted with it.

use rusqlite::{params, Connection, OptionalExtension};

use jkb_rbac::{Grant, RoleTable};
use jkb_types::{Error as TypeError, ItemId};

use crate::store::WriteMeta;
use crate::{Error, Result};

/// A role a caller can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// The human. Holds everything, and is the only role whose credential never enters the dev
    /// container.
    Operator,
    /// The session driving a task: spawns workers, advances the workflow, never approves what the
    /// operator approves unless the task's strategy toggles say so.
    Coordinator,
    /// Gathers details and writes the design.
    Designer,
    /// Writes the code.
    Implementer,
    /// Reviews it and files findings.
    Reviewer,
    /// Looks for the systemic cause when review keeps finding the same areas.
    SystemicReviewer,
}

impl jkb_rbac::Role for Role {
    const ALL: &'static [Self] = &[
        Self::Operator,
        Self::Coordinator,
        Self::Designer,
        Self::Implementer,
        Self::Reviewer,
        Self::SystemicReviewer,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Coordinator => "coordinator",
            Self::Designer => "designer",
            Self::Implementer => "implementer",
            Self::Reviewer => "reviewer",
            Self::SystemicReviewer => "systemic_reviewer",
        }
    }
}

/// A role is also a *permission* in one table: which roles a holder may grant.
impl jkb_rbac::Permission for Role {
    const ALL: &'static [Self] = <Self as jkb_rbac::Role>::ALL;

    fn name(self) -> &'static str {
        <Self as jkb_rbac::Role>::name(self)
    }
}

impl Role {
    /// The stored and printed name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        jkb_rbac::Role::name(self)
    }

    /// Parse a name, for the CLI and for reading rows back.
    ///
    /// # Errors
    /// [`Error::Types`] naming the roles that exist.
    pub fn parse(name: &str) -> Result<Self> {
        <Self as jkb_rbac::Role>::parse(name).ok_or_else(|| {
            let known: Vec<&str> = <Self as jkb_rbac::Role>::ALL
                .iter()
                .map(|r| r.as_str())
                .collect();
            Error::Types(TypeError::Validation(format!(
                "no role `{name}` (roles: {})",
                known.join(", ")
            )))
        })
    }

    /// The four roles a coordinator hands to the agents it spawns.
    #[must_use]
    pub const fn is_worker(self) -> bool {
        matches!(
            self,
            Self::Designer | Self::Implementer | Self::Reviewer | Self::SystemicReviewer
        )
    }
}

/// Which role may grant which (design D52.2). The operator grants anything; a coordinator grants
/// the four worker roles, and [`mint`] additionally holds it to its own scope.
pub static GRANTABLE: RoleTable<Role, Role> = RoleTable {
    grants: &[
        Grant {
            role: Role::Operator,
            permits: &[
                Role::Operator,
                Role::Coordinator,
                Role::Designer,
                Role::Implementer,
                Role::Reviewer,
                Role::SystemicReviewer,
            ],
        },
        Grant {
            role: Role::Coordinator,
            permits: &[
                Role::Designer,
                Role::Implementer,
                Role::Reviewer,
                Role::SystemicReviewer,
            ],
        },
        Grant {
            role: Role::Designer,
            permits: &[],
        },
        Grant {
            role: Role::Implementer,
            permits: &[],
        },
        Grant {
            role: Role::Reviewer,
            permits: &[],
        },
        Grant {
            role: Role::SystemicReviewer,
            permits: &[],
        },
    ],
};

/// The `agent` label of the dev container's own credential (D52.3).
pub const CONTAINER_AGENT: &str = "container";

/// The longest agent label a grant records.
const MAX_AGENT_BYTES: usize = 128;

/// One grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRow {
    /// Its id — what `jkb role revoke` names.
    pub id: i64,
    /// The role it confers.
    pub role: Role,
    /// Who it was handed to: a free-form label, recorded in every history row it writes.
    pub agent: String,
    /// The one task it may act on; `None` for an unscoped grant.
    pub scope: Option<ItemId>,
    /// The grant that minted it, if another grant did (the operator mints with none).
    pub parent: Option<i64>,
    /// When.
    pub granted_at: String,
    /// When it was revoked, if it was.
    pub revoked_at: Option<String>,
}

fn validation(msg: impl Into<String>) -> Error {
    Error::Types(TypeError::Validation(msg.into()))
}

/// The stored form of a token: its blake3 hash, hex.
#[must_use]
pub fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// 256 random bits, hex.
///
/// # Errors
/// [`Error::Types`] if the platform has no randomness to give.
pub fn fresh_token() -> Result<String> {
    use std::fmt::Write as _;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| validation(format!("no randomness: {e}")))?;
    Ok(bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

fn check_agent(agent: &str) -> Result<()> {
    if agent.is_empty()
        || agent.len() > MAX_AGENT_BYTES
        || !agent
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ':'))
    {
        return Err(validation(format!(
            "an agent label of 1 to {MAX_AGENT_BYTES} letters, digits and `-_.@:` (got `{agent}`)"
        )));
    }
    Ok(())
}

const GRANT_COLUMNS: &str = "id, role, agent, item_id, parent_id, granted_at, revoked_at";

/// A `role_grants` row as read, before its role is parsed.
type RawGrant = (
    i64,
    String,
    String,
    Option<i64>,
    Option<i64>,
    String,
    Option<String>,
);

fn grant_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawGrant> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

fn into_grant(
    (id, role, agent, item, parent, granted_at, revoked_at): RawGrant,
) -> Result<GrantRow> {
    Ok(GrantRow {
        id,
        role: Role::parse(&role)?,
        agent,
        scope: item.map(ItemId::new),
        parent,
        granted_at,
        revoked_at,
    })
}

/// Who is minting: the operator (no grant), or a grant holding [`GRANTABLE`] rights.
#[derive(Debug, Clone, Copy)]
pub enum Minter<'a> {
    /// The operator: any role, any scope.
    Operator,
    /// A grant: only roles [`GRANTABLE`] lets its role grant, and within its own scope.
    Grant(&'a GrantRow),
}

/// Mint a grant of `role` to `agent`, optionally scoped to `scope`, returning it and its token —
/// the only time the token exists outside the caller that hands it on.
///
/// A grant minted by another grant is refused unless [`GRANTABLE`] lets the minter's role grant
/// `role`, and an unscoped minter's child may be scoped anywhere while a scoped minter's child must
/// be scoped to the same task: a coordinator on one task cannot mint a reviewer for another.
///
/// # Errors
/// [`Error::Types`] for a refused or malformed grant, or a database error.
pub fn mint(
    conn: &Connection,
    meta: &WriteMeta,
    minter: Minter<'_>,
    role: Role,
    agent: &str,
    scope: Option<ItemId>,
) -> Result<(GrantRow, String)> {
    use jkb_rbac::Grants as _;
    check_agent(agent)?;
    let parent = match minter {
        Minter::Operator => None,
        Minter::Grant(g) => {
            if g.revoked_at.is_some() {
                return Err(validation("a revoked grant mints nothing"));
            }
            if !GRANTABLE.permits(g.role, role) {
                return Err(validation(format!(
                    "a {} may not grant {} (it may grant: {})",
                    g.role.as_str(),
                    role.as_str(),
                    GRANTABLE
                        .permissions_of(g.role)
                        .iter()
                        .map(|r| r.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            if g.scope.is_some() && scope != g.scope {
                return Err(validation(
                    "a grant scoped to a task may mint only grants scoped to that same task",
                ));
            }
            Some(g.id)
        }
    };
    let token = fresh_token()?;
    let id: i64 = conn
        .prepare_cached(
            "INSERT INTO role_grants (token_hash, role, agent, item_id, parent_id, granted_at, txn_id)
             VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?6)
             RETURNING id",
        )?
        .query_row(
            params![
                token_hash(&token),
                role.as_str(),
                agent,
                scope.map(ItemId::get),
                parent,
                meta.txn_id
            ],
            |r| r.get(0),
        )?;
    let row = get(conn, id)?.ok_or_else(|| validation("the grant vanished as it was written"))?;
    Ok((row, token))
}

/// A grant by id, revoked or not.
///
/// # Errors
/// A database error.
pub fn get(conn: &Connection, id: i64) -> Result<Option<GrantRow>> {
    conn.prepare_cached(&format!(
        "SELECT {GRANT_COLUMNS} FROM role_grants WHERE id = ?1"
    ))?
    .query_row([id], grant_row)
    .optional()?
    .map(into_grant)
    .transpose()
}

/// The live grant `token` names, if any. A revoked grant, and a grant whose ancestor was revoked
/// (revocation is recursive, so that is the same thing), resolves to nothing.
///
/// # Errors
/// A database error.
pub fn resolve(conn: &Connection, token: &str) -> Result<Option<GrantRow>> {
    conn.prepare_cached(&format!(
        "SELECT {GRANT_COLUMNS} FROM role_grants WHERE token_hash = ?1 AND revoked_at IS NULL"
    ))?
    .query_row([token_hash(token)], grant_row)
    .optional()?
    .map(into_grant)
    .transpose()
}

/// Every live grant with its token hash, for a daemon's in-memory resolution cache.
///
/// # Errors
/// A database error.
pub fn live_by_hash(conn: &Connection) -> Result<Vec<(String, GrantRow)>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT token_hash, {GRANT_COLUMNS} FROM role_grants WHERE revoked_at IS NULL"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            (
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ),
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (hash, g) = row?;
        out.push((hash, into_grant(g)?));
    }
    Ok(out)
}

/// A number that changes whenever a grant is minted or revoked — what a daemon's cache compares to
/// know it is stale without re-reading every row.
///
/// # Errors
/// A database error.
pub fn generation(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn
        .prepare_cached("SELECT coalesce(max(id), 0), count(revoked_at) FROM role_grants")?
        .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))?)
}

/// Grants, newest first: only those scoped to `scope` when given, and revoked ones only when
/// `include_revoked`.
///
/// # Errors
/// A database error.
pub fn list(
    conn: &Connection,
    scope: Option<ItemId>,
    include_revoked: bool,
) -> Result<Vec<GrantRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {GRANT_COLUMNS} FROM role_grants
         WHERE (?1 IS NULL OR item_id = ?1) AND (?2 OR revoked_at IS NULL)
         ORDER BY id DESC"
    ))?;
    let rows = stmt.query_map(params![scope.map(ItemId::get), include_revoked], grant_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(into_grant(row?)?);
    }
    Ok(out)
}

/// Revoke grant `id` and, recursively, everything it minted. Returns how many grants this revoked
/// (zero for one already revoked).
///
/// # Errors
/// A database error.
pub fn revoke(conn: &Connection, _meta: &WriteMeta, id: i64) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM role_grants WHERE id = ?1
                 UNION SELECT g.id FROM role_grants g JOIN tree t ON g.parent_id = t.id
             )
             UPDATE role_grants SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id IN (SELECT id FROM tree) AND revoked_at IS NULL",
        )?
        .execute([id])?)
}

/// Revoke every live grant scoped to `task` (and what they minted) — what a task reaching a settled
/// workflow phase does, so no worker outlives the work it was for.
///
/// # Errors
/// A database error.
pub fn revoke_task(conn: &Connection, meta: &WriteMeta, task: ItemId) -> Result<usize> {
    let ids: Vec<i64> = conn
        .prepare_cached("SELECT id FROM role_grants WHERE item_id = ?1 AND revoked_at IS NULL")?
        .query_map([task.get()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut n = 0;
    for id in ids {
        n += revoke(conn, meta, id)?;
    }
    Ok(n)
}

/// Replace the dev container's credential: revoke every live `container` grant (and what it
/// minted) and mint a fresh unscoped coordinator one. Returns the new grant and token. Host-only:
/// the op layer refuses it to anyone but the operator.
///
/// # Errors
/// A database error.
pub fn rotate_container(conn: &Connection, meta: &WriteMeta) -> Result<(GrantRow, String)> {
    let ids: Vec<i64> = conn
        .prepare_cached(
            "SELECT id FROM role_grants WHERE agent = ?1 AND parent_id IS NULL AND revoked_at IS NULL",
        )?
        .query_map([CONTAINER_AGENT], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        revoke(conn, meta, id)?;
    }
    mint(
        conn,
        meta,
        Minter::Operator,
        Role::Coordinator,
        CONTAINER_AGENT,
        None,
    )
}

/// Map a Claude Code `agent_type` (a subagent definition's `name`) to a role, or clear it with
/// `None`. Operator-only at the op layer: this is what makes an attested subagent a reviewer.
///
/// # Errors
/// [`Error::Types`] for a malformed agent type, or a database error.
pub fn map_agent_type(
    conn: &Connection,
    _meta: &WriteMeta,
    agent_type: &str,
    role: Option<Role>,
) -> Result<()> {
    check_agent(agent_type)?;
    match role {
        Some(role) => {
            conn.prepare_cached(
                "INSERT INTO agent_role_map (agent_type, role) VALUES (?1, ?2)
                 ON CONFLICT(agent_type) DO UPDATE SET role = excluded.role",
            )?
            .execute(params![agent_type, role.as_str()])?;
        }
        None => {
            conn.prepare_cached("DELETE FROM agent_role_map WHERE agent_type = ?1")?
                .execute([agent_type])?;
        }
    }
    Ok(())
}

/// The role an attested `agent_type` holds, if the operator mapped one.
///
/// # Errors
/// A database error.
pub fn role_for_agent_type(conn: &Connection, agent_type: &str) -> Result<Option<Role>> {
    conn.prepare_cached("SELECT role FROM agent_role_map WHERE agent_type = ?1")?
        .query_row([agent_type], |r| r.get::<_, String>(0))
        .optional()?
        .map(|r| Role::parse(&r))
        .transpose()
}

/// The whole agent-type map, sorted.
///
/// # Errors
/// A database error.
pub fn agent_type_map(conn: &Connection) -> Result<Vec<(String, Role)>> {
    let mut stmt =
        conn.prepare_cached("SELECT agent_type, role FROM agent_role_map ORDER BY agent_type")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (t, r) = row?;
        out.push((t, Role::parse(&r)?));
    }
    Ok(out)
}

/// What [`bind_agent`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Bound now, or already bound to this task.
    To(ItemId),
    /// Already bound to a different task: the first bind wins (D52.9).
    Elsewhere(ItemId),
}

/// Bind an attested subagent (`session`, `agent_id`) to `task`, first bind wins. A worker cannot
/// hop to a second task once it has acted on one.
///
/// # Errors
/// A database error.
pub fn bind_agent(
    conn: &Connection,
    _meta: &WriteMeta,
    session: &str,
    agent_id: &str,
    task: ItemId,
) -> Result<Bound> {
    check_agent(agent_id)?;
    conn.prepare_cached(
        "INSERT INTO agent_bindings (session, agent_id, item_id, bound_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT(session, agent_id) DO NOTHING",
    )?
    .execute(params![session, agent_id, task.get()])?;
    let bound = agent_binding(conn, session, agent_id)?
        .ok_or_else(|| validation("the binding vanished as it was written"))?;
    Ok(if bound == task {
        Bound::To(task)
    } else {
        Bound::Elsewhere(bound)
    })
}

/// The task an attested subagent is bound to, if it has bound one.
///
/// # Errors
/// A database error.
pub fn agent_binding(conn: &Connection, session: &str, agent_id: &str) -> Result<Option<ItemId>> {
    Ok(conn
        .prepare_cached("SELECT item_id FROM agent_bindings WHERE session = ?1 AND agent_id = ?2")?
        .query_row([session, agent_id], |r| r.get::<_, i64>(0))
        .optional()?
        .map(ItemId::new))
}

/// The deepest containment chain [`in_scope`] walks before giving up — far past any real subtask tree,
/// and a bound on a cycle a hand-edited database could hold.
const MAX_SCOPE_DEPTH: usize = 64;

/// Whether `target` is inside `scope`'s reach: the task itself, one of its subtasks at any depth, or
/// a finding filed in one of its recorded review rounds. What a principal scoped to a task may write.
///
/// # Errors
/// A database error.
pub fn in_scope(conn: &Connection, scope: ItemId, target: ItemId) -> Result<bool> {
    let mut at = Some(target);
    for _ in 0..MAX_SCOPE_DEPTH {
        match at {
            Some(id) if id == scope => return Ok(true),
            Some(id) => at = crate::containment::parent(conn, id)?,
            None => break,
        }
    }
    let rounds = crate::reviews::state(conn, scope)?.namespaces;
    if rounds.is_empty() {
        return Ok(false);
    }
    let homes: Vec<String> = conn
        .prepare_cached(
            "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
             WHERE p.item_id = ?1",
        )?
        .query_map([target.get()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(homes.iter().any(|h| {
        rounds.iter().any(|ns| {
            h == ns
                || h.strip_prefix(ns.as_str())
                    .is_some_and(|t| t.starts_with('/'))
        })
    }))
}

/// Whether grant `id` is `ancestor` or was minted, directly or not, by it — what a grant holder may
/// revoke.
///
/// # Errors
/// A database error.
pub fn descends_from(conn: &Connection, id: i64, ancestor: i64) -> Result<bool> {
    let mut at = Some(id);
    for _ in 0..MAX_SCOPE_DEPTH {
        match at {
            Some(g) if g == ancestor => return Ok(true),
            Some(g) => at = get(conn, g)?.and_then(|r| r.parent),
            None => break,
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests;
