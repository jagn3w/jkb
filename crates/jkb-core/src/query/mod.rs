//! The typed query engine: an AST over the item substrate and an evaluator that
//! compiles it to one parameterized `SQLite` query (design D13 — filter on real
//! columns/tags, not JSON).
//!
//! The AST is usable programmatically ([`Query`]); [`parse`] builds it from the CLI
//! DSL. Evaluation returns the **candidate item ids** matching the structured
//! predicates (kind/status/priority/due/tags/scope/`is:ready`/`blocks:`) and any FTS
//! `match`. A `~"…"` vector term is carried on the AST but *not* applied here — it is
//! a ranking route owned by `jkb-search` (task 8.3: structured filter narrows the
//! candidate set before ranking).

mod parse;

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};

use jkb_types::{ItemId, NamespaceId};

use crate::{ns, Result};

pub use parse::parse;

/// A comparison operator for numeric/date/tag predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

impl CmpOp {
    /// The SQL spelling of the operator.
    #[must_use]
    pub fn as_sql(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

/// A `due` operand: an explicit date, or `today` (resolved against the DB clock).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DueValue {
    /// `due:today` — compared to `date('now', 'localtime')`.
    Today,
    /// An explicit ISO date, e.g. `2025-12-31`.
    Date(String),
}

/// A tag predicate: `facet <op> value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagPred {
    /// The tag facet (e.g. `size`, `read_year`).
    pub facet: String,
    /// The comparison operator.
    pub op: CmpOp,
    /// The value to compare against (compared as `TEXT`; see the crate docs on
    /// ordinal facets).
    pub value: String,
}

/// A namespace scope: exact, a subtree (`/**`), or a union of scopes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Scope {
    /// All namespaces (no scope filter).
    #[default]
    All,
    /// Exactly this namespace path.
    Exact(String),
    /// This namespace and all descendants.
    Subtree(String),
    /// The union of several scopes (results from any of them).
    Union(Vec<Scope>),
}

/// A typed query over the item substrate. Build it directly or via [`parse`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// `kind:<k>` — exact item kind.
    pub kind: Option<String>,
    /// `status:<s>` — exact status.
    pub status: Option<String>,
    /// `priority<op><n>`.
    pub priority: Option<(CmpOp, i64)>,
    /// `due<op><date>` / `due:today`.
    pub due: Option<(CmpOp, DueValue)>,
    /// `tag:<facet><op><value>` predicates (all must hold).
    pub tags: Vec<TagPred>,
    /// Namespace scope (default [`Scope::All`]).
    pub scope: Scope,
    /// `is:ready` — non-terminal status with no unfinished `depends_on`.
    pub ready: bool,
    /// `blocks:<uid>` — items that block the item with this uid.
    pub blocks: Option<String>,
    /// A bare/quoted term for FTS `match`.
    pub fts: Option<String>,
    /// `~"…"` — a vector-similarity ranking term (applied by `jkb-search`, not here).
    pub vector: Option<String>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

