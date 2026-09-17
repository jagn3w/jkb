//! `jkb inv` through the ops (tasks S6.4 stage 5): investigations (design Dmem), read with `inv.read`
//! and written with `inv.write`.
//!
//! The engine is `jkb_core::investigation`'s, unchanged; these ops only carry its answers. A strategy's
//! verbs and kinds are static data, so `inv.read` answers only the namespace's type name and the client
//! looks the strategy up in its own build.
//!
//! **A write is held to the file roots** like a task write: an investigation namespace under a file
//! mount outside the roots is refused (a unit placed there is exported into that file), and so is a
//! unit filed outside them (an edge or a resolution on it reaches its file). A task's tasks.md line is
//! held to its round trip.

use jkb_core::investigation::{self, UnitRow};
use jkb_core::{edge, item, ns, nstype, WriteMeta};
use jkb_types::EdgeType;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::kb::Budget;
use crate::tasks::{
    check_line, line_problem, ns_writable, writable_id, FileRoots, MAX_QUICK_ADD_MODIFIERS,
};
use crate::{ApiError, ErrorCode};

fn invalid(why: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Invalid, why)
}

/// One investigation unit, as the bucket listings show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    /// Its uid.
    pub uid: String,
    /// Its kind.
    pub kind: String,
    /// How it ended.
    pub resolution: Option<String>,
    /// Its frontier rank.
    pub rank: f64,
    /// Its signed-evidence balance.
    pub evidence: f64,
    /// Its namespace.
    pub namespace: Option<String>,
    /// The first line of its body.
    pub snippet: Option<String>,
}

fn snippet(content: Option<&str>) -> Option<String> {
    content.map(|c| item::snippet(c, item::SNIPPET_CHARS))
}

impl From<UnitRow> for Unit {
    fn from(u: UnitRow) -> Self {
        Self {
            snippet: snippet(u.content.as_deref()),
            uid: u.uid,
            kind: u.kind,
            resolution: u.resolution,
            rank: u.rank,
            evidence: u.evidence,
            namespace: u.namespace,
        }
    }
}

/// What killed a dead end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Killer {
    /// The edge type.
    pub edge: String,
    /// The killer's uid.
    pub uid: String,
    /// The first line of its body.
    pub snippet: Option<String>,
}

/// A dead end and what killed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tombstone {
    /// The dead unit.
    pub unit: Unit,
    /// What killed it; empty when nothing records why.
    pub killed_by: Vec<Killer>,
}

/// One signed evidence edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// `supports` or `contradicts`.
    pub edge: String,
    /// The source unit's uid.
    pub uid: String,
    /// Its signed contribution.
    pub contribution: f64,
    /// The first line of its body.
    pub snippet: Option<String>,
}

/// One investigation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Investigation {
    /// Its namespace.
    pub ns: String,
    /// Its strategy.
    #[serde(rename = "type")]
    pub type_name: String,
    /// How many units it holds.
    pub units: usize,
}

/// An `inv.read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "read", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvRead {
    /// Every investigation.
    Ls {},
    /// The type governing a namespace.
    Type {
        /// The namespace.
        ns: String,
    },
    /// The live, unblocked units, ranked.
    Frontier {
        /// The investigation.
        ns: String,
        /// Keep units another agent is working on.
        #[serde(default)]
        all: bool,
        /// At most this many.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// The settled results.
    Core {
        /// The investigation.
        ns: String,
    },
    /// The dead ends and what killed each.
    Tombstones {
        /// The investigation.
        ns: String,
    },
    /// What near a unit has already been ruled out.
    Retread {
        /// The unit.
        uid: String,
        /// How far to look.
        depth: usize,
    },
    /// A unit's signed evidence.
    Evidence {
        /// The unit.
        uid: String,
    },
    /// The state digest, unwritten.
    Digest {
        /// The investigation.
        ns: String,
    },
}

/// An edge `inv.write` adds with a new unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEdge {
    /// The edge type.
    pub edge: String,
    /// The existing unit it points at.
    pub target: String,
}

