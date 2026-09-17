//! `jkb inv …` (investigations, design Dmem) as a client of `inv.read`/`inv.write` (tasks S6.4 stage 5).
//! The renderings are the ones these commands printed before they were ported.

use anyhow::{Context as _, Result};
use jkb_api::inv::{InvAnswer, InvRead, InvWrite, NewEdge, Unit};
use jkb_api::{Request, Response};
use jkb_core::{investigation, nstype};
use jkb_types::{EdgeType, Resolution};

use crate::ops_cli::{unexpected, Ops};
use crate::{first_line, InvCmd};

fn read(ops: &Ops<'_>, ask: InvRead) -> Result<InvAnswer> {
    match ops.call(Request::InvRead(ask))? {
        Response::Inv { answer, .. } => Ok(answer),
        other => unexpected("inv.read", &other),
    }
}

fn write(ops: &Ops<'_>, ask: InvWrite) -> Result<InvAnswer> {
    match ops.call(Request::InvWrite(ask))? {
        Response::Inv { answer, .. } => Ok(answer),
        other => unexpected("inv.write", &other),
    }
}

fn wrong(op: &str, answer: &InvAnswer) -> anyhow::Error {
    anyhow::anyhow!("internal: {op} answered {answer:?}")
}

fn units(ops: &Ops<'_>, ask: InvRead) -> Result<Vec<Unit>> {
    match read(ops, ask)? {
        InvAnswer::Units { units, at_node_cap } => {
            if at_node_cap {
                eprintln!("{}", node_cap_notice());
            }
            Ok(units)
        }
        other => Err(wrong("inv.read", &other)),
    }
}

/// The namespace an investigation `name` lives at: as given under `memory/`, else under the ambient
/// repo's `memory/<repo>/` (the first segment after `repos/` of the mount the command runs in).
fn investigation_path(ops: &Ops<'_>, name: &str, global: bool) -> Result<String> {
    let root = investigation::MEMORY_ROOT;
    if name == root || name.starts_with(&format!("{root}/")) {
        return Ok(name.to_owned());
    }
    if global {
        return Ok(format!("{root}/{name}"));
    }
    let repo = ops.ambient_here()?.and_then(|mount| {
        mount
            .strip_prefix("repos/")
            .unwrap_or(&mount)
            .split('/')
            .next()
            .map(str::to_owned)
    });
    Ok(match repo {
        Some(repo) => format!("{root}/{repo}/{name}"),
        None => format!("{root}/{name}"),
    })
}