impl Query {
    /// Evaluate the structured filter (plus any FTS `match`) and return the matching
    /// item ids, ordered by id. The `~"…"` vector term is ignored here (see the
    /// module docs).
    ///
    /// # Errors
    /// Returns an error if a scope path is malformed or a statement fails.
    pub fn evaluate(&self, conn: &Connection) -> Result<Vec<ItemId>> {
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<Value> = Vec::new();

        if let Some(kind) = &self.kind {
            clauses.push("i.kind = ?".to_owned());
            params.push(Value::Text(kind.clone()));
        }
        if let Some(status) = &self.status {
            clauses.push("i.status = ?".to_owned());
            params.push(Value::Text(status.clone()));
        }
        if let Some((op, n)) = self.priority {
            clauses.push(format!("i.priority {} ?", op.as_sql()));
            params.push(Value::Integer(n));
        }
        if let Some((op, due)) = &self.due {
            match due {
                DueValue::Today => {
                    clauses.push("date(i.due) = date('now', 'localtime')".to_owned());
                }
                DueValue::Date(d) => {
                    clauses.push(format!("date(i.due) {} date(?)", op.as_sql()));
                    params.push(Value::Text(d.clone()));
                }
            }
        }
        for tag in &self.tags {
            clauses.push(format!(
                "i.id IN (SELECT item_id FROM tag_applications WHERE facet = ? AND value {} ?)",
                tag.op.as_sql()
            ));
            params.push(Value::Text(tag.facet.clone()));
            params.push(Value::Text(tag.value.clone()));
        }
        if let Some(ids) = resolve_scope(conn, &self.scope)? {
            if ids.is_empty() {
                // A scope that resolves to no namespaces matches nothing.
                clauses.push("1 = 0".to_owned());
            } else {
                let placeholders = vec!["?"; ids.len()].join(", ");
                clauses.push(format!(
                    "i.id IN (SELECT item_id FROM placements WHERE namespace_id IN ({placeholders}))"
                ));
                params.extend(ids.into_iter().map(Value::Integer));
            }
        }
        if self.ready {
            // A task is ready if it is itself unsettled and every `depends_on` target is
            // settled. The settled set is `done`/`cancelled`/`needs_review` (see
            // `TaskStatus::unblocks_dependents`): a cancelled dependency will never
            // complete, and a `needs_review` one is finished enough to proceed on, so both
            // unblock rather than block their dependents. `IS NOT` keeps NULL-status rows
            // (non-tasks) treated as unsettled, matching the pre-`needs_review` behaviour.
            clauses.push(
                "i.status IS NOT 'done' AND i.status IS NOT 'cancelled'
                   AND i.status IS NOT 'needs_review'
                 AND NOT EXISTS (
                     SELECT 1 FROM edges e JOIN items d ON e.dst_item_id = d.id
                     WHERE e.src_item_id = i.id AND e.type = 'depends_on'
                       AND d.status IS NOT 'done' AND d.status IS NOT 'cancelled'
                       AND d.status IS NOT 'needs_review'
                 )"
                .to_owned(),
            );
        }
        if let Some(uid) = &self.blocks {
            clauses.push(
                "i.id IN (
                     SELECT e.dst_item_id FROM edges e JOIN items s ON e.src_item_id = s.id
                     WHERE s.uid = ? AND e.type = 'depends_on'
                 )"
                .to_owned(),
            );
            params.push(Value::Text(uid.clone()));
        }
        if let Some(term) = &self.fts {
            clauses
                .push("i.id IN (SELECT rowid FROM fts_items WHERE fts_items MATCH ?)".to_owned());
            params.push(Value::Text(term.clone()));
        }

        let mut sql = "SELECT i.id FROM items i".to_owned();
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY i.id");
        if let Some(limit) = self.limit {
            sql.push_str(" LIMIT ?");
            params.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        }

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), |row| {
                Ok(ItemId::new(row.get(0)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Resolve a scope to the set of namespace ids it covers. `None` means "all
/// namespaces" (no filter); `Some(vec![])` means "no namespace" (matches nothing).
fn resolve_scope(conn: &Connection, scope: &Scope) -> Result<Option<Vec<i64>>> {
    match scope {
        Scope::All => Ok(None),
        Scope::Exact(path) => Ok(Some(
            ns::get(conn, path)?
                .into_iter()
                .map(NamespaceId::get)
                .collect(),
        )),
        Scope::Subtree(path) => Ok(Some(
            ns::subtree(conn, path)?
                .into_iter()
                .map(|(id, _)| id.get())
                .collect(),
        )),
        Scope::Union(scopes) => {
            let mut ids = Vec::new();
            for s in scopes {
                if let Some(part) = resolve_scope(conn, s)? {
                    ids.extend(part);
                } else {
                    // A union containing "all" is "all".
                    return Ok(None);
                }
            }
            ids.sort_unstable();
            ids.dedup();
            Ok(Some(ids))
        }
    }
}
