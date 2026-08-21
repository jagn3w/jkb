//! The sync machine's proof — and the generalization test.
//!
//! The audit here is over a **modelled** observation space, not a raw cross-product of the
//! facts. That is not a convenience: most fact combinations are impossible (a file that is not
//! on disk cannot have changed on disk), and a dead end reported for an impossible observation
//! is noise that trains you to ignore the check. So the generator walks the dimensions a sync
//! pass really varies — does the file exist, did it parse, which side changed, what does the
//! mount permit, what is the policy, is the store empty of what the file declares — and derives
//! the dependent facts from those.
//!
//! Hand-restricting the space is also where a real defect could hide, which is why the
//! restriction is written as *derivation from independent dimensions* rather than as a list of
//! cases: a combination the author did not think of is still generated, as long as its
//! dimensions are.

use jkb_fsm::{Event, Fact, Reconciliation, State};

use super::{machine, observe, FileEffect, FileEvent, FileFacts, FileState, Policy};

/// Every observation a sync pass can really make, at every state.
fn every_observation() -> Vec<FileFacts> {
    let mut out = Vec::new();
    for &state in <FileState as State>::ALL {
        // With no journal row there is no base, so the change facts cannot be established at
        // all — which is the honest reading, and the reason `Untracked` accepts so few events.
        let tracked = state != FileState::Untracked;
        for &on_disk in &[Fact::Yes, Fact::No] {
            for &parses in &[Fact::Yes, Fact::No] {
                for (disk_changed, kb_changed) in change_pairs(tracked, on_disk) {
                    for &disjoint in &[Fact::Yes, Fact::No, Fact::Unknown] {
                        for (imports, exports) in [
                            (Fact::Yes, Fact::No),
                            (Fact::No, Fact::Yes),
                            (Fact::Yes, Fact::Yes),
                        ] {
                            for policy in [Policy::Manual, Policy::DiskWins, Policy::KbWins] {
                                for (store_empty_of_declared, items_still_bound) in [
                                    (Fact::No, Fact::Yes),
                                    (Fact::No, Fact::No),
                                    (Fact::Yes, Fact::Yes),
                                    (Fact::Yes, Fact::No),
                                ] {
                                    for &would_drop_items in &[Fact::Yes, Fact::No] {
                                        out.push(FileFacts {
                                            state,
                                            on_disk,
                                            // A file that is not there did not parse and did
                                            // not fail to: there was nothing to read.
                                            parses: if on_disk.is_yes() {
                                                parses
                                            } else {
                                                Fact::Unknown
                                            },
                                            disk_changed,
                                            kb_changed,
                                            disjoint,
                                            store_empty_of_declared,
                                            items_still_bound,
                                            would_drop_items,
                                            imports,
                                            exports,
                                            policy,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// The `(disk_changed, kb_changed)` pairs that can be established, given whether there is a base
/// to compare against and whether the file is there at all.
fn change_pairs(tracked: bool, on_disk: Fact) -> Vec<(Fact, Fact)> {
    if !tracked {
        return vec![(Fact::Unknown, Fact::Unknown)];
    }
    if on_disk.is_no() {
        // No disk side to have changed; the store side is still knowable.
        return vec![(Fact::Unknown, Fact::Yes), (Fact::Unknown, Fact::No)];
    }
    vec![
        (Fact::Yes, Fact::Yes),
        (Fact::Yes, Fact::No),
        (Fact::No, Fact::Yes),
        (Fact::No, Fact::No),
    ]
}

// ---------------------------------------------------------------------------------------------
// Well-formedness
// ---------------------------------------------------------------------------------------------

#[test]
fn the_machine_is_well_formed() {
    let defects = machine().check();
    let rendered: Vec<String> = defects.iter().map(ToString::to_string).collect();
    assert!(defects.is_empty(), "{rendered:#?}");
}

/// The property the whole area exists for: **a file can always get back to settled**.
///
/// The sync engine's incident history is the opposite of this — `Refused` on every pass for
/// ever, a quarantine that could not recover, a guard satisfied only by deleting the file it
/// was protecting. Static liveness (`check`) says the *edges* exist; this says that under every
/// observation a pass can really make, some edge is actually available.
#[test]
fn no_observation_leaves_a_file_with_no_way_back_to_settled() {
    let space = every_observation();
    let defects = machine().audit(&space);
    let rendered: Vec<String> = defects.iter().map(ToString::to_string).collect();
    assert!(
        defects.is_empty(),
        "audited {} observations:\n{rendered:#?}",
        space.len()
    );
}

/// The guards **partition** the observation space: at most one applies to any one observation.
///
/// This is D45.5's conclusion — *a route is not a cause; the condition must dominate every arm*
/// — made checkable. In the engine the arms are ordered, so an overlap is silently resolved by
/// whichever is reached first; here it is [`Reconciliation::Ambiguous`], and the audit above
/// reports it as a defect.
#[test]
fn at_most_one_condition_applies_to_any_observation() {
    for facts in every_observation() {
        assert!(
            !matches!(observe(&facts), Reconciliation::Ambiguous(_)),
            "two conditions apply at once: {facts:?}"
        );
    }
}

#[test]
fn the_journal_renders_as_a_diagram() {
    let dot = machine().dot("sync");
    assert!(dot.contains("\"untracked\" -> \"settled\" [label=\"adopted\", style=dashed]"));
    // Every edge is dashed: a synced file has no applied events at all, which is the shape that
    // distinguishes a reconciler from a lifecycle.
    assert!(!dot.contains("style=solid"));
    assert!(dot.contains("\"settled\" [shape=doublecircle"));
}

// ---------------------------------------------------------------------------------------------
// The behaviours, each one a real incident from the sync corpus
// ---------------------------------------------------------------------------------------------

/// A settled file with an ordinary one-sided edit imports.
fn settled() -> FileFacts {
    FileFacts {
        state: FileState::Settled,
        on_disk: Fact::Yes,
        parses: Fact::Yes,
        disk_changed: Fact::Yes,
        kb_changed: Fact::No,
        disjoint: Fact::Unknown,
        store_empty_of_declared: Fact::No,
        items_still_bound: Fact::Yes,
        would_drop_items: Fact::No,
        imports: Fact::Yes,
        exports: Fact::Yes,
        policy: Policy::Manual,
    }
}

fn fired(facts: &FileFacts) -> Option<(FileEvent, FileState)> {
    match observe(facts) {
        Reconciliation::Fired(jkb_fsm::Outcome::Moved { event, to, .. }) => Some((event, to)),
        _ => None,
    }
}

#[test]
fn a_one_sided_edit_moves_the_file_the_only_way_it_can() {
    assert_eq!(
        fired(&settled()),
        Some((FileEvent::Imported, FileState::Settled))
    );
    let kb_only = FileFacts {
        disk_changed: Fact::No,
        kb_changed: Fact::Yes,
        ..settled()
    };
    assert_eq!(
        fired(&kb_only),
        Some((FileEvent::Exported, FileState::Settled))
    );
}

/// D45.5 — the store contributing nothing to a file that declares items is the condition that
/// **dominates every arm**, and it must win over whatever the hashes say.
///
/// Every direction arm got this wrong in its own way, and each fix was correct and was not the
/// last, because a route is not a cause. As a transition it is one row against the same
/// observation as every other.
#[test]
fn an_emptied_store_is_recovered_whatever_the_hashes_say() {
    for (disk_changed, kb_changed) in [
        (Fact::Yes, Fact::No),
        (Fact::No, Fact::Yes),
        (Fact::Yes, Fact::Yes),
        (Fact::No, Fact::No),
    ] {
        let facts = FileFacts {
            store_empty_of_declared: Fact::Yes,
            items_still_bound: Fact::No,
            disk_changed,
            kb_changed,
            ..settled()
        };
        assert_eq!(
            fired(&facts),
            Some((FileEvent::Recovered, FileState::Settled)),
            "the direction arms took an observation the recovery owns \
             ({disk_changed:?}, {kb_changed:?})"
        );
    }
}

/// ...but an empty render is **not** proof of an empty store. A bound item that merely lost its
/// primary placement renders as nothing too, and importing over it destroys un-exported work.
/// The two are told apart by asking the store, and they end in different states.
#[test]
fn an_empty_render_over_live_items_is_blocked_not_recovered() {
    let facts = FileFacts {
        store_empty_of_declared: Fact::Yes,
        items_still_bound: Fact::Yes,
        ..settled()
    };
    assert_eq!(
        fired(&facts),
        Some((FileEvent::WriteBlocked, FileState::Blocked))
    );
}

/// An export-only mount cannot read the file back, so the same condition ends in `Blocked`
/// rather than `Recovered` — and the state set says which, where the journal spelled both
/// `needs_attention`.
#[test]
fn a_mount_that_cannot_import_is_blocked_rather_than_recovered() {
    let facts = FileFacts {
        store_empty_of_declared: Fact::Yes,
        items_still_bound: Fact::No,
        imports: Fact::No,
        ..settled()
    };
    assert_eq!(
        fired(&facts),
        Some((FileEvent::WriteBlocked, FileState::Blocked))
    );
}

/// Every flagged state gets back to settled on the next good pass. This is the sync engine's
/// most repeated defect stated as a property, and `audit` checks it over the whole space; this
/// walks the three flagged states explicitly so a reader can see it.
#[test]
fn every_flagged_state_recovers_on_the_next_good_edit() {
    for state in [
        FileState::Conflicted,
        FileState::Quarantined,
        FileState::Blocked,
    ] {
        let facts = FileFacts { state, ..settled() };
        assert_eq!(
            fired(&facts),
            Some((FileEvent::Imported, FileState::Settled)),
            "{state:?} could not be cleared by an ordinary disk edit"
        );
    }
}

/// A parse failure needs nothing else established — the bytes are there and they are not a
/// document, whatever the mount mode or the hashes say. It keeps the last-good items (D25).
#[test]
fn unparseable_bytes_quarantine_from_anywhere() {
    for state in <FileState as State>::ALL {
        let facts = FileFacts {
            state: *state,
            parses: Fact::No,
            ..settled()
        };
        // Including from `Quarantined` itself: a second failing pass has different bytes to
        // stash, so it concludes the same thing and writes it again. That row is declared
        // rather than absorbed, because absorbing it would throw the stash away.
        assert_eq!(
            fired(&facts),
            Some((FileEvent::ParseFailed, FileState::Quarantined)),
            "{state:?}"
        );
    }
}

/// The three both-changed outcomes are told apart by **facts**, not by the order of three arms
/// inside one.
#[test]
fn both_changed_splits_three_ways_on_the_facts() {
    let both = FileFacts {
        disk_changed: Fact::Yes,
        kb_changed: Fact::Yes,
        ..settled()
    };
    assert_eq!(
        fired(&FileFacts {
            disjoint: Fact::Yes,
            ..both
        }),
        Some((FileEvent::Merged, FileState::Settled))
    );
    assert_eq!(
        fired(&FileFacts {
            disjoint: Fact::No,
            policy: Policy::Manual,
            ..both
        }),
        Some((FileEvent::Conflicted, FileState::Conflicted))
    );
    assert_eq!(
        fired(&FileFacts {
            disjoint: Fact::No,
            policy: Policy::DiskWins,
            ..both
        }),
        Some((FileEvent::ResolvedByPolicy, FileState::Settled))
    );
}

/// A pass that could not establish what it saw does **nothing**. `Unknown` licenses no write,
/// which for a reconciler is the whole safety property: the alternative is overwriting a file
/// on the strength of a stat that failed.
#[test]
fn an_observation_that_could_not_be_completed_does_nothing() {
    let hazy = FileFacts {
        state: FileState::Settled,
        ..FileFacts::default()
    };
    assert!(matches!(observe(&hazy), Reconciliation::Settled));

    // ...including the one fact that decides between recovering and blocking.
    let half_seen = FileFacts {
        store_empty_of_declared: Fact::Yes,
        items_still_bound: Fact::Unknown,
        ..settled()
    };
    assert_eq!(fired(&half_seen), None, "acted on an unread store");
}

/// The plan a settling pass yields is the one the journal writer performs, so a status change
/// and the base it settles to cannot come apart.
#[test]
fn settling_yields_the_journal_write_with_the_move() {
    let Reconciliation::Fired(out) = observe(&settled()) else {
        panic!("an ordinary import should fire");
    };
    assert_eq!(out.effects(), [FileEffect::Settle]);

    let quarantining = FileFacts {
        parses: Fact::No,
        ..settled()
    };
    let Reconciliation::Fired(out) = observe(&quarantining) else {
        panic!("a parse failure should fire");
    };
    assert_eq!(out.effects(), [FileEffect::Stash]);
}

/// Every event is declared somewhere, and every state is reachable — both are `check()`'s job,
/// asserted here as the shape a reader should expect rather than trusted to the sweep.
#[test]
fn every_event_is_wired_and_every_state_reachable() {
    let m = machine();
    for &event in <FileEvent as Event>::ALL {
        assert!(
            m.transitions.iter().any(|t| t.event == event),
            "`{}` is declared nowhere",
            event.name()
        );
    }
    for &state in <FileState as State>::ALL {
        assert!(
            !m.accepted_from(state).is_empty(),
            "`{}` accepts nothing at all",
            state.name()
        );
    }
}
