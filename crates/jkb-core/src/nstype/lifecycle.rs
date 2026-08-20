//! An investigation unit's resolution, as a machine — the third one on [`jkb_fsm`], and the one
//! whose rules are **strategy-supplied**.
//!
//! The first two machines each had one table. This has two, over one state set, because two
//! strategies genuinely disagree about what an observation means:
//!
//! | | base (`conjecture-attack`, contracts) | `debugging` |
//! | --- | --- | --- |
//! | a settled unit can go back to `unresolved` | no | **yes** — an observation about a moving system goes stale |
//! | a tombstone can be revived by fresh evidence | **yes** | no — "deaths and supersessions stand as-is" |
//!
//! Both differences already existed; neither was written down anywhere a reader could see it.
//! They lived in the shape of two functions — `default_rollup`'s fall-through and
//! `debugging::resolution_rollup`'s early return — and the second one is a *four-line if-chain
//! whose behaviour is its ordering*.
//!
//! That ordering is the other thing this machine changes. `default_rollup` asks *refuted?* then
//! *superseded?* then *confirmed?* and returns at the first hit, so a unit carrying contradictory
//! evidence is resolved by which question is asked first. Here the priority is a **guard clause**
//! — `Confirmed` requires `!refuted` — so it is visible, and [`jkb_fsm::Machine::audit`] proves
//! the conditions partition rather than overlap.
//!
//! The division of labour is the point: **the strategy supplies the facts, the machine supplies
//! the rules.** A strategy that merely observes differently (a `debugging` symptom is confirmed
//! by a *verified fix*, not by a `confirms` edge) needs no table of its own; only a strategy that
//! draws a different conclusion from the same facts gets one.

use jkb_fsm::{
    all_of, require_no, require_yes, Denial, Dest, Event, EventKind, Fact, Machine, Stateful,
    Transition, Verdict,
};
use jkb_types::Resolution;
use rusqlite::OptionalExtension as _;

/// What happened to a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitEvent {
    /// Something refutes it, or rules out the region it lives in.
    Refuted,
    /// Something replaces it.
    Superseded,
    /// Something confirms or verifies it.
    Confirmed,
    /// The evidence that settled it no longer describes today's system (design Dmem: an
    /// observation carries a `commit-range=`, and a mutable system moves out from under it).
    WentStale,
    /// An operator states the resolution directly (`jkb inv resolve`).
    ///
    /// [`Dest::Stated`], so it is excluded from the liveness walk exactly as the task machine's
    /// `override` is: a person naming an outcome is not the investigation reaching one.
    Stated,
}

impl Event for UnitEvent {
    const ALL: &'static [Self] = &[
        Self::Refuted,
        Self::Superseded,
        Self::Confirmed,
        Self::WentStale,
        Self::Stated,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Refuted => "refuted",
            Self::Superseded => "superseded",
            Self::Confirmed => "confirmed",
            Self::WentStale => "went_stale",
            Self::Stated => "stated",
        }
    }

    fn kind(self) -> EventKind {
        match self {
            Self::Stated => EventKind::Applied,
            _ => EventKind::Reconciled,
        }
    }
}

/// What a unit's machine asks the caller to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitEffect {
    /// Write `items.resolution`.
    SetResolution(Resolution),
}

/// What a strategy observed about one unit.
///
/// **The strategy fills this in**; the machine reads it. That split is what lets `debugging`
/// answer `confirmed` differently for a symptom — a verified fix rather than a `confirms` edge —
/// without needing a table of its own. A table is only for a strategy that draws a different
/// *conclusion*, which is a much rarer thing and worth being able to see.
///
/// Deliberately **not** `Copy`. A guard takes `&C` — that is [`jkb_fsm::Transition`]'s signature
/// — and a context small enough to copy trips `clippy::trivially_copy_pass_by_ref` on every
/// guard in the table. The other two machines' contexts are large enough not to notice; this one
/// is six bytes.
#[derive(Debug, Clone)]
pub struct UnitFacts {
    /// Where the unit stands now.
    pub resolution: Resolution,
    /// Something refutes it or rules out its region.
    pub refuted: Fact,
    /// Something replaces it.
    pub superseded: Fact,
    /// Something confirms or verifies it.
    pub confirmed: Fact,
    /// The evidence that settled it no longer applies.
    pub stale: Fact,
    /// The resolution an operator named, for [`UnitEvent::Stated`].
    pub stated: Option<Resolution>,
}

impl Default for UnitFacts {
    fn default() -> Self {
        Self {
            resolution: Resolution::Unresolved,
            refuted: Fact::Unknown,
            superseded: Fact::Unknown,
            confirmed: Fact::Unknown,
            stale: Fact::Unknown,
            stated: None,
        }
    }
}

impl Stateful<Resolution> for UnitFacts {
    fn state(&self) -> Resolution {
        self.resolution
    }
}

/// A unit's machine.
pub type UnitMachine = Machine<Resolution, UnitEvent, UnitFacts, UnitEffect>;