/// The investigation strategy governing `ns`, refusing an untyped namespace and one typed with a
/// *contract* (design D33.1) — a contract type has no verbs, frontier or acceptance predicate.
fn investigation_strategy(ops: &Ops<'_>, ns: &str) -> Result<&'static dyn nstype::NamespaceType> {
    let (source, type_name) = match read(ops, InvRead::Type { ns: ns.to_owned() })? {
        InvAnswer::Type {
            source: Some(source),
            type_name: Some(type_name),
        } => (source, type_name),
        InvAnswer::Type { .. } => anyhow::bail!("`{ns}` is not an investigation namespace"),
        other => return Err(wrong("inv.read", &other)),
    };
    let strategy = nstype::resolve(&type_name).with_context(|| {
        format!("`{ns}` is typed `{type_name}`, which this build does not know")
    })?;
    anyhow::ensure!(
        strategy.role() == nstype::TypeRole::Investigation,
        "`{ns}` is typed `{}` (from `{source}`), a contract that {} — it is not an \
         investigation, so it has no verbs or frontier",
        strategy.name(),
        strategy.about()
    );
    Ok(strategy)
}

/// Print a bucket of investigation units, human or JSON.
fn print_units(units: &[Unit], json: bool, show_rank: bool) {
    if json {
        let arr: Vec<serde_json::Value> = units
            .iter()
            .map(|u| {
                serde_json::json!({
                    "uid": u.uid,
                    "kind": u.kind,
                    "resolution": u.resolution,
                    "rank": u.rank,
                    "evidence": u.evidence,
                    "namespace": u.namespace,
                    "snippet": u.snippet.as_deref().map(first_line),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(arr)).unwrap_or_default()
        );
    } else if units.is_empty() {
        println!("(empty)");
    } else {
        for u in units {
            let rank = if show_rank {
                format!(" rank {:.2}", u.rank)
            } else {
                String::new()
            };
            let evidence = if u.evidence.abs() < f64::EPSILON {
                String::new()
            } else {
                format!(" ev {:+.2}", u.evidence)
            };
            println!(
                "{:<34} [{}]{rank}{evidence} — {}",
                u.uid,
                u.kind,
                u.snippet.as_deref().map(first_line).unwrap_or_default(),
            );
        }
    }
}

/// Parse `--tag facet=value` arguments.
fn parse_tag_args(tags: &[String]) -> Result<Vec<(String, String)>> {
    tags.iter()
        .map(|t| {
            let (facet, value) = t
                .split_once('=')
                .with_context(|| format!("tag `{t}` must be `facet=value`"))?;
            if facet.is_empty() {
                anyhow::bail!("tag `{t}` needs a facet before `=`");
            }
            Ok((facet.to_owned(), value.to_owned()))
        })
        .collect()
}

fn node_cap_notice() -> String {
    format!(
        "jkb: stopped at {} items — the walk and the edge list are capped there everywhere",
        jkb_api::items::MAX_RELATED_NODES
    )
}

fn edge_names() -> String {
    EdgeType::ALL
        .iter()
        .map(|e| e.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `jkb inv …`.
///
/// # Errors
/// A malformed argument, or the op's refusal.
pub(crate) fn run(ops: &Ops<'_>, cmd: InvCmd, global: bool) -> Result<()> {
    let json = ops.json;
    match cmd {
        InvCmd::Ls => ls(ops),
        InvCmd::New {
            type_name,
            path,
            goal,
            accept,
            goal_kind,
        } => new(ops, &type_name, &path, &goal, accept, goal_kind, global),
        InvCmd::Verbs { ns } => verbs(ops, &ns),
        InvCmd::Kinds { ns } => kinds(ops, &ns),
        InvCmd::Frontier { ns, all, limit } => {
            print_units(
                &units(ops, InvRead::Frontier { ns, all, limit })?,
                json,
                true,
            );
            Ok(())
        }
        InvCmd::Core { ns } => {
            print_units(&units(ops, InvRead::Core { ns })?, json, false);
            Ok(())
        }
        InvCmd::Tombstones { ns } => tombstones(ops, ns),
        InvCmd::Retread { uid, depth } => {
            let units = units(ops, InvRead::Retread { uid, depth })?;
            if !json && units.is_empty() {
                println!("(nothing related has been ruled out — clear to proceed)");
                return Ok(());
            }
            print_units(&units, json, false);
            Ok(())
        }
        InvCmd::Evidence { uid } => evidence(ops, &uid),
        InvCmd::Digest { ns, dry_run } => digest(ops, ns, dry_run),
        InvCmd::Rollup { ns } => rollup(ops, ns),
        cmd @ (InvCmd::Do { .. } | InvCmd::Add { .. } | InvCmd::Link { .. }) => {
            unit_write(ops, cmd)
        }
        InvCmd::Promise { uid, value } => {
            write(ops, InvWrite::Promise { uid, value })?;
            if !json {
                println!("promise = {value}");
            }
            Ok(())
        }
        InvCmd::Resolve { uid, resolution } => {
            // `resolve_unit` owns the guard (a task's lifecycle is `status`, not `resolution`).
            write(ops, InvWrite::Resolve { uid, resolution })?;
            if !json {
                println!("resolution set (the unit is retained — link what changed it)");
            }
            Ok(())
        }
        InvCmd::Reopen { route, mechanism } => reopen(ops, route, mechanism),
        InvCmd::Stale { ns, window } => {
            let answer = write(ops, InvWrite::Stale { ns, window })?;
            let InvAnswer::Marked { uids } = answer else {
                return Err(wrong("inv.write", &answer));
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&uids)?);
            } else if uids.is_empty() {
                println!("(no observations went stale)");
            } else {
                for uid in &uids {
                    println!("{uid} -> staleness=stale (excluded, not deleted)");
                }
            }
            Ok(())
        }
    }
}

/// `jkb inv do`.
fn do_verb(ops: &Ops<'_>, cmd: InvCmd) -> Result<()> {
    let json = ops.json;
    match cmd {
        InvCmd::Do {
            ns,
            verb,
            text,
            target,
            weight,
            tags,
        } => {
            let answer = write(
                ops,
                InvWrite::Do {
                    ns,
                    verb,
                    text: text.join(" "),
                    on: target,
                    weight,
                    tags: parse_tag_args(&tags)?,
                },
            )?;
            let InvAnswer::Unit {
                uid,
                target_resolution,
            } = answer
            else {
                return Err(wrong("inv.write", &answer));
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "uid": uid, "target_resolution": target_resolution })
                );
            } else {
                println!("{uid}");
                if let Some(r) = target_resolution {
                    println!("target resolution -> {r}");
                }
            }
            Ok(())
        }
        _ => anyhow::bail!("internal: not `inv do`"),
    }
}

