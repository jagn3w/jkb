//! `jkb doctor` and `jkb task reclaim` as clients of the ops (tasks S6.4 stage 5, design-s6-4.md H, I).
//!
//! The database's own health is `kb.health`, and claims are listed with `task.claims`, probed here —
//! where the owners' processes and checkouts are — and freed with `task.reclaim`. So both commands say
//! the same thing on the host and in the dev container. What only the host has — the embedder, the
//! database file, the old worktree-removal store — is printed only when this process holds the
//! database ([`Host`]), and `--fix`/`--backup` need it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use jkb_api::claims::Claim;
use jkb_core::Db;
use jkb_fsm::Fact;

use crate::session_cli::Kb;

/// The held claims, sorted by what probing their owners here established.
#[derive(Default)]
pub(crate) struct Probed {
    /// How many claims are held.
    pub(crate) held: usize,
    /// Owner proven gone.
    pub(crate) gone: Vec<Claim>,
    /// Owner whose liveness nothing here can establish.
    pub(crate) unknown: Vec<Claim>,
}

impl Probed {
    /// The owners proven gone, once each.
    fn dead_owners(&self) -> Vec<String> {
        let mut owners: Vec<String> = self.gone.iter().map(|c| c.owner.clone()).collect();
        owners.sort_unstable();
        owners.dedup();
        owners
    }
}

/// Every held claim, its owner probed once ([`crate::owner::is_alive`]); `keep` owners are alive by
/// fiat and never probed, so a live coordinator passing its own id never reclaims its own work.
///
/// **Reported, never freed, when unestablished**: an owner whose liveness cannot be established from
/// here keeps its claim, because reclaiming on an unestablished answer frees a live agent's task
/// (design S3.2).
///
/// # Errors
/// The op's failure.
pub(crate) fn probe(kb: &Kb<'_>, keep: &[String]) -> Result<Probed> {
    probe_with(kb, keep, crate::owner::is_alive)
}

fn probe_with(kb: &Kb<'_>, keep: &[String], alive: impl Fn(&str) -> Fact) -> Result<Probed> {
    let claims = kb.claims()?;
    let mut facts: BTreeMap<String, Fact> = BTreeMap::new();
    let mut out = Probed {
        held: claims.len(),
        ..Probed::default()
    };
    for c in claims {
        let fact = *facts.entry(c.owner.clone()).or_insert_with(|| {
            if keep.iter().any(|k| k == &c.owner) {
                Fact::Yes
            } else {
                alive(&c.owner)
            }
        });
        match fact {
            Fact::No => out.gone.push(c),
            Fact::Unknown => out.unknown.push(c),
            Fact::Yes => {}
        }
    }
    Ok(out)
}

/// What a reclaim did, as the commands report it.
struct Freed {
    probed: Probed,
    cleared: Vec<Claim>,
    /// Claims of a proven-gone owner the op left held, with why.
    held_back: Vec<(Claim, String)>,
}

fn free(kb: &Kb<'_>, probed: Probed) -> Result<Freed> {
    if probed.gone.is_empty() {
        return Ok(Freed {
            probed,
            cleared: Vec::new(),
            held_back: Vec::new(),
        });
    }
    // In batches the op takes. Each owner string is its own compare-and-set, so nothing is lost by
    // asking about them in pieces.
    let mut answer = jkb_api::claims::Reclaimed::default();
    for batch in probed
        .dead_owners()
        .chunks(jkb_api::claims::MAX_DEAD_OWNERS)
    {
        let part = kb.reclaim(batch.to_vec())?;
        answer.cleared.extend(part.cleared);
        answer.refused.extend(part.refused);
        answer.unwritable.extend(part.unwritable);
    }
    let mut held_back = Vec::new();
    for c in &probed.gone {
        if answer.cleared.contains(c) {
            continue;
        }
        let why = answer
            .refused
            .iter()
            .find(|r| r.owner == c.owner)
            .map(|r| r.reason.clone())
            .or_else(|| {
                answer.unwritable.contains(c).then(|| {
                    "filed outside the directories this client may write; run `jkb task reclaim` \
                     on the host"
                        .to_owned()
                })
            })
            .unwrap_or_else(|| "released or taken again meanwhile".to_owned());
        held_back.push((c.clone(), why));
    }
    Ok(Freed {
        probed,
        cleared: answer.cleared,
        held_back,
    })
}

