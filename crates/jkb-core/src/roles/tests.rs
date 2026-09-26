use jkb_rbac::Grants as _;
use jkb_types::ItemId;

use super::{
    agent_binding, agent_type_map, bind_agent, descends_from, generation, in_scope, list,
    live_by_hash, map_agent_type, mint, resolve, revoke, role_for_agent_type, rotate_container,
    token_hash, Bound, Minter, Role, CONTAINER_AGENT, GRANTABLE,
};
use crate::task::{create, NewTask};
use crate::Db;

fn tasks(db: &Db) -> (ItemId, ItemId) {
    db.write_txn("test", |c, m| {
        Ok((
            create(c, m, &NewTask::new("task:a", "A"))?,
            create(c, m, &NewTask::new("task:b", "B"))?,
        ))
    })
    .unwrap()
}

#[test]
fn the_grant_table_is_sound_and_only_the_operator_mints_a_coordinator() {
    assert_eq!(GRANTABLE.check(), vec![]);
    assert!(GRANTABLE.permits(Role::Coordinator, Role::Implementer));
    // A reviewer a coordinator minted would be the coordinator reviewing its own work.
    assert!(!GRANTABLE.permits(Role::Coordinator, Role::Reviewer));
    assert!(!GRANTABLE.permits(Role::Coordinator, Role::SystemicReviewer));
    assert!(!GRANTABLE.permits(Role::Coordinator, Role::Coordinator));
    assert!(!GRANTABLE.permits(Role::Reviewer, Role::Reviewer));
    assert_eq!(
        Role::parse("systemic_reviewer").unwrap(),
        Role::SystemicReviewer
    );
    assert!(Role::parse("root")
        .unwrap_err()
        .to_string()
        .contains("roles: operator"));
}

#[test]
fn a_token_resolves_to_its_grant_and_only_its_hash_is_stored() {
    let db = Db::open_in_memory().unwrap();
    let (a, _) = tasks(&db);
    let (g, token) = db
        .write_txn("test", move |c, m| {
            mint(c, m, Minter::Operator, Role::Coordinator, "sess-1", Some(a))
        })
        .unwrap();
    assert_eq!(token.len(), 64);
    let t = token.clone();
    let found = db.read(move |c| resolve(c, &t)).unwrap().unwrap();
    assert_eq!(found, g);
    assert_eq!(found.scope, Some(a));
    let stored: Vec<(String, _)> = db.read(live_by_hash).unwrap();
    assert_eq!(stored[0].0, token_hash(&token));
    assert_ne!(stored[0].0, token, "the token itself is never stored");
    assert_eq!(db.read(|c| resolve(c, "nope")).unwrap(), None);
}