/// An `inv.write`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "write", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvWrite {
    /// Start an investigation, or leave an existing one as it is.
    New {
        /// Its strategy.
        type_name: String,
        /// Its namespace, resolved by the client.
        ns: String,
        /// The goal unit's kind.
        goal_kind: String,
        /// The goal's body, acceptance text included.
        goal: String,
        /// Tags for the goal.
        #[serde(default)]
        tags: Vec<(String, String)>,
    },
    /// Write the state digest.
    Digest {
        /// The investigation.
        ns: String,
    },
    /// Reconcile every unit's resolution with its edges.
    Rollup {
        /// The investigation.
        ns: String,
    },
    /// Apply a strategy verb.
    Do {
        /// The investigation.
        ns: String,
        /// The verb.
        verb: String,
        /// The new unit's body.
        text: String,
        /// The unit it acts on.
        #[serde(default)]
        on: Option<String>,
        /// The edge's weight.
        #[serde(default)]
        weight: Option<f64>,
        /// Tags for the new unit.
        #[serde(default)]
        tags: Vec<(String, String)>,
    },
    /// Add a unit of any kind.
    Add {
        /// The investigation.
        ns: String,
        /// Its kind.
        kind: String,
        /// Its body.
        text: String,
        /// Edges from it.
        #[serde(default)]
        edges: Vec<NewEdge>,
        /// Their weight.
        #[serde(default)]
        weight: Option<f64>,
        /// Its tags.
        #[serde(default)]
        tags: Vec<(String, String)>,
    },
    /// Link two existing units.
    Link {
        /// The source.
        src: String,
        /// The edge type.
        edge: String,
        /// The destination.
        dst: String,
        /// The weight.
        #[serde(default)]
        weight: Option<f64>,
    },
    /// Set a unit's promise.
    Promise {
        /// The unit.
        uid: String,
        /// The promise.
        value: f64,
    },
    /// Set a unit's resolution.
    Resolve {
        /// The unit.
        uid: String,
        /// The resolution.
        resolution: String,
    },
    /// Reopen a route on a new mechanism.
    Reopen {
        /// The route.
        route: String,
        /// The mechanism.
        mechanism: String,
    },
    /// Mark a debugging investigation's observations stale.
    Stale {
        /// The investigation.
        ns: String,
        /// The current window.
        window: String,
    },
}

/// What an `inv.read` or `inv.write` answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum InvAnswer {
    /// Every investigation.
    List {
        /// The investigations.
        rows: Vec<Investigation>,
    },
    /// The type governing a namespace.
    Type {
        /// Where the type was found (the namespace or an ancestor); `None` for an untyped namespace.
        source: Option<String>,
        /// The type's name.
        type_name: Option<String>,
    },
    /// A bucket of units.
    Units {
        /// The units.
        units: Vec<Unit>,
        /// The walk behind them stopped at [`crate::items::MAX_RELATED_NODES`] (`retread`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        at_node_cap: bool,
    },
    /// The dead ends.
    Tombstones {
        /// The dead ends.
        rows: Vec<Tombstone>,
    },
    /// A unit's evidence.
    Evidence {
        /// The signed balance, over every edge.
        balance: f64,
        /// The edges behind it, strongest first — the first
        /// [`crate::items::MAX_RELATED_NODES`].
        edges: Vec<Evidence>,
        /// More edges than that point at the unit.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        at_node_cap: bool,
    },
    /// A digest.
    Digest {
        /// The digest item, when it was written.
        uid: Option<String>,
        /// The digest.
        text: String,
    },
    /// An investigation started.
    Created {
        /// Its goal unit.
        goal_uid: String,
        /// It already existed and was left as it was.
        existed: bool,
    },
    /// Resolutions a roll-up changed: `(uid, from, to)`.
    Rolled {
        /// The changes.
        changed: Vec<(String, String, String)>,
    },
    /// A unit created.
    Unit {
        /// Its uid.
        uid: String,
        /// The resolution stamped on the verb's target.
        #[serde(default)]
        target_resolution: Option<String>,
    },
    /// A write with nothing to report.
    Applied {},
    /// A route reopened.
    Reopened {
        /// The mechanism's kind.
        mechanism_kind: String,
        /// The gaps it superseded.
        superseded_gaps: Vec<String>,
    },
    /// Observations marked stale.
    Marked {
        /// Their uids.
        uids: Vec<String>,
    },
}