/// `jkb task reclaim`: free every claim whose owner is proven gone.
///
/// # Errors
/// The ops' failures.
pub(crate) fn reclaim(kb: &Kb<'_>, keep: &[String], json: bool) -> Result<()> {
    let freed = free(kb, probe(kb, keep)?)?;
    if json {
        let uids = |cs: &[Claim]| cs.iter().map(|c| c.uid.clone()).collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::json!({
                "held": freed.probed.held,
                "reclaimed": uids(&freed.cleared),
                // Owners nothing here can prove gone.
                "unverifiable": uids(&freed.probed.unknown),
                // Owners proven gone whose claims the op did not free, and why.
                "held_back": freed.held_back.iter().map(|(c, why)| serde_json::json!({
                    "uid": c.uid, "owner": c.owner, "reason": why,
                })).collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }
    println!(
        "reclaimed {} of {} claim(s)",
        freed.cleared.len(),
        freed.probed.held
    );
    for c in &freed.cleared {
        println!("  {} (dead owner {})", c.uid, c.owner);
    }
    for (c, why) in &freed.held_back {
        println!("  {} still held by {} — {why}", c.uid, c.owner);
    }
    // Reported, never freed: of the two ways to be wrong, this is the one that costs a command.
    for c in &freed.probed.unknown {
        println!(
            "  {} still held by {} — liveness cannot be checked from here; \
             `jkb task release {} --owner {}` if you know it is gone",
            c.uid, c.owner, c.uid, c.owner
        );
    }
    Ok(())
}

/// The claims half of `jkb doctor` (design D27.1/D27.2, S3.2): a bare run reports, `fix` frees the
/// claims whose owner is proven gone.
fn report_claims(kb: &Kb<'_>, fix: bool) -> Result<()> {
    let probed = probe(kb, &[])?;
    if probed.held == 0 {
        println!("task claims: none held");
        return Ok(());
    }
    if probed.gone.is_empty() && probed.unknown.is_empty() {
        println!("task claims: {} held, all owners alive", probed.held);
        return Ok(());
    }
    let unknown = probed.unknown.clone();
    if !probed.gone.is_empty() {
        println!(
            "task claims: {} orphaned (owner gone) of {} held",
            probed.gone.len(),
            probed.held,
        );
        for c in &probed.gone {
            println!("  {} claimed by dead owner {}", c.uid, c.owner);
        }
        if fix {
            let freed = free(kb, probed)?;
            println!("  cleared {} orphaned claim(s)", freed.cleared.len());
            for (c, why) in &freed.held_back {
                println!("  {} not cleared — {why}", c.uid);
            }
        } else {
            println!("  run `jkb task reclaim` (or `jkb doctor --fix` on the host) to clear them");
        }
    }
    // Its own bucket, because it is its own answer: not "the owner is gone" but "nothing here can
    // tell". Neither `--fix` nor `task reclaim` touches these (design S3.2).
    if !unknown.is_empty() {
        println!(
            "task claims: {} held by an owner whose liveness cannot be checked here",
            unknown.len(),
        );
        for c in &unknown {
            println!("  {} claimed by {}", c.uid, c.owner);
        }
        println!(
            "  these are NOT auto-reclaimed — `jkb task release <uid> --owner <owner>` once you \
             know the owner is gone"
        );
    }
    Ok(())
}

/// What only the process holding the database has.
pub(crate) struct Host<'a> {
    pub(crate) db: &'a Db,
    pub(crate) path: &'a Path,
}

/// `jkb doctor`. `backup` and `fix` need `host`: remote mode refuses them before this runs.
///
/// # Errors
/// A failed op or host step.
pub(crate) fn run(
    kb: &Kb<'_>,
    host: Option<&Host<'_>>,
    backup: Option<&Path>,
    fix: bool,
) -> Result<()> {
    anyhow::ensure!(
        host.is_some() || (backup.is_none() && !fix),
        "`jkb doctor --fix` and `--backup` change the host's database, so they run on the host"
    );
    // FIRST, before any diagnostic and before `--fix` mutates anything: `--backup` is the safety copy
    // taken *before* a repair.
    if let (Some(dest), Some(h)) = (backup, host) {
        h.db.backup(dest)?;
        println!("backup written to {}", dest.display());
    }
    if let Some(h) = host {
        crate::report_embedder(h.db);
    }

    let health = kb.health()?;
    println!(
        "fts integrity: {}",
        if health.fts_ok { "ok" } else { "FAILED" }
    );
    println!("schema user_version: {}", health.schema_version);

    // Files needing sync attention: conflicts and quarantined parse failures (D25).
    if health.flagged_count == 0 {
        println!("sync journal: ok");
    } else {
        println!(
            "sync journal: {} file(s) need attention",
            health.flagged_count
        );
        for s in &health.flagged {
            let detail = s.detail.as_deref().unwrap_or("both sides changed");
            println!("  {} [{}]: {detail}", s.uri, s.status);
        }
        if health.flagged_count > health.flagged.len() {
            println!(
                "  … and {} more",
                health.flagged_count - health.flagged.len()
            );
        }
    }

    report_claims(kb, fix)?;

    let swept = match (host, fix) {
        (Some(h), true) if health.vector_tables > 0 => {
            // Destructured exhaustively: a new derived index is a compile error here.
            let jkb_index::StaleRows { vectors } = crate::sweep_stale(h.db)?;
            Some(vectors)
        }
        _ => None,
    };
    for line in vector_report(&health, swept) {
        println!("{line}");
    }

    // Task sessions in this repo (design D36.6). A session's worktree keeps its claim on purpose, so a
    // session is never reported as orphaned; doctor lists every one.
    crate::report_sessions(kb);
    // Directories under `.jkb/work` git no longer lists: what nothing above is tracking.
    crate::report_untracked_sessions(kb, fix);

    if let Some(h) = host {
        crate::report_worktree_removals(h.db, h.path, fix);
        // Cloud-sync-folder warning (design D23).
        match jkb_core::cloud_sync_warning(h.path) {
            Some(w) => println!("warning: {w}"),
            None => println!("db location: ok ({})", h.path.display()),
        }
    }
    Ok(())
}

/// The vector-index lines of `jkb doctor`. `swept` is the stale-row count `--fix` removed, when it
/// ran.
///
/// Healthy needs two things, not one: no stale rows now, and the delete-GC trigger on every vector
/// table so no more appear. A database written before the trigger can have no stale rows today and
/// still orphan a vector on every item delete, and reporting it `ok` on the row count alone hid
/// exactly that. The repair (`sweep_stale`, behind both `jkb index --sweep` and `--fix`) installs
/// the trigger, so the `--fix` arm reports the tables it guarded.
fn vector_report(health: &jkb_api::health::Health, swept: Option<usize>) -> Vec<String> {
    if health.vector_tables == 0 {
        return vec!["vector index: no vector table yet".to_owned()];
    }
    let unguarded = health.unguarded_vector_tables.as_deref();
    if let Some(removed) = swept {
        let mut lines = vec![format!("vector index: removed {removed} stale row(s)")];
        if let Some(tables) = unguarded.filter(|t| !t.is_empty()) {
            lines.push(format!(
                "vector index: installed the delete-GC trigger on {}",
                tables.join(", ")
            ));
        }
        return lines;
    }
    let mut lines = Vec::new();
    if health.stale_vectors > 0 {
        lines.push(format!(
            "vector index: {} stale row(s) whose item is gone",
            health.stale_vectors
        ));
    }
    match unguarded {
        Some([]) => {}
        Some(tables) => lines.push(format!(
            "vector index: no delete-GC trigger on {}, so every item delete leaves its vector behind",
            tables.join(", ")
        )),
        None => lines.push(
            "vector index: the GC trigger was not checked (the daemon predates the check)"
                .to_owned(),
        ),
    }
    if lines.is_empty() {
        return vec!["vector index: ok".to_owned()];
    }
    if health.stale_vectors > 0 || unguarded.is_some_and(|t| !t.is_empty()) {
        lines.push(
            "  run `jkb index --sweep` (or `jkb doctor --fix`) on the host to repair it".to_owned(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{free, probe_with, vector_report};
    use jkb_api::health::Health;

    fn health(vector_tables: usize, stale: usize, unguarded: Option<&[&str]>) -> Health {
        Health {
            schema_version: 0,
            fts_ok: true,
            flagged: Vec::new(),
            flagged_count: 0,
            vector_tables,
            stale_vectors: stale,
            unguarded_vector_tables: unguarded.map(|t| t.iter().map(|s| (*s).to_owned()).collect()),
        }
    }

    /// A legacy database with no stale rows yet but no GC trigger is NOT reported ok: the row count
    /// alone hid that every item delete still orphans a vector.
    #[test]
    fn a_missing_gc_trigger_is_not_reported_ok() {
        let lines = vector_report(&health(1, 0, Some(&["vec_items_16"])), None);
        assert!(
            !lines.iter().any(|l| l.ends_with(": ok")),
            "an unguarded table was reported healthy: {lines:?}"
        );
        assert!(lines[0].contains("no delete-GC trigger on vec_items_16"));
        assert!(lines.last().unwrap().contains("jkb index --sweep"));

        // Healthy needs both: no stale rows AND every table guarded.
        assert_eq!(
            vector_report(&health(1, 0, Some(&[])), None),
            ["vector index: ok"]
        );
        // Stale rows and a missing trigger are both reported, with one repair hint.
        let both = vector_report(&health(1, 3, Some(&["vec_items_16"])), None);
        assert_eq!(both.len(), 3, "{both:?}");
        // An older daemon that cannot say is not read as healthy either.
        let unknown = vector_report(&health(1, 0, None), None);
        assert!(unknown[0].contains("not checked"), "{unknown:?}");
        // `--fix` installs it, and says so.
        let fixed = vector_report(&health(1, 2, Some(&["vec_items_16"])), Some(2));
        assert_eq!(
            fixed,
            [
                "vector index: removed 2 stale row(s)",
                "vector index: installed the delete-GC trigger on vec_items_16"
            ]
        );
        assert_eq!(
            vector_report(&health(0, 0, Some(&[])), None),
            ["vector index: no vector table yet"]
        );
    }
    use crate::session_cli::Kb;
    use jkb_api::{Backend as _, LocalBackend};
    use jkb_core::Db;
    use jkb_fsm::Fact;

    /// Each owner is probed once, a kept owner never, and only a proven absence is sent to be freed.
    #[test]
    fn owners_are_probed_once_and_only_the_proven_gone_are_freed() {
        let backend = LocalBackend::new(Db::open_in_memory().unwrap());
        let mut uids = Vec::new();
        for (text, owner) in [
            ("a", "box:1"),
            ("b", "box:1"),
            ("c", "box:2"),
            ("d", "box:3"),
            ("e", "box:4"),
        ] {
            let added = backend
                .call(
                    serde_json::from_value(serde_json::json!({
                        "op": "task.add", "text": text, "managed": true
                    }))
                    .unwrap(),
                )
                .unwrap();
            let jkb_api::Response::Added { added } = added else {
                panic!("added")
            };
            backend
                .call(
                    serde_json::from_value(serde_json::json!({
                        "op": "task.claim", "uid": added.uid, "owner": owner
                    }))
                    .unwrap(),
                )
                .unwrap();
            uids.push(added.uid);
        }
        let kb = Kb::new(&backend);
        let asked = std::cell::RefCell::new(Vec::new());
        let probed = probe_with(&kb, &["box:4".to_owned()], |o| {
            asked.borrow_mut().push(o.to_owned());
            match o {
                "box:1" => Fact::No,
                "box:2" => Fact::Unknown,
                _ => Fact::Yes,
            }
        })
        .unwrap();
        assert_eq!(asked.into_inner(), ["box:1", "box:2", "box:3"]);
        assert_eq!(probed.held, 5);
        assert_eq!(probed.dead_owners(), ["box:1"]);
        assert_eq!(probed.unknown.len(), 1);
        let freed = free(&kb, probed).unwrap();
        let cleared: Vec<&str> = freed.cleared.iter().map(|c| c.uid.as_str()).collect();
        assert_eq!(cleared, [uids[0].as_str(), uids[1].as_str()]);
        assert!(freed.held_back.is_empty());
        assert_eq!(kb.claims().unwrap().len(), 3);
    }
}