/// The `inv` writes that add a unit or an edge by hand.
fn unit_write(ops: &Ops<'_>, cmd: InvCmd) -> Result<()> {
    let json = ops.json;
    match cmd {
        cmd @ InvCmd::Do { .. } => do_verb(ops, cmd),
        InvCmd::Add {
            ns,
            kind,
            text,
            edges,
            weight,
            tags,
        } => {
            let tags = parse_tag_args(&tags)?;
            let mut parsed = Vec::new();
            for spec in &edges {
                let (type_name, target) = spec
                    .split_once(':')
                    .with_context(|| format!("edge `{spec}` must be `<type>:<target-uid>`"))?;
                EdgeType::from_str_opt(type_name)
                    .with_context(|| format!("unknown edge type `{type_name}`"))?;
                parsed.push(NewEdge {
                    edge: type_name.to_owned(),
                    target: target.to_owned(),
                });
            }
            let answer = write(
                ops,
                InvWrite::Add {
                    ns,
                    kind,
                    text: text.join(" "),
                    edges: parsed,
                    weight,
                    tags,
                },
            )?;
            let InvAnswer::Unit { uid, .. } = answer else {
                return Err(wrong("inv.write", &answer));
            };
            if json {
                println!("{}", serde_json::json!({"uid": uid}));
            } else {
                println!("{uid}");
            }
            Ok(())
        }
        InvCmd::Link {
            src,
            edge,
            dst,
            weight,
        } => {
            EdgeType::from_str_opt(&edge).with_context(|| {
                format!("unknown edge type `{edge}`; available: {}", edge_names())
            })?;
            write(
                ops,
                InvWrite::Link {
                    src,
                    edge,
                    dst,
                    weight,
                },
            )?;
            if !json {
                println!("linked");
            }
            Ok(())
        }
        _ => anyhow::bail!("internal: not a unit write"),
    }
}