fn edge_type(name: &str) -> Result<EdgeType, ApiError> {
    EdgeType::from_str_opt(name).ok_or_else(|| {
        invalid(format!(
            "unknown edge type `{name}`; available: {}",
            EdgeType::ALL
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

fn require_id(conn: &Connection, uid: &str) -> Result<jkb_types::ItemId, ApiError> {
    item::id_for_uid(conn, uid)?.ok_or_else(|| {
        ApiError::with_code(ErrorCode::NotFound, format!("no item with uid `{uid}`"))
    })
}

/// Keep the prefix of `rows` that fits `budget`.
fn within<T: Serialize>(rows: impl IntoIterator<Item = T>, budget: &mut Budget) -> Vec<T> {
    rows.into_iter().take_while(|r| budget.take(r)).collect()
}

/// `inv.read`, each listing within `budget` and each walk within
/// [`crate::items::MAX_RELATED_NODES`].
///
/// **Residual, stated:** a bucket (`frontier`, `core`, `tombstones`) is computed over its whole
/// investigation before the budget cuts the answer, and `retread` reads the full body of each of the
/// (at most [`crate::items::MAX_RELATED_NODES`]) dead ends it reaches — the engine loads units whole,
/// as it does on the host.
///
/// # Errors
/// The engine's refusal (an untyped namespace, an unknown uid), or a failed read.
pub fn read(conn: &Connection, ask: &InvRead, budget: &mut Budget) -> Result<InvAnswer, ApiError> {
    let cap = crate::items::MAX_RELATED_NODES;
    let mut units = |rows: Vec<UnitRow>, at_node_cap: bool| InvAnswer::Units {
        units: within(rows.into_iter().map(Unit::from), budget),
        at_node_cap,
    };
    Ok(match ask {
        InvRead::Ls {} => InvAnswer::List {
            rows: within(
                investigation::list(conn)?
                    .into_iter()
                    .map(|r| Investigation {
                        ns: r.ns_path,
                        type_name: r.type_name.to_owned(),
                        units: r.units,
                    }),
                budget,
            ),
        },
        InvRead::Type { ns } => {
            let found = nstype::for_namespace(conn, ns)?;
            InvAnswer::Type {
                source: found.as_ref().map(|(source, _)| source.clone()),
                type_name: found.map(|(_, s)| s.name().to_owned()),
            }
        }
        InvRead::Frontier { ns, all, limit } => {
            units(investigation::frontier(conn, ns, *all, *limit)?, false)
        }
        InvRead::Core { ns } => units(investigation::confirmed_core(conn, ns)?, false),
        InvRead::Tombstones { ns } => InvAnswer::Tombstones {
            rows: within(
                investigation::tombstones(conn, ns)?
                    .into_iter()
                    .map(|t| Tombstone {
                        unit: t.unit.into(),
                        killed_by: t
                            .killed_by
                            .into_iter()
                            .map(|(e, uid, body)| Killer {
                                edge: e.as_str().to_owned(),
                                uid,
                                snippet: snippet(body.as_deref()),
                            })
                            .collect(),
                    }),
                budget,
            ),
        },
        InvRead::Retread { uid, depth } => {
            if *depth > crate::items::MAX_RELATED_DEPTH {
                return Err(invalid(format!(
                    "a depth of at most {}",
                    crate::items::MAX_RELATED_DEPTH
                )));
            }
            let (rows, at_cap) =
                investigation::anti_retread_limited(conn, require_id(conn, uid)?, *depth, cap)?;
            units(rows, at_cap)
        }
        InvRead::Evidence { uid } => {
            let id = require_id(conn, uid)?;
            let (edges, at_node_cap) = edge::evidence_edges_capped(conn, id, cap)?;
            let mut rows = Vec::new();
            for e in &edges {
                let Some((uid, _, _, _, snippet)) = crate::items::light_row(conn, e.src)? else {
                    continue;
                };
                let row = Evidence {
                    edge: e.edge_type.as_str().to_owned(),
                    uid,
                    contribution: e.contribution,
                    snippet,
                };
                if !budget.take(&row) {
                    break;
                }
                rows.push(row);
            }
            InvAnswer::Evidence {
                balance: edge::evidence_for(conn, id)?,
                edges: rows,
                at_node_cap,
            }
        }
        InvRead::Digest { ns } => InvAnswer::Digest {
            uid: None,
            text: investigation::digest(conn, ns)?.render(),
        },
    })
}

/// [`writable_id`] for a unit named by uid, and its tasks.md line's problem before the write.
fn unit_writable(
    conn: &Connection,
    uid: &str,
    roots: Option<&FileRoots>,
) -> Result<Option<String>, ApiError> {
    let id = require_id(conn, uid)?;
    writable_id(conn, id, uid, roots)?;
    line_problem(conn, uid)
}

/// Refuse more than [`MAX_QUICK_ADD_MODIFIERS`] edges or tags on one write: each edge target is judged
/// against the roots and its tasks.md line checked, twice, in the writer's transaction.
fn check_counts(edges: usize, tags: usize) -> Result<(), ApiError> {
    if edges > MAX_QUICK_ADD_MODIFIERS || tags > MAX_QUICK_ADD_MODIFIERS {
        return Err(invalid(format!(
            "at most {MAX_QUICK_ADD_MODIFIERS} edges and {MAX_QUICK_ADD_MODIFIERS} tags on one unit"
        )));
    }
    Ok(())
}

/// `inv.write`, in the caller's transaction.
///
/// # Errors
/// [`ErrorCode::Forbidden`] under `roots` (see the module docs), the engine's refusal, or a failed
/// write.
pub fn write(
    conn: &Connection,
    meta: &WriteMeta,
    ask: &InvWrite,
    roots: Option<&FileRoots>,
) -> Result<InvAnswer, ApiError> {
    let uid_of = |id| -> Result<String, ApiError> {
        Ok(item::get(conn, id)?.map(|m| m.uid).unwrap_or_default())
    };
    Ok(match ask {
        InvWrite::New {
            type_name,
            ns: path,
            goal_kind,
            goal,
            tags,
        } => {
            check_counts(0, tags.len())?;
            ns_writable(conn, path, roots)?;
            let existed = ns::get_type(conn, path)?.is_some();
            let id = investigation::create(conn, meta, path, type_name, goal_kind, goal, tags)?;
            InvAnswer::Created {
                goal_uid: uid_of(id)?,
                existed,
            }
        }
        InvWrite::Digest { ns: path } => {
            ns_writable(conn, path, roots)?;
            let (id, text) = investigation::write_digest(conn, meta, path)?;
            InvAnswer::Digest {
                uid: Some(uid_of(id)?),
                text,
            }
        }
        InvWrite::Rollup { ns: path } => {
            ns_writable(conn, path, roots)?;
            InvAnswer::Rolled {
                changed: investigation::roll_up(conn, meta, path)?
                    .into_iter()
                    .map(|(uid, from, to)| (uid, from.as_str().to_owned(), to.as_str().to_owned()))
                    .collect(),
            }
        }
        InvWrite::Do {
            ns: path,
            verb,
            text,
            on,
            weight,
            tags,
        } => {
            check_counts(0, tags.len())?;
            ns_writable(conn, path, roots)?;
            let before = on
                .as_deref()
                .map(|t| unit_writable(conn, t, roots))
                .transpose()?
                .flatten();
            let outcome = investigation::apply_verb(
                conn,
                meta,
                path,
                &investigation::VerbCall {
                    verb,
                    content: text,
                    target_uid: on.as_deref(),
                    weight: *weight,
                    tags,
                },
            )?;
            if let Some(t) = on {
                check_line(conn, t, before.as_deref())?;
            }
            InvAnswer::Unit {
                uid: outcome.uid,
                target_resolution: outcome.target_resolution.map(|r| r.as_str().to_owned()),
            }
        }
        InvWrite::Add { .. } => add_unit(conn, meta, ask, roots)?,
        InvWrite::Link { .. }
        | InvWrite::Promise { .. }
        | InvWrite::Resolve { .. }
        | InvWrite::Reopen { .. } => write_units(conn, meta, ask, roots)?,
        InvWrite::Stale { ns: path, window } => {
            ns_writable(conn, path, roots)?;
            InvAnswer::Marked {
                uids: nstype::debugging::mark_stale_observations(conn, meta, path, window)?,
            }
        }
    })
}

/// `inv.write`'s `add`.
fn add_unit(
    conn: &Connection,
    meta: &WriteMeta,
    ask: &InvWrite,
    roots: Option<&FileRoots>,
) -> Result<InvAnswer, ApiError> {
    let InvWrite::Add {
        ns: path,
        kind,
        text,
        edges,
        weight,
        tags,
    } = ask
    else {
        return Err(ApiError::with_code(ErrorCode::Internal, "not an add"));
    };
    check_counts(edges.len(), tags.len())?;
    ns_writable(conn, path, roots)?;
    let mut parsed = Vec::with_capacity(edges.len());
    let mut befores = std::collections::BTreeMap::new();
    for e in edges {
        if !befores.contains_key(&e.target) {
            let before = unit_writable(conn, &e.target, roots)?;
            befores.insert(e.target.clone(), before);
        }
        parsed.push((edge_type(&e.edge)?, e.target.clone(), *weight));
    }
    let id = investigation::add(
        conn,
        meta,
        &investigation::NewUnit {
            kind: kind.clone(),
            content: text.clone(),
            namespace: path.clone(),
            tags: tags.clone(),
            edges: parsed,
            reverse_edges: Vec::new(),
        },
    )?;
    for (target, before) in befores {
        check_line(conn, &target, before.as_deref())?;
    }
    Ok(InvAnswer::Unit {
        uid: item::get(conn, id)?.map(|m| m.uid).unwrap_or_default(),
        target_resolution: None,
    })
}

/// The `inv.write`s that name existing units rather than a namespace.
fn write_units(
    conn: &Connection,
    meta: &WriteMeta,
    ask: &InvWrite,
    roots: Option<&FileRoots>,
) -> Result<InvAnswer, ApiError> {
    Ok(match ask {
        InvWrite::Link {
            src,
            edge,
            dst,
            weight,
        } => {
            let edge = edge_type(edge)?;
            let (before_src, before_dst) = (
                unit_writable(conn, src, roots)?,
                unit_writable(conn, dst, roots)?,
            );
            investigation::link(conn, meta, src, edge, dst, *weight)?;
            check_line(conn, src, before_src.as_deref())?;
            check_line(conn, dst, before_dst.as_deref())?;
            InvAnswer::Applied {}
        }
        InvWrite::Promise { uid, value } => {
            let before = unit_writable(conn, uid, roots)?;
            investigation::set_promise(conn, meta, uid, *value)?;
            check_line(conn, uid, before.as_deref())?;
            InvAnswer::Applied {}
        }
        InvWrite::Resolve { uid, resolution } => {
            let before = unit_writable(conn, uid, roots)?;
            investigation::resolve_unit(conn, meta, uid, resolution)?;
            check_line(conn, uid, before.as_deref())?;
            InvAnswer::Applied {}
        }
        InvWrite::Reopen { route, mechanism } => {
            for uid in [route, mechanism] {
                unit_writable(conn, uid, roots)?;
            }
            if let Some(path) = item::primary_namespace(conn, require_id(conn, route)?)? {
                ns_writable(conn, &path, roots)?;
            }
            let outcome = investigation::reopen(conn, meta, route, mechanism)?;
            InvAnswer::Reopened {
                mechanism_kind: outcome.mechanism_kind,
                superseded_gaps: outcome.superseded_gaps,
            }
        }
        InvWrite::New { .. }
        | InvWrite::Digest { .. }
        | InvWrite::Rollup { .. }
        | InvWrite::Do { .. }
        | InvWrite::Add { .. }
        | InvWrite::Stale { .. } => {
            return Err(ApiError::with_code(
                ErrorCode::Internal,
                "a namespace write reached write_units",
            ))
        }
    })
}

#[cfg(test)]
mod tests;