// ---------------------------------------------------------------------------------------------
// Guards. The priority is written as **clauses**, not as arm order.
//
// `default_rollup` asks *refuted? superseded? confirmed?* and returns at the first hit, so a
// unit carrying contradictory evidence is resolved by which question the code asks first. The
// answer is the same here — a refutation outranks a confirmation, because a graveyard that
// forgets is not a graveyard — but it is stated where a reader can disagree with it, and
// `audit` proves the four conditions do not overlap.
// ---------------------------------------------------------------------------------------------

fn refuted(f: &UnitFacts) -> Verdict<UnitEvent> {
    require_yes(f.refuted, || Denial::new("nothing refutes it."))
}

fn superseded(f: &UnitFacts) -> Verdict<UnitEvent> {
    all_of([
        require_yes(f.superseded, || Denial::new("nothing replaces it.")),
        require_no(f.refuted, || {
            Denial::with_remedy(
                "it is refuted, and a refutation outranks a replacement.",
                UnitEvent::Refuted,
                "Unlink the refuting edge if the refutation no longer stands.",
            )
        }),
    ])
}

fn confirmed(f: &UnitFacts) -> Verdict<UnitEvent> {
    all_of([
        require_yes(f.confirmed, || Denial::new("nothing confirms it.")),
        require_no(f.refuted, || {
            Denial::with_remedy(
                "it is refuted, and a refutation outranks a confirmation.",
                UnitEvent::Refuted,
                "Unlink the refuting edge if the refutation no longer stands.",
            )
        }),
        require_no(f.superseded, || {
            Denial::with_remedy(
                "it has been replaced, and the replacement is what to confirm.",
                UnitEvent::Superseded,
                "Confirm the unit that superseded it instead.",
            )
        }),
        // **Staleness outranks confirmation**, which is the priority `debugging`'s rollup had
        // and the one place the ordering is not obvious: the confirming evidence is *what went
        // stale*, so a confirmation that no longer describes today's system is not a result. A
        // strategy with no staleness notion answers `stale: No`, so this clause costs it nothing.
        // No remedy named. `went_stale` is the obvious candidate and is exactly wrong: it is
        // not a transition every machine here has — the base table has no staleness notion at
        // all — and `audit` caught that. What to do is re-run the observation, which reaches
        // this machine as a fresh `confirmed` on the next pass rather than as an event anyone
        // applies.
        require_no(f.stale, || {
            Denial::new("the evidence confirming it no longer describes today's system.")
        }),
    ])
}

/// The evidence that settled a unit no longer applies, so it goes back on the frontier.
///
/// A refutation or a replacement still outranks it — those do not go stale, and a unit killed by
/// one stays killed.
fn went_stale(f: &UnitFacts) -> Verdict<UnitEvent> {
    all_of([
        require_yes(f.stale, || {
            Denial::new("the evidence that settled it still applies.")
        }),
        require_no(f.refuted, || Denial::new("it is refuted.")),
        require_no(f.superseded, || Denial::new("it has been replaced.")),
    ])
}

/// The resolution an operator named. `None` refuses rather than silently staying put.
fn stated(f: &UnitFacts) -> Option<Resolution> {
    f.stated
}

fn set_derived(to: Resolution) -> Vec<UnitEffect> {
    vec![UnitEffect::SetResolution(to)]
}