#[test]
fn a_coordinator_mints_workers_only_inside_its_own_scope() {
    let db = Db::open_in_memory().unwrap();
    let (a, b) = tasks(&db);
    let (coord, _) = db
        .write_txn("test", move |c, m| {
            mint(c, m, Minter::Operator, Role::Coordinator, "coord", Some(a))
        })
        .unwrap();
    let c1 = coord.clone();
    let (rev, _) = db
        .write_txn("test", move |c, m| {
            mint(c, m, Minter::Grant(&c1), Role::Implementer, "rev", Some(a))
        })
        .unwrap();
    assert_eq!(rev.parent, Some(coord.id));
    for (role, scope, why) in [
        (Role::Implementer, Some(b), "that same task"),
        (Role::Implementer, None, "that same task"),
        (Role::Coordinator, Some(a), "may not grant coordinator"),
        (Role::Reviewer, Some(a), "may not grant reviewer"),
        (
            Role::SystemicReviewer,
            Some(a),
            "may not grant systemic_reviewer",
        ),
        (Role::Operator, Some(a), "may not grant operator"),
    ] {
        let c2 = coord.clone();
        let e = db
            .write_txn("test", move |c, m| {
                mint(c, m, Minter::Grant(&c2), role, "x", scope)
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains(why), "{role:?}/{scope:?}: {e}");
    }
    let e = db
        .write_txn("test", move |c, m| {
            mint(c, m, Minter::Grant(&rev), Role::Implementer, "x", Some(a))
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("may not grant"), "a worker mints nothing: {e}");
}

#[test]
fn revoking_a_grant_revokes_everything_it_minted() {
    let db = Db::open_in_memory().unwrap();
    let (a, _) = tasks(&db);
    let (coord, ct, wt) = db
        .write_txn("test", move |c, m| {
            let (coord, ct) = mint(c, m, Minter::Operator, Role::Coordinator, "c", Some(a))?;
            let (_, wt) = mint(c, m, Minter::Grant(&coord), Role::Implementer, "w", Some(a))?;
            Ok((coord, ct, wt))
        })
        .unwrap();
    let before = db.read(generation).unwrap();
    let n = db
        .write_txn("test", move |c, m| revoke(c, m, coord.id))
        .unwrap();
    assert_eq!(n, 2);
    assert_ne!(
        db.read(generation).unwrap(),
        before,
        "a cache sees the change"
    );
    assert_eq!(db.read(move |c| resolve(c, &ct)).unwrap(), None);
    assert_eq!(db.read(move |c| resolve(c, &wt)).unwrap(), None);
    assert_eq!(db.read(|c| list(c, None, false)).unwrap(), vec![]);
    assert_eq!(db.read(|c| list(c, None, true)).unwrap().len(), 2);
}

#[test]
fn rotating_the_container_credential_retires_the_old_one_and_its_workers() {
    let db = Db::open_in_memory().unwrap();
    let (a, _) = tasks(&db);
    let (first, t1) = db.write_txn("test", rotate_container).unwrap();
    assert_eq!(first.role, Role::Coordinator);
    assert_eq!(first.agent, CONTAINER_AGENT);
    assert_eq!(first.scope, None);
    let (_, worker) = db
        .write_txn("test", move |c, m| {
            mint(c, m, Minter::Grant(&first), Role::Implementer, "r", Some(a))
        })
        .unwrap();
    let (_, t2) = db.write_txn("test", rotate_container).unwrap();
    assert_eq!(db.read(move |c| resolve(c, &t1)).unwrap(), None);
    assert_eq!(db.read(move |c| resolve(c, &worker)).unwrap(), None);
    assert!(db.read(move |c| resolve(c, &t2)).unwrap().is_some());
}

#[test]
fn the_agent_type_map_is_explicit_and_unmapped_types_hold_nothing() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("test", |c, m| {
        map_agent_type(c, m, "reviewer", Some(Role::Reviewer))?;
        map_agent_type(c, m, "implementer", Some(Role::Implementer))?;
        map_agent_type(c, m, "implementer", Some(Role::Designer))?;
        map_agent_type(c, m, "reviewer", None)
    })
    .unwrap();
    assert_eq!(
        db.read(|c| role_for_agent_type(c, "reviewer")).unwrap(),
        None
    );
    assert_eq!(
        db.read(|c| role_for_agent_type(c, "implementer")).unwrap(),
        Some(Role::Designer),
        "remapping replaces"
    );
    assert_eq!(
        db.read(|c| role_for_agent_type(c, "general-purpose"))
            .unwrap(),
        None
    );
    assert_eq!(db.read(agent_type_map).unwrap().len(), 1);
}

#[test]
fn an_attested_agent_binds_to_its_first_task_and_cannot_hop() {
    let db = Db::open_in_memory().unwrap();
    let (a, b) = tasks(&db);
    let first = db
        .write_txn("test", move |c, m| bind_agent(c, m, "s1", "ag1", a))
        .unwrap();
    assert_eq!(first, Bound::Now(a));
    let again = db
        .write_txn("test", move |c, m| bind_agent(c, m, "s1", "ag1", a))
        .unwrap();
    assert_eq!(again, Bound::To(a));
    let hop = db
        .write_txn("test", move |c, m| bind_agent(c, m, "s1", "ag1", b))
        .unwrap();
    assert_eq!(hop, Bound::Elsewhere(a));
    let other_session = db
        .write_txn("test", move |c, m| bind_agent(c, m, "s2", "ag1", b))
        .unwrap();
    assert_eq!(other_session, Bound::Now(b), "the binding is per session");
    assert_eq!(db.read(|c| agent_binding(c, "s1", "ag1")).unwrap(), Some(a));
}

#[test]
fn scope_reaches_subtasks_and_the_task_s_own_findings_and_nothing_else() {
    let db = Db::open_in_memory().unwrap();
    let (a, b) = tasks(&db);
    let (sub, finding, stray) = db
        .write_txn("test", move |c, m| {
            let sub = create(c, m, &NewTask::new("task:a1", "A1"))?;
            crate::task::add_subtask(c, m, a, sub)?;
            let mut f = NewTask::new("task:f", "a finding");
            f.home = "reviews/a-1/must-fix".into();
            let finding = create(c, m, &f)?;
            let mut s = NewTask::new("task:s", "someone else's finding");
            s.home = "reviews/a-10/must-fix".into();
            let stray = create(c, m, &s)?;
            crate::reviews::record(
                c,
                m,
                a,
                "reviews/a-1",
                "abc",
                "t",
                crate::reviews::RoundSource::AnyNamespace,
            )?;
            Ok((sub, finding, stray))
        })
        .unwrap();
    let reach = |t| db.read(move |c| in_scope(c, a, t)).unwrap();
    assert!(reach(a));
    assert!(reach(sub), "a subtask");
    assert!(reach(finding), "a finding in the task's own round");
    assert!(!reach(stray), "reviews/a-10 is not under reviews/a-1");
    assert!(!reach(b), "another task");
}

#[test]
fn a_grant_descends_from_whatever_minted_it() {
    let db = Db::open_in_memory().unwrap();
    let (a, _) = tasks(&db);
    let (root, child, other) = db
        .write_txn("test", move |c, m| {
            let (root, _) = mint(c, m, Minter::Operator, Role::Coordinator, "c", Some(a))?;
            let (child, _) = mint(c, m, Minter::Grant(&root), Role::Implementer, "r", Some(a))?;
            let (other, _) = mint(c, m, Minter::Operator, Role::Coordinator, "d", Some(a))?;
            Ok((root.id, child.id, other.id))
        })
        .unwrap();
    assert!(db.read(move |c| descends_from(c, child, root)).unwrap());
    assert!(db.read(move |c| descends_from(c, root, root)).unwrap());
    assert!(!db.read(move |c| descends_from(c, root, child)).unwrap());
    assert!(!db.read(move |c| descends_from(c, child, other)).unwrap());
}

#[test]
fn a_grant_revoked_after_it_was_read_mints_nothing() {
    let db = Db::open_in_memory().unwrap();
    let (a, _) = tasks(&db);
    let (coord, _) = db
        .write_txn("test", |c, m| {
            mint(c, m, Minter::Operator, Role::Coordinator, "c", None)
        })
        .unwrap();
    // `coord` is the row as the request was admitted with; the revoke commits after.
    let id = coord.id;
    db.write_txn("test", move |c, m| revoke(c, m, id)).unwrap();
    let stale = coord.clone();
    let e = db
        .write_txn("test", move |c, m| {
            mint(c, m, Minter::Grant(&stale), Role::Implementer, "r", Some(a))
        })
        .unwrap_err();
    assert!(e.to_string().contains("revoked"), "{e}");
    assert_eq!(db.read(|c| list(c, None, false)).unwrap(), vec![]);
}

#[test]
fn only_rotation_makes_the_container_credential_whatever_a_grant_is_labelled() {
    let db = Db::open_in_memory().unwrap();
    let (a, _) = tasks(&db);
    let (labelled, _) = db
        .write_txn("test", move |c, m| {
            mint(
                c,
                m,
                Minter::Operator,
                Role::Implementer,
                CONTAINER_AGENT,
                Some(a),
            )
        })
        .unwrap();
    assert!(!labelled.container, "the label decides nothing");
    let (cred, _) = db.write_txn("test", rotate_container).unwrap();
    assert!(cred.container);
    db.write_txn("test", rotate_container).unwrap();
    let live = db.read(|c| list(c, None, false)).unwrap();
    assert!(
        live.iter().any(|g| g.id == labelled.id),
        "rotation retires only container credentials, not a grant that shares the label"
    );
    assert!(!live.iter().any(|g| g.id == cred.id));
}

#[test]
fn deleting_a_task_revokes_its_grants_and_keeps_them_listed() {
    let db = Db::open_in_memory().unwrap();
    let (a, _) = tasks(&db);
    let (g, token) = db
        .write_txn("test", move |c, m| {
            mint(c, m, Minter::Operator, Role::Implementer, "i", Some(a))
        })
        .unwrap();
    db.write_txn("test", move |c, m| crate::item::remove(c, m, a, true))
        .unwrap();
    assert_eq!(db.read(move |c| resolve(c, &token)).unwrap(), None);
    let all = db.read(|c| list(c, None, true)).unwrap();
    let row = all
        .iter()
        .find(|r| r.id == g.id)
        .expect("kept, not deleted");
    assert!(row.revoked_at.is_some());
}

/// A grant minted before the table was tightened — a coordinator's reviewer, from before review
/// round 4 — no longer resolves, in a daemon's cache or out of it; one the table still allows does.
#[test]
fn a_grant_its_minter_may_no_longer_grant_does_not_resolve() {
    let db = Db::open_in_memory().unwrap();
    let (a, _) = tasks(&db);
    let (coord, _) = db
        .write_txn("test", move |c, m| {
            mint(c, m, Minter::Operator, Role::Coordinator, "coord", None)
        })
        .unwrap();
    let parent = coord.id;
    let ((stale, stale_token), (fine, fine_token)) = db
        .write_txn("test", move |c, m| {
            // As the mint of an older build wrote it, past today's check.
            let stale = super::insert(c, m, Role::Reviewer, "rev", Some(a), Some(parent), false)?;
            let fine = mint(
                c,
                m,
                Minter::Grant(&coord),
                Role::Implementer,
                "impl",
                Some(a),
            )?;
            Ok((stale, fine))
        })
        .unwrap();
    let (t1, t2) = (stale_token.clone(), fine_token.clone());
    assert_eq!(db.read(move |c| resolve(c, &t1)).unwrap(), None);
    assert_eq!(
        db.read(move |c| resolve(c, &t2)).unwrap(),
        Some(fine.clone())
    );
    let cached = db.read(live_by_hash).unwrap();
    assert!(!cached.iter().any(|(_, g)| g.id == stale.id), "{cached:?}");
    assert!(cached.iter().any(|(_, g)| g.id == fine.id), "{cached:?}");
    // Nor is it listed as live, where the operator would see a grant that cannot work.
    let listed = db.read(|c| list(c, None, false)).unwrap();
    assert!(!listed.iter().any(|g| g.id == stale.id), "{listed:?}");
    assert!(listed.iter().any(|g| g.id == fine.id), "{listed:?}");
}