fn ls(ops: &Ops<'_>) -> Result<()> {
    let answer = read(ops, InvRead::Ls {})?;
    let InvAnswer::List { rows } = answer else {
        return Err(wrong("inv.read", &answer));
    };
    if ops.json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| serde_json::json!({ "ns": r.ns, "type": r.type_name, "units": r.units }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if rows.is_empty() {
        println!(
            "(no investigations yet) available types: {}",
            nstype::AVAILABLE.join(", ")
        );
    } else {
        for r in &rows {
            println!("{:<40} [{}] {} unit(s)", r.ns, r.type_name, r.units);
        }
    }
    Ok(())
}

fn new(
    ops: &Ops<'_>,
    type_name: &str,
    path: &str,
    goal: &[String],
    accept: Option<String>,
    goal_kind: Option<String>,
    global: bool,
) -> Result<()> {
    let strategy = nstype::resolve_strategy(type_name)?;
    let ns_path = investigation_path(ops, path, global)?;
    // Default the goal unit to the strategy's own goal kind (`symptom`, `conjecture`, …) so the
    // seeded unit reads naturally in its investigation.
    let goal_kind = goal_kind.unwrap_or_else(|| {
        strategy
            .node_kinds()
            .iter()
            .find(|k| k.base == nstype::BaseKind::Goal)
            .map_or(nstype::KIND_GOAL, |k| k.kind)
            .to_owned()
    });
    let mut body = goal.join(" ");
    if body.trim().is_empty() {
        body = format!("(state the goal for {ns_path} here)");
    }
    let mut tags = Vec::new();
    if let Some(preset) = accept {
        // The presets belong to the STRATEGY, so one strategy's predicate can never be stamped onto
        // another's goal.
        let presets = strategy.acceptance_presets();
        anyhow::ensure!(
            !presets.is_empty(),
            "the `{}` strategy has no acceptance presets, so --accept does not apply to it; \
             state the bar in --goal instead",
            strategy.name()
        );
        let text = strategy.acceptance_text(&preset).with_context(|| {
            format!(
                "unknown acceptance preset `{preset}` for `{}`; expected one of {}",
                strategy.name(),
                presets.join(", ")
            )
        })?;
        // The acceptance predicate lives IN the goal body: every agent that picks this up must read
        // the same bar.
        body = format!("{body}\n\n{text}");
        tags.push((nstype::conjecture::FACET_ACCEPTANCE.to_owned(), preset));
    }
    let answer = write(
        ops,
        InvWrite::New {
            type_name: type_name.to_owned(),
            ns: ns_path.clone(),
            goal_kind,
            goal: body,
            tags,
        },
    )?;
    let InvAnswer::Created { goal_uid, existed } = answer else {
        return Err(wrong("inv.write", &answer));
    };
    if ops.json {
        println!(
            "{}",
            serde_json::json!({
                "ns": ns_path, "goal_uid": goal_uid, "type": strategy.name(),
                "created": !existed,
            })
        );
    } else if existed {
        println!(
            "investigation {ns_path} [{}] already exists — left as it is",
            strategy.name()
        );
        println!("goal: {goal_uid}");
        println!("next: jkb inv digest {ns_path}");
    } else {
        println!("created investigation {ns_path} [{}]", strategy.name());
        println!("goal: {goal_uid}");
        println!("next: jkb inv verbs {ns_path}");
    }
    Ok(())
}

fn verbs(ops: &Ops<'_>, ns: &str) -> Result<()> {
    let strategy = investigation_strategy(ops, ns)?;
    if ops.json {
        let arr: Vec<serde_json::Value> = strategy
            .verbs()
            .iter()
            .map(|v| {
                serde_json::json!({
                    "verb": v.verb, "creates": v.kind, "about": v.about,
                    "edge": v.edge.map(EdgeType::as_str),
                    "target": match v.target {
                        nstype::TargetRule::Required => "required",
                        nstype::TargetRule::Optional => "optional",
                        nstype::TargetRule::Forbidden => "none",
                    },
                    "resolves_target": v.resolves_target.map(Resolution::as_str),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else {
        println!("{} [{}]\n{}", ns, strategy.name(), strategy.about());
        for v in strategy.verbs() {
            let target = match v.target {
                nstype::TargetRule::Required => " --on <uid>",
                nstype::TargetRule::Optional => " [--on <uid>]",
                nstype::TargetRule::Forbidden => "",
            };
            println!("  {:<24}{target:<14} {}", v.verb, v.about);
        }
    }
    Ok(())
}

fn kinds(ops: &Ops<'_>, ns: &str) -> Result<()> {
    let strategy = investigation_strategy(ops, ns)?;
    let edges = strategy
        .edge_types()
        .iter()
        .map(|e| e.as_str())
        .collect::<Vec<_>>();
    if ops.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "type": strategy.name(),
                "base_kinds": nstype::BASE_KINDS,
                "kinds": strategy.node_kinds().iter().map(|k| serde_json::json!({
                    "kind": k.kind, "about": k.about,
                })).collect::<Vec<_>>(),
                "edges": edges,
            }))?
        );
    } else {
        println!("{} [{}]", ns, strategy.name());
        println!("base kinds: {}", nstype::BASE_KINDS.join(", "));
        for k in strategy.node_kinds() {
            println!("  {:<24} {}", k.kind, k.about);
        }
        println!("edges: {}", edges.join(", "));
    }
    Ok(())
}

fn tombstones(ops: &Ops<'_>, ns: String) -> Result<()> {
    let answer = read(ops, InvRead::Tombstones { ns })?;
    let InvAnswer::Tombstones { rows } = answer else {
        return Err(wrong("inv.read", &answer));
    };
    if ops.json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|t| {
                serde_json::json!({
                    "uid": t.unit.uid,
                    "kind": t.unit.kind,
                    "resolution": t.unit.resolution,
                    "snippet": t.unit.snippet.as_deref().map(first_line),
                    "killed_by": t.killed_by.iter().map(|k| serde_json::json!({
                        "edge": k.edge, "uid": k.uid,
                        "snippet": k.snippet.as_deref().map(first_line),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if rows.is_empty() {
        println!("(no dead ends recorded yet)");
    } else {
        for t in &rows {
            println!(
                "{:<34} [{}] {} — {}",
                t.unit.uid,
                t.unit.kind,
                t.unit.resolution.as_deref().unwrap_or("unresolved"),
                t.unit
                    .snippet
                    .as_deref()
                    .map(first_line)
                    .unwrap_or_default(),
            );
            for k in &t.killed_by {
                println!(
                    "    {} by {}: {}",
                    k.edge,
                    k.uid,
                    k.snippet.as_deref().map(first_line).unwrap_or_default()
                );
            }
            if t.killed_by.is_empty() {
                println!("    (no edge records why — link what killed it)");
            }
        }
    }
    Ok(())
}

fn evidence(ops: &Ops<'_>, uid: &str) -> Result<()> {
    let answer = read(
        ops,
        InvRead::Evidence {
            uid: uid.to_owned(),
        },
    )?;
    let InvAnswer::Evidence {
        balance,
        edges,
        at_node_cap,
    } = answer
    else {
        return Err(wrong("inv.read", &answer));
    };
    if at_node_cap {
        eprintln!("{}", node_cap_notice());
    }
    if ops.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "uid": uid,
                "balance": balance,
                "edges": edges.iter().map(|e| serde_json::json!({
                    "edge": e.edge,
                    "uid": e.uid,
                    "contribution": e.contribution,
                    "snippet": e.snippet.as_deref().map(first_line),
                })).collect::<Vec<_>>(),
            }))?
        );
    } else {
        println!("{uid}: balance {balance:+.2}");
        for e in &edges {
            println!(
                "  {:+.2} {:<12} {:<30} {}",
                e.contribution,
                e.edge,
                e.uid,
                e.snippet.as_deref().map(first_line).unwrap_or_default()
            );
        }
        if edges.is_empty() {
            println!("  (no supports/contradicts edges)");
        }
    }
    Ok(())
}

fn digest(ops: &Ops<'_>, ns: String, dry_run: bool) -> Result<()> {
    let answer = if dry_run {
        read(ops, InvRead::Digest { ns })?
    } else {
        write(ops, InvWrite::Digest { ns })?
    };
    let InvAnswer::Digest { uid, text } = answer else {
        return Err(wrong("inv", &answer));
    };
    match uid {
        None => print!("{text}"),
        Some(uid) if ops.json => println!("{}", serde_json::json!({"uid": uid, "digest": text})),
        Some(uid) => {
            print!("{text}");
            println!("\n(written to {uid})");
        }
    }
    Ok(())
}

fn rollup(ops: &Ops<'_>, ns: String) -> Result<()> {
    let answer = write(ops, InvWrite::Rollup { ns })?;
    let InvAnswer::Rolled { changed } = answer else {
        return Err(wrong("inv.write", &answer));
    };
    if ops.json {
        let arr: Vec<serde_json::Value> = changed
            .iter()
            .map(|(uid, from, to)| serde_json::json!({ "uid": uid, "from": from, "to": to }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if changed.is_empty() {
        println!("(every resolution already matches its edges)");
    } else {
        for (uid, from, to) in &changed {
            println!("{uid}: {from} -> {to}");
        }
    }
    Ok(())
}

fn reopen(ops: &Ops<'_>, route: String, mechanism: String) -> Result<()> {
    let answer = write(ops, InvWrite::Reopen { route, mechanism })?;
    let InvAnswer::Reopened {
        mechanism_kind,
        superseded_gaps,
    } = answer
    else {
        return Err(wrong("inv.write", &answer));
    };
    if ops.json {
        println!(
            "{}",
            serde_json::json!({
                "mechanism_kind": mechanism_kind,
                "superseded_gaps": superseded_gaps,
                "reopened": !superseded_gaps.is_empty(),
            })
        );
    } else if superseded_gaps.is_empty() {
        // Nothing was blocking it, so nothing was reopened — say so plainly.
        println!(
            "nothing to reopen: no open gap was blocking it (recorded the {mechanism_kind} as \
             informing the route)"
        );
    } else {
        println!("reopened on a new {mechanism_kind}");
        for uid in &superseded_gaps {
            println!("  superseded gap {uid}");
        }
    }
    Ok(())
}
