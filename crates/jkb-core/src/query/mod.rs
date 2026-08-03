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

use jkb_types::{ItemId, NamespaceId, Resolution};

use crate::{ns, Result};

pub use parse::parse;

/// "This item is unresolved" — NULL (never set) or the explicit `unresolved` string
/// (design Dmem.3). Shared by the `resolution:unresolved` predicate and `is:frontier` so
/// the two can never drift apart.
const UNRESOLVED_SELF: &str = "(i.resolution IS NULL OR i.resolution = 'unresolved')";

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
    /// `kind:<a>,<b>` — any of these kinds (union). Empty means "no kind filter";
    /// combined with [`Query::kind`] the item must match both, so set only one.
    pub kinds: Vec<String>,
    /// `-kind:<k>` — exclude these kinds. Lets a frontier ask for "work" without having to
    /// enumerate every kind that counts as work.
    pub exclude_kinds: Vec<String>,
    /// Restrict to these item ids. Not reachable from the DSL — it is for callers that
    /// already hold a candidate set and want a predicate applied to *those* rows rather than
    /// to the whole table (an empty vec means "no id restriction").
    pub ids: Vec<ItemId>,
    /// `status:<s>` — exact status.
    pub status: Option<String>,
    /// `resolution:<r>` — the outcome axis (design Dmem.3). `unresolved` also matches a
    /// NULL column, which is how every pre-existing item reads.
    pub resolution: Option<String>,
    /// `priority<op><n>`.
    pub priority: Option<(CmpOp, i64)>,
    /// `due<op><date>` / `due:today`.
    pub due: Option<(CmpOp, DueValue)>,
    /// `tag:<facet><op><value>` predicates (all must hold).
    pub tags: Vec<TagPred>,
    /// `-tag:<facet><op><value>` predicates — items matching **any** of these are
    /// excluded. Lets a strategy's frontier drop, say, stale observations without
    /// resorting to raw SQL.
    pub exclude_tags: Vec<TagPred>,
    /// Namespace scope (default [`Scope::All`]).
    pub scope: Scope,
    /// `is:ready` — non-terminal status with no unfinished `depends_on`.
    pub ready: bool,
    /// `is:frontier` — the **generalized** frontier (design Dmem.3/Dmem.5): unresolved,
    /// non-terminal, and not blocked by anything still unresolved. Strictly more general
    /// than [`Query::ready`]: for a task (whose `resolution` is NULL) it selects exactly
    /// the same rows. The claim filter is *not* bundled in — set
    /// [`Query::claimed`] to `Some(false)` to hand work out safely (which is what the
    /// `is:frontier` DSL term and every descriptor's frontier query do).
    pub frontier: bool,
    /// `is:tombstone` — the anti-retread set (design Dmem.5): resolved `dead_end`/
    /// `superseded`, or killed by an incident `refutes`/`rules_out` edge.
    pub tombstone: bool,
    /// `is:claimed` / `is:unclaimed` — filter on live agent claims (design D27.1).
    /// `None` means "either".
    pub claimed: Option<bool>,
    /// `blocks:<uid>` — items that block the item with this uid.
    pub blocks: Option<String>,
    /// A bare/quoted term for FTS `match`.
    pub fts: Option<String>,
    /// `~"…"` — a vector-similarity ranking term (applied by `jkb-search`, not here).
    pub vector: Option<String>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