fn plan_refuted(_: &UnitFacts) -> Vec<UnitEffect> {
    set_derived(Resolution::DeadEnd)
}
fn plan_superseded(_: &UnitFacts) -> Vec<UnitEffect> {
    set_derived(Resolution::Superseded)
}
fn plan_confirmed(_: &UnitFacts) -> Vec<UnitEffect> {
    set_derived(Resolution::Success)
}
fn plan_stale(_: &UnitFacts) -> Vec<UnitEffect> {
    set_derived(Resolution::Unresolved)
}
fn plan_stated(f: &UnitFacts) -> Vec<UnitEffect> {
    f.stated.map(set_derived).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// The two tables
// ---------------------------------------------------------------------------------------------

macro_rules! rows {
    ($( [$($from:ident),+] -$event:ident-> $to:expr, $guard:expr, $plan:expr; )+) => {
        &[$($(Transition {
            from: Resolution::$from,
            event: UnitEvent::$event,
            to: $to,
            guard: Some($guard),
            plan: Some($plan),
        },)+)+]
    };
}

/// The base table: what every strategy agrees an observation means.
///
/// A tombstone here is **not** absorbing — withdraw the refutation and confirm it, and it
/// revives. That is `default_rollup`'s behaviour today (its fall-through has no early return for
/// a settled unit), and it is deliberate as far as this table is concerned: the refuting *edge*
/// is what makes it a dead end, so removing that edge is a considered act, not an accident.
const BASE_ROWS: &[Transition<Resolution, UnitEvent, UnitFacts, UnitEffect>] = rows! {
    [Unresolved, Success, Superseded, Abandoned] -Refuted->    Dest::To(Resolution::DeadEnd), refuted, plan_refuted;
    [Unresolved, Success, DeadEnd, Abandoned]    -Superseded-> Dest::To(Resolution::Superseded), superseded, plan_superseded;
    [Unresolved, DeadEnd, Superseded, Abandoned] -Confirmed->  Dest::To(Resolution::Success), confirmed, plan_confirmed;
    // An operator may name any resolution, from any state. Excluded from the liveness walk —
    // an escape hatch is not an exit — and counted by the reachability walk, which is what makes
    // `abandoned` a state the object can really be in rather than dead code.
    [Unresolved, Success, DeadEnd, Superseded, Abandoned]
                                                 -Stated->     Dest::Stated(stated), |_: &UnitFacts| Verdict::Allow, plan_stated;
};

/// `debugging`'s table: the base, **minus** tombstone revival, **plus** staleness.
///
/// Both differences are real and both were already in the code, in the shape of
/// `debugging::resolution_rollup`'s early return and its `is_stale` check. Neither was
/// discoverable from anywhere but that function.
///
/// - *Staleness*: an observation about a mutable system carries a `commit-range=`, and the code
///   moves out from under it. A `success` that rests on stale evidence is not a result any more.
/// - *No revival*: "deaths and supersessions stand as-is" — an obstruction does not go stale, and
///   a symptom refuted as "not actually a bug" stays refuted.
const DEBUGGING_ROWS: &[Transition<Resolution, UnitEvent, UnitFacts, UnitEffect>] = rows! {
    [Unresolved, Success, Superseded, Abandoned] -Refuted->    Dest::To(Resolution::DeadEnd), refuted, plan_refuted;
    [Unresolved, Success, Abandoned]             -Superseded-> Dest::To(Resolution::Superseded), superseded, plan_superseded;
    [Unresolved, Abandoned]                      -Confirmed->  Dest::To(Resolution::Success), confirmed, plan_confirmed;
    [Success]                                    -WentStale->  Dest::To(Resolution::Unresolved), went_stale, plan_stale;
    [Unresolved, Success, DeadEnd, Superseded, Abandoned]
                                                 -Stated->     Dest::Stated(stated), |_: &UnitFacts| Verdict::Allow, plan_stated;
};

/// The machine every strategy uses unless it says otherwise.
pub const BASE: UnitMachine = Machine {
    transitions: BASE_ROWS,
    initial: Resolution::Unresolved,
};

/// `debugging`'s machine.
pub const DEBUGGING: UnitMachine = Machine {
    transitions: DEBUGGING_ROWS,
    initial: Resolution::Unresolved,
};

/// Read every fact the base machine needs about `node` from its edges.
///
/// The **default fact-gatherer**, and the base strategy's whole contribution: what an incoming
/// edge means is shared, so a strategy that merely observes differently overrides this rather
/// than getting a table of its own.
///
/// `stale` is `No`, not `Unknown`: a strategy with no staleness notion has *established* that
/// nothing here goes stale, and answering `Unknown` would make every confirmation refuse.
///
/// # Errors
/// Returns an error if a query fails.
pub fn base_facts(
    conn: &rusqlite::Connection,
    node: jkb_types::ItemId,
) -> crate::Result<UnitFacts> {
    use jkb_types::EdgeType;
    let incoming = |types: &[EdgeType]| -> crate::Result<Fact> {
        let list = types
            .iter()
            .map(|t| format!("'{}'", t.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        // The type strings come from the closed `EdgeType` enum, never from user input.
        let sql =
            format!("SELECT 1 FROM edges WHERE dst_item_id = ?1 AND type IN ({list}) LIMIT 1");
        Ok(Fact::from(
            conn.prepare_cached(&sql)?
                .query_row([node.get()], |_| Ok(true))
                .optional()?
                .unwrap_or(false),
        ))
    };
    Ok(UnitFacts {
        resolution: crate::item::get_resolution(conn, node)?.unwrap_or(Resolution::Unresolved),
        refuted: incoming(&[EdgeType::Refutes, EdgeType::RulesOut])?,
        superseded: incoming(&[EdgeType::Supersedes])?,
        confirmed: incoming(&[EdgeType::Confirms, EdgeType::Verifies])?,
        stale: Fact::No,
        stated: None,
    })
}

/// The event that would produce `to` from an observation.
///
/// Total over the resolutions a *reconciliation* can produce, which is what makes it usable as a
/// check: a caller that has computed a resolution some other way can ask whether the machine
/// declares that move, and get a named refusal if it does not.
#[must_use]
pub fn deriving(to: Resolution) -> UnitEvent {
    match to {
        Resolution::DeadEnd => UnitEvent::Refuted,
        Resolution::Superseded => UnitEvent::Superseded,
        Resolution::Success => UnitEvent::Confirmed,
        Resolution::Unresolved => UnitEvent::WentStale,
        // Nothing derives `abandoned` from evidence; only a person does.
        Resolution::Abandoned => UnitEvent::Stated,
    }
}

#[cfg(test)]
mod tests;