/// A unit with an unfinished `parent_of` child is not itself workable — the leaves are.
///
/// Shared verbatim by `is:ready` and `is:frontier` because those two must stay equivalent
/// for tasks (a task's `resolution` is always NULL, so the frontier clauses collapse onto
/// the ready ones). Putting this rule in only one of them would break that the moment a
/// task had a subtask.
const SUBTASK_CLAUSE: &str = "
                 AND NOT EXISTS (
                     SELECT 1 FROM edges pe JOIN items c ON pe.dst_item_id = c.id
                     WHERE pe.src_item_id = i.id AND pe.type = 'parent_of'
                       AND c.status IS NOT 'done' AND c.status IS NOT 'cancelled'
                 )";

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

        self.push_column_clauses(&mut clauses, &mut params);
        self.push_tag_clauses(&mut clauses, &mut params);
        self.push_scope_clause(conn, &mut clauses, &mut params)?;
        self.push_graph_clauses(&mut clauses, &mut params);
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

    /// Predicates over `items`' own columns: kind, status, resolution, priority, due.
    fn push_column_clauses(&self, clauses: &mut Vec<String>, params: &mut Vec<Value>) {
        if let Some(kind) = &self.kind {
            clauses.push("i.kind = ?".to_owned());
            params.push(Value::Text(kind.clone()));
        }
        if !self.kinds.is_empty() {
            let placeholders = vec!["?"; self.kinds.len()].join(", ");
            clauses.push(format!("i.kind IN ({placeholders})"));
            params.extend(self.kinds.iter().cloned().map(Value::Text));
        }
        if !self.exclude_kinds.is_empty() {
            let placeholders = vec!["?"; self.exclude_kinds.len()].join(", ");
            clauses.push(format!("i.kind NOT IN ({placeholders})"));
            params.extend(self.exclude_kinds.iter().cloned().map(Value::Text));
        }
        if !self.ids.is_empty() {
            let placeholders = vec!["?"; self.ids.len()].join(", ");
            clauses.push(format!("i.id IN ({placeholders})"));
            params.extend(self.ids.iter().map(|id| Value::Integer(id.get())));
        }
        if let Some(status) = &self.status {
            clauses.push("i.status = ?".to_owned());
            params.push(Value::Text(status.clone()));
        }
        if let Some(resolution) = &self.resolution {
            // NULL is the stored form of `unresolved` (design Dmem.3: no back-fill), so the
            // default value must match rows that were never given one.
            if resolution == Resolution::Unresolved.as_str() {
                clauses.push(UNRESOLVED_SELF.to_owned());
            } else {
                clauses.push("i.resolution = ?".to_owned());
                params.push(Value::Text(resolution.clone()));
            }
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
    }

    /// Positive (`tag:`) and negated (`-tag:`) facet predicates.
    fn push_tag_clauses(&self, clauses: &mut Vec<String>, params: &mut Vec<Value>) {
        for tag in &self.tags {
            clauses.push(format!(
                "i.id IN (SELECT item_id FROM tag_applications WHERE facet = ? AND value {} ?)",
                tag.op.as_sql()
            ));
            params.push(Value::Text(tag.facet.clone()));
            params.push(Value::Text(tag.value.clone()));
        }
        for tag in &self.exclude_tags {
            clauses.push(format!(
                "i.id NOT IN (SELECT item_id FROM tag_applications WHERE facet = ? AND value {} ?)",
                tag.op.as_sql()
            ));
            params.push(Value::Text(tag.facet.clone()));
            params.push(Value::Text(tag.value.clone()));
        }
    }

    /// The namespace-scope predicate (via any placement).
    fn push_scope_clause(
        &self,
        conn: &Connection,
        clauses: &mut Vec<String>,
        params: &mut Vec<Value>,
    ) -> Result<()> {
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
        Ok(())
    }

    /// Predicates over the edge graph and claim state: `is:ready`, `is:frontier`,
    /// `is:tombstone`, `is:claimed`/`is:unclaimed`, `blocks:`.
    fn push_graph_clauses(&self, clauses: &mut Vec<String>, params: &mut Vec<Value>) {
        if self.ready {
            // A task is ready if its own status is non-terminal and every `depends_on`
            // target is settled. The settled set is exactly the **terminal** statuses
            // `done`/`cancelled` (see `TaskStatus::unblocks_dependents`, design D27.7): a
            // cancelled dependency will never complete, so it unblocks; a `needs_review`
            // dependency is *not* settled — its work is not yet landed and may bounce
            // back, so it blocks. `IS NOT` keeps NULL-status rows (non-tasks) treated as
            // unsettled.
            //
            // A live claim (any non-null `claimant_id`) also excludes a task from the
            // frontier (design D27.1): work already in flight must not be handed out
            // twice. This is a plain column predicate — no anti-join. The owner-existence
            // reclaim NULLs a dead owner's `claimant_id`, which drops the task back in.
            //
            // A task with an unfinished **subtask** is likewise off the frontier (design
            // D34.3): `parent_of` runs parent -> child, so a parent is a container and the
            // leaves are the units of work. Handing out the parent would have an agent
            // claim work that is really several pieces, which is the situation subtasks
            // exist to split.
            clauses.push(format!(
                "i.claimant_id IS NULL
                 AND i.status IS NOT 'done' AND i.status IS NOT 'cancelled'
                 AND NOT EXISTS (
                     SELECT 1 FROM edges e JOIN items d ON e.dst_item_id = d.id
                     WHERE e.src_item_id = i.id AND e.type = 'depends_on'
                       AND d.status IS NOT 'done' AND d.status IS NOT 'cancelled'
                 ){SUBTASK_CLAUSE}"
            ));
        }
        if self.frontier {
            // The generalized frontier (design Dmem.3): a unit is on it when it is itself
            // still live — non-terminal `status` AND unresolved `resolution` — and nothing
            // it `depends_on` is still live. "Settled" therefore means *either* axis has
            // concluded: a dependency that is `done` (task axis) or `dead_end`/`success`
            // (outcome axis) will never change again, so waiting on it forever would strand
            // its dependents. `IS NOT` (not `NOT IN`) is deliberate — it is NULL-safe, and
            // both columns are NULL for the kinds that do not use them.
            //
            // For a task, `resolution` is always NULL, so these clauses collapse to exactly
            // the `is:ready` anti-join above: one frontier concept, two vocabularies.
            //
            // The claim filter is NOT bundled here (see `Query::claimed`) so a coordinator
            // can ask for the whole frontier including in-flight work; the `is:frontier`
            // DSL term and the strategy descriptors add `claimed = Some(false)`.
            clauses.push(format!(
                "i.status IS NOT 'done' AND i.status IS NOT 'cancelled'
                 AND {UNRESOLVED_SELF}
                 AND NOT EXISTS (
                     SELECT 1 FROM edges e JOIN items d ON e.dst_item_id = d.id
                     WHERE e.src_item_id = i.id AND e.type = 'depends_on'
                       AND d.status IS NOT 'done' AND d.status IS NOT 'cancelled'
                       AND (d.resolution IS NULL OR d.resolution = 'unresolved')
                 ){SUBTASK_CLAUSE}"
            ));
        }
        if self.tombstone {
            // The anti-retread set: somebody already tried this. Either the unit carries a
            // tombstone resolution, or an edge records what killed it — `refutes` (this
            // specific unit was disproved) or `rules_out` (an obstruction eliminated the
            // whole region it lives in). Both readings matter: the resolution is the
            // summary, the edge is the reason, and a unit can have the edge before anyone
            // got round to setting the resolution.
            clauses.push(
                "(i.resolution IN ('dead_end', 'superseded')
                  OR EXISTS (
                      SELECT 1 FROM edges e
                      WHERE e.dst_item_id = i.id AND e.type IN ('refutes', 'rules_out')
                  ))"
                .to_owned(),
            );
        }
        if let Some(claimed) = self.claimed {
            clauses.push(if claimed {
                "i.claimant_id IS NOT NULL".to_owned()
            } else {
                "i.claimant_id IS NULL".to_owned()
            });
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
