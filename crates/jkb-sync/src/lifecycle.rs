//! The sync journal's state machine — the second machine on [`jkb_fsm`], and the one that
//! tests whether the library generalizes.
//!
//! It is deliberately unlike the task lifecycle. A task has a *lifecycle*: it starts, it is
//! worked, it ends, and most of what happens to it is something somebody asked for. A synced
//! file has none of that — it is a **reconciler**. Nothing "finishes"; every transition is an
//! observation; and the interesting question is never *what may I do next* but *which of these
//! conditions applies to what I just saw*.
//!
//! What the two share is the shape of their failures. The sync engine's incident history is
//! `Outcome::Refused` on every pass for ever, a quarantined file that cannot recover, a
//! `foreign_layout` guard that refused until the file was deleted — states with no way back to
//! settled, which is exactly [`jkb_fsm::Defect::Wedged`] and [`jkb_fsm::Defect::DeadEnd`]. And
//! D45.5's whole conclusion — *"a route is not a cause; the condition must dominate every
//! arm"* — is [`jkb_fsm::Machine::reconcile`] stated as a rule: every candidate's guard is
//! evaluated against the same observation, and two that both apply are **reported**, not
//! resolved by whichever arm the code reached first.
//!
//! **This module does not drive the reconcile.** `engine::reconcile` still decides direction and
//! does the work; what routes through here is the **journal status**, so that column has one
//! writer, every status change is a declared transition, and an engine change that produces an
//! unmodelled one fails loudly instead of writing a state nothing expects.

use jkb_fsm::{
    all_of, require_no, require_yes, Denial, Dest, Event, EventKind, Fact, Machine, State,
    Stateful, Transition, Verdict,
};

/// Where one synced file stands.
///
/// The first thing modelling this surfaced: **`needs_attention` is two states**. It is written
/// both when the file did not parse (its bytes are stashed, the KB keeps its last-good items)
/// and when a write was refused because it would have deleted item lines. Those want opposite
/// remedies — the first wants the *file* fixed, the second wants the *store* fixed or the file
/// re-read — and `Outcome::Refused`'s own doc already says so, in prose, with the warning that
/// the reason "must be read rather than assumed". A state set says it instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileState {
    /// No journal row: this pass is the first time jkb has looked at this file.
    Untracked,
    /// Settled. Disk and store agree, and nothing is owed.
    Settled,
    /// Both sides changed the same unit and the policy is `manual`, so neither was overwritten.
    Conflicted,
    /// The file did not parse. Its bytes are stashed in a blob and the store keeps its last-good
    /// items (design D25: quarantine, do not destroy).
    Quarantined,
    /// A write was refused because it would have removed item lines the file still declares
    /// (design D45.5). Nothing was written.
    Blocked,
}

impl State for FileState {
    const ALL: &'static [Self] = &[
        Self::Untracked,
        Self::Settled,
        Self::Conflicted,
        Self::Quarantined,
        Self::Blocked,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Untracked => "untracked",
            Self::Settled => "settled",
            Self::Conflicted => "conflicted",
            Self::Quarantined => "quarantined",
            Self::Blocked => "blocked",
        }
    }

    /// Settled — and **untracked**, which is the part worth arguing.
    ///
    /// This is where the library had to change. `State::is_settled` was `is_terminal`, meaning
    /// *the lifecycle is finished here* — a task is `done` and nothing more will happen to it.
    /// A synced file is never finished; it settles and is then edited again. What the checks
    /// actually want is **rest**: a state in which the object owes the system nothing.
    ///
    /// Taken seriously, that makes the *initial* state a resting one too. A file an export-only
    /// mount has never imported and holds no items for is nobody's business — the pass looks,
    /// has nothing to do, and writes no journal row. Calling that stuck would report a defect
    /// for the ordinary case of a file jkb does not manage.
    fn is_settled(self) -> bool {
        matches!(self, Self::Settled | Self::Untracked)
    }

    /// The three flagged states are waiting on a person.
    ///
    /// A conflicted file moves when you edit it; until then no observation moves it, and that
    /// is correct rather than stuck. [`jkb_fsm::Defect::Wedged`] still applies to all three, and
    /// for them it is the check that matters: *is there an edge back to rest at all* is exactly
    /// the "refused on every pass for ever" defect.
    fn awaits_input(self) -> bool {
        matches!(self, Self::Conflicted | Self::Quarantined | Self::Blocked)
    }
}

impl FileState {
    /// The string this state is stored as in `sync_state.status`.
    ///
    /// **Two states share one spelling**, and this is where that is visible: a quarantine and a
    /// blocked write are both `needs_attention`. They are told apart on the way back in by
    /// whether the failing bytes were stashed — see [`FileState::from_journal`] — because the
    /// column cannot say. Collapsing them at the boundary rather than in the model is the point:
    /// the states are distinct where the reasoning happens, and a migration could separate the
    /// column later without touching a guard.
    ///
    /// `Untracked` has no spelling: it is the absence of a row.
    #[must_use]
    pub fn stored(self) -> Option<&'static str> {
        match self {
            Self::Untracked => None,
            Self::Settled => Some("ok"),
            Self::Conflicted => Some("conflict"),
            Self::Quarantined | Self::Blocked => Some("needs_attention"),
        }
    }

    /// Read a journal row back into a state.
    ///
    /// `stashed` is whether the row carries `quarantine_blob_hash` — the only thing that tells a
    /// quarantine from a blocked write, since both are stored `needs_attention`. A status this
    /// build does not recognize reads as `Blocked`, the most conservative of the flagged states:
    /// it owes attention and nothing is assumed about why.
    #[must_use]
    pub fn from_journal(status: Option<&str>, stashed: bool) -> Self {
        match status {
            None => Self::Untracked,
            Some("ok") => Self::Settled,
            Some("conflict") => Self::Conflicted,
            Some("needs_attention") if stashed => Self::Quarantined,
            _ => Self::Blocked,
        }
    }
}

/// What a sync pass observed about one file.
///
/// **Every event is [`EventKind::Reconciled`]** — there are no applied events at all, which is
/// the other way this machine differs from the task lifecycle. Nobody asks a file to import;
/// they edit it, and the next pass notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileEvent {
    /// First sight of a file the mount can import.
    Adopted,
    /// Only the disk changed.
    Imported,
    /// Only the store changed.
    Exported,
    /// Both changed, in different units, and the merge succeeded.
    Merged,
    /// Both changed the same unit, and the policy named a winner.
    ResolvedByPolicy,
    /// Neither side changed in substance.
    Unchanged,
    /// The store contributes nothing to a file that still declares items, and the mount can
    /// import — so the file is the good copy and is read back (design D45.5).
    ///
    /// Its own event rather than a case inside `Imported`, because it is the condition that has
    /// to **dominate every arm**: each direction arm got it wrong in its own way, and each would
    /// have needed its own gate. As a transition it is one row whose guard is evaluated against
    /// the same observation as every other, and if it and another both apply, `reconcile` says so.
    Recovered,
    /// Both changed the same unit and the policy is `manual`: neither side overwritten.
    Conflicted,
    /// The bytes did not parse.
    ParseFailed,
    /// The write would have removed item lines the file still declares.
    WriteBlocked,
}

impl Event for FileEvent {
    const ALL: &'static [Self] = &[
        Self::Adopted,
        Self::Imported,
        Self::Exported,
        Self::Merged,
        Self::ResolvedByPolicy,
        Self::Unchanged,
        Self::Recovered,
        Self::Conflicted,
        Self::ParseFailed,
        Self::WriteBlocked,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Adopted => "adopted",
            Self::Imported => "imported",
            Self::Exported => "exported",
            Self::Merged => "merged",
            Self::ResolvedByPolicy => "resolved_by_policy",
            Self::Unchanged => "unchanged",
            Self::Recovered => "recovered",
            Self::Conflicted => "conflicted",
            Self::ParseFailed => "parse_failed",
            Self::WriteBlocked => "write_blocked",
        }
    }

    fn kind(self) -> EventKind {
        EventKind::Reconciled
    }
}

/// What the journal write must carry, beyond the status itself.
///
/// Small, because the journal's other columns are carried forward by the engine's own writers —
/// what the machine owns is the **status**, which is the field that had four writers and no
/// shared rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEffect {
    /// Record the pass's rendering as the new base, and clear any flag.
    Settle,
    /// Keep the base untouched and flag the file with this reason.
    Flag(&'static str),
    /// Stash the failing bytes and flag the file.
    Stash,
}

/// Everything a guard may look at, gathered by one sync pass.
///
/// Every field the outside world answers is a [`Fact`], for the reason the task machine's are:
/// a file we could not stat, a document we could not render and a store we could not query are
/// all *unestablished*, and none of them is a licence to write.
#[derive(Debug, Clone, Copy)]
pub struct FileFacts {
    /// Where the file stands now.
    pub state: FileState,
    /// The file exists on disk.
    pub on_disk: Fact,
    /// Its bytes parsed.
    pub parses: Fact,
    /// The disk side differs from the recorded base.
    pub disk_changed: Fact,
    /// The store side differs from the recorded base.
    pub kb_changed: Fact,
    /// The two sides' edits touch different units, so they can be merged.
    pub disjoint: Fact,
    /// The store contributes **no** items to a file that declares some (design D45.5).
    pub store_empty_of_declared: Fact,
    /// Some item is still bound to this file — so an empty render is a lost placement, not lost
    /// items, and the two want opposite handling.
    pub items_still_bound: Fact,
    /// Writing would remove item lines the file still declares.
    pub would_drop_items: Fact,
    /// The mount permits disk → store.
    pub imports: Fact,
    /// The mount permits store → disk.
    pub exports: Fact,
    /// The mount's conflict policy.
    ///
    /// Deliberately **not** a [`Fact`]: it is stored configuration with a default, read before
    /// the pass begins, so it is never unestablished. `Fact` is for what the outside world
    /// answers, and using it for a value that is always known would make the three-valued
    /// discipline decorative.
    pub policy: Policy,
}

/// What the mount does when both sides changed the same unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Take the disk.
    DiskWins,
    /// Take the store, overwriting the file.
    KbWins,
    /// Overwrite neither; flag the file.
    Manual,
}

impl Stateful<FileState> for FileFacts {
    fn state(&self) -> FileState {
        self.state
    }
}

impl Default for FileFacts {
    fn default() -> Self {
        Self {
            state: FileState::Untracked,
            on_disk: Fact::Unknown,
            parses: Fact::Unknown,
            disk_changed: Fact::Unknown,
            kb_changed: Fact::Unknown,
            disjoint: Fact::Unknown,
            store_empty_of_declared: Fact::Unknown,
            items_still_bound: Fact::Unknown,
            would_drop_items: Fact::Unknown,
            imports: Fact::Unknown,
            exports: Fact::Unknown,
            policy: Policy::Manual,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Guards. Together they must **partition** the observation space: `reconcile` evaluates all of
// them against one observation, so two that both apply is `Ambiguous` rather than whichever the
// code reached first. That is D45.5's conclusion made checkable — and `audit` checks it over a
// modelled observation space rather than over a set of cases somebody thought of.
// ---------------------------------------------------------------------------------------------

/// The file is readable: it exists **and** parsed. Both proven — a file we could not stat and
/// one whose bytes we could not render are equally not a source to reconcile against.
fn readable(f: &FileFacts) -> Fact {
    f.on_disk.and(f.parses)
}

/// **The wholesale-loss condition owns its observation.** The store contributing nothing to a
/// file that declares items means the disk is the good copy, by this condition's own reasoning —
/// so it is decided before direction, not inside an arm, and every ordinary guard below requires
/// it proven absent.
///
/// That is D45.5's conclusion expressed as a *partition* rather than as a hoisted `if`. The
/// difference is that the partition is checkable: two conditions claiming one observation is
/// [`jkb_fsm::Reconciliation::Ambiguous`], which `audit` reports. Writing it as an ordering
/// (this arm first, then the dispatch) makes an overlap invisible — the first arm simply wins,
/// which is the failure mode the whole area kept producing.
///
/// Note this must be *proven absent* for an ordinary pass, not merely unproven: a store we could
/// not render tells us nothing, and nothing licenses a write.
fn ordinary(f: &FileFacts) -> Fact {
    readable(f).and(!f.store_empty_of_declared)
}

/// The recovery half of the loss case: the file is the good copy and the mount can read it back
/// (design D45.5).
///
/// `items_still_bound` must be proven **false**. An empty render is not proof of an empty store
/// — a bound item that merely lost its primary placement renders as nothing too — and those want
/// opposite handling: importing over live items destroys un-exported work, so that case belongs
/// to [`FileEvent::WriteBlocked`] and its one-command remedy.
fn recovering(f: &FileFacts) -> Verdict<FileEvent> {
    all_of([
        require_yes(readable(f), || Denial::new("the file is not readable.")),
        require_yes(f.store_empty_of_declared, || {
            Denial::new("the store still contributes items to this file.")
        }),
        require_no(f.items_still_bound, || {
            Denial::with_remedy(
                "items are still bound to this file, so an empty render is a lost placement \
                 rather than lost items.",
                FileEvent::WriteBlocked,
                "Restore the placement with `jkb task place <uid> <ns> --home`.",
            )
        }),
        require_yes(f.imports, || {
            Denial::with_remedy(
                "this mount cannot import, so the file cannot be read back.",
                FileEvent::WriteBlocked,
                "Nothing is written; re-read the file, or make the mount bidirectional.",
            )
        }),
    ])
}

/// First sight of a file.
///
/// Deliberately **not** gated on the mount direction. An export-only mount adopts a file too —
/// it writes the store's rendering over it — and the state change is the same either way. Which
/// direction the bytes move is `engine::reconcile`'s business; what this machine owns is that
/// the file goes from unknown to settled.
fn adopting(f: &FileFacts) -> Verdict<FileEvent> {
    require_yes(ordinary(f), || {
        Denial::new("there is nothing readable here to adopt.")
    })
}

fn importing(f: &FileFacts) -> Verdict<FileEvent> {
    all_of([
        require_yes(ordinary(f), || Denial::new("nothing to import.")),
        require_yes(f.disk_changed, || {
            Denial::new("the disk side is unchanged.")
        }),
        require_no(f.kb_changed, || Denial::new("the store side changed too.")),
        require_yes(f.imports, || Denial::new("this mount cannot import.")),
    ])
}

fn exporting(f: &FileFacts) -> Verdict<FileEvent> {
    all_of([
        require_yes(ordinary(f), || Denial::new("nothing to export.")),
        require_no(f.disk_changed, || Denial::new("the disk side changed too.")),
        require_yes(f.kb_changed, || Denial::new("the store side is unchanged.")),
        require_yes(f.exports, || Denial::new("this mount cannot export.")),
        require_no(f.would_drop_items, || {
            Denial::with_remedy(
                "the write would remove item lines this file still declares.",
                FileEvent::WriteBlocked,
                "Restore the missing placements, or delete those lines from the file.",
            )
        }),
    ])
}

/// Both sides changed. The three outcomes are three events, and they are told apart by **facts**
/// — not by the order of three arms inside one.
fn both_changed(f: &FileFacts) -> Fact {
    ordinary(f).and(f.disk_changed).and(f.kb_changed)
}

/// Whether this observation calls for writing the **store's rendering** over the file.
///
/// The one place the drop check applies, and the reason it is derived rather than asserted: a
/// merge writes the merged document, which contains both sides, so it cannot drop what the file
/// declares. Only the two arms that write the store's own rendering can.
fn would_write_kb_render(f: &FileFacts) -> Fact {
    let pure_export = (!f.disk_changed).and(f.kb_changed);
    let kb_wins = both_changed(f)
        .and(!f.disjoint)
        .and(Fact::from(f.policy == Policy::KbWins));
    pure_export.or(kb_wins).and(f.exports)
}

fn merging(f: &FileFacts) -> Verdict<FileEvent> {
    all_of([
        require_yes(both_changed(f), || Denial::new("only one side changed.")),
        require_yes(f.disjoint, || {
            Denial::with_remedy(
                "both sides changed the same unit.",
                FileEvent::Conflicted,
                "Resolve it in the file, or set the mount's conflict policy.",
            )
        }),
    ])
}

fn policy_resolving(f: &FileFacts) -> Verdict<FileEvent> {
    all_of([
        require_yes(both_changed(f), || Denial::new("only one side changed.")),
        require_no(f.disjoint, || {
            Denial::new("the edits are disjoint and merge.")
        }),
        require_no(Fact::from(f.policy == Policy::Manual), || {
            Denial::with_remedy(
                "the conflict policy is `manual`, so neither side is overwritten.",
                FileEvent::Conflicted,
                "Resolve it in the file, or re-run with `jkb sync --conflict disk_wins`.",
            )
        }),
        // `kb_wins` writes the store's rendering, so it is subject to the same drop refusal an
        // ordinary export is. `disk_wins` reads the file and cannot drop anything.
        require_no(would_write_kb_render(f).and(f.would_drop_items), || {
            Denial::with_remedy(
                "resolving in favour of the store would remove item lines this file declares.",
                FileEvent::WriteBlocked,
                "Restore the missing placements, or resolve in favour of the disk.",
            )
        }),
    ])
}

fn conflicting(f: &FileFacts) -> Verdict<FileEvent> {
    all_of([
        require_yes(both_changed(f), || Denial::new("only one side changed.")),
        require_no(f.disjoint, || {
            Denial::new("the edits are disjoint and merge.")
        }),
        require_yes(Fact::from(f.policy == Policy::Manual), || {
            Denial::new("the policy names a winner, so this is resolved rather than flagged.")
        }),
    ])
}

fn unchanged(f: &FileFacts) -> Verdict<FileEvent> {
    all_of([
        require_yes(ordinary(f), || Denial::new("the file is not readable.")),
        require_no(f.disk_changed, || Denial::new("the disk side changed.")),
        require_no(f.kb_changed, || Denial::new("the store side changed.")),
    ])
}

/// A parse failure is the one observation that needs nothing else established — the bytes are
/// there and they are not a document, whatever the mount mode or the hashes say.
fn parse_failed(f: &FileFacts) -> Verdict<FileEvent> {
    all_of([
        require_yes(f.on_disk, || Denial::new("there is no file to parse.")),
        require_no(f.parses, || Denial::new("the file parsed.")),
    ])
}

/// The write-blocked arm, reachable both from an export that would drop lines and from the
/// wholesale-loss case on a mount that cannot read the file back.
fn write_blocked(f: &FileFacts) -> Verdict<FileEvent> {
    // A write of the store's rendering that would remove lines. `would_write_kb_render` already
    // excludes the merge arm (which writes both sides) and `ordinary` excludes the loss case,
    // which owns that observation and decides it differently.
    let dropping = would_write_kb_render(f)
        .and(f.would_drop_items)
        .and(!f.store_empty_of_declared);
    // The loss case on a mount that cannot heal itself: items are still bound (so importing
    // would destroy live work), or the mount cannot read the file back at all.
    let unrecoverable = f
        .store_empty_of_declared
        .and(f.items_still_bound.or(!f.imports));
    all_of([
        require_yes(readable(f), || Denial::new("the file is not readable.")),
        require_yes(dropping.or(unrecoverable), || {
            Denial::new("no write would remove anything this file declares.")
        }),
    ])
}

// ---------------------------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------------------------

fn settle(_: &FileFacts) -> Vec<FileEffect> {
    vec![FileEffect::Settle]
}

fn stash(_: &FileFacts) -> Vec<FileEffect> {
    vec![FileEffect::Stash]
}

fn flag_conflict(_: &FileFacts) -> Vec<FileEffect> {
    vec![FileEffect::Flag("both sides changed the same unit")]
}

fn flag_blocked(_: &FileFacts) -> Vec<FileEffect> {
    vec![FileEffect::Flag(
        "writing would remove item lines this file declares",
    )]
}

// ---------------------------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------------------------

/// The table.
///
/// One row per `(state, event)`, generated from a list that names **which states** each event
/// applies in — so "a file reaches settled from wherever it was" is visible as a property of the
/// declaration rather than something you infer by reading forty rows. That property is what
/// makes a quarantine recoverable and a refusal clearable, and it is what eight review passes'
/// worth of must-fixes violated.
macro_rules! rows {
    ($( [$($from:ident),+] -$event:ident-> $to:ident : $guard:expr, $plan:expr; )+) => {
        &[$($(Transition {
            from: FileState::$from,
            event: FileEvent::$event,
            to: Dest::To(FileState::$to),
            guard: Some($guard),
            plan: Some($plan),
        },)+)+]
    };
}

/// Which states each event applies in.
///
/// `Untracked` is deliberately thin: with no journal row there is no base, so `disk_changed`,
/// `kb_changed` and `disjoint` cannot be established at all and every event that reads them is
/// unreachable there. Declaring those rows anyway would put edges in the diagram that nothing
/// can take.
const ROWS: &[Transition<FileState, FileEvent, FileFacts, FileEffect>] = rows! {
    // --- back to rest, from wherever the file was ------------------------------------------
    [Untracked]                                        -Adopted->          Settled : adopting, settle;
    [Settled, Conflicted, Quarantined, Blocked]        -Imported->         Settled : importing, settle;
    [Settled, Conflicted, Quarantined, Blocked]        -Exported->         Settled : exporting, settle;
    [Settled, Conflicted, Quarantined, Blocked]        -Merged->           Settled : merging, settle;
    [Settled, Conflicted, Quarantined, Blocked]        -ResolvedByPolicy-> Settled : policy_resolving, settle;
    [Settled, Conflicted, Quarantined, Blocked]        -Unchanged->        Settled : unchanged, settle;
    // The wholesale-loss recovery reaches every state, `Untracked` included: a file whose store
    // side is empty is exactly what a first sync after `jkb undo` looks like.
    [Untracked, Settled, Conflicted, Quarantined, Blocked]
                                                       -Recovered->        Settled : recovering, settle;

    // --- the three ways a pass ends without settling ---------------------------------------
    [Untracked, Settled, Conflicted, Blocked]          -ParseFailed->      Quarantined : parse_failed, stash;
    [Settled, Quarantined, Blocked]                    -Conflicted->       Conflicted : conflicting, flag_conflict;
    [Untracked, Settled, Conflicted, Quarantined]      -WriteBlocked->     Blocked : write_blocked, flag_blocked;
};

/// The sync journal's machine.
#[must_use]
pub fn machine() -> Machine<FileState, FileEvent, FileFacts, FileEffect> {
    Machine {
        transitions: ROWS,
        initial: FileState::Untracked,
    }
}

/// The journal status a pass's conclusion settles on — the **one** place that string is chosen.
///
/// `engine::reconcile` still decides direction and does the work; what it may not do is invent a
/// status. It names the conclusion it reached and gets back the state's spelling, so an arm that
/// produces a `(state, conclusion)` pair the machine does not declare fails loudly instead of
/// writing something nothing expects.
///
/// The guards are **not** re-run here. The engine has already established the facts through its
/// own reads, and asking it to re-assemble them as a [`FileFacts`] purely to be told what it
/// just worked out would be a second derivation of the same thing — which is the defect this
/// whole design is about. What is checked is the transition's *existence*: that this conclusion
/// is a thing that can happen to a file in this state at all.
///
/// # Errors
/// Returns a message naming the undeclared transition.
pub fn status_for(from: FileState, event: FileEvent) -> Result<&'static str, String> {
    use jkb_fsm::Acceptance;
    match machine().accepts(from, event) {
        Acceptance::Moves(to) => to.stored().ok_or_else(|| {
            format!(
                "`{}` in `{}` leads to `{}`, which has no stored spelling",
                event.name(),
                from.name(),
                to.name()
            )
        }),
        // The destination of a transition absorbs its own event: a file already quarantined that
        // fails to parse again stays quarantined, and that is a no-op rather than an error.
        Acceptance::Idempotent => from
            .stored()
            .ok_or_else(|| format!("`{}` has no stored spelling", from.name())),
        Acceptance::MovesAsStated | Acceptance::Undefined => Err(format!(
            "the sync journal has no `{}` transition out of `{}` — a reconcile arm reached a \
             conclusion the machine does not declare",
            event.name(),
            from.name()
        )),
    }
}

/// Take one reconciliation step against what this pass observed.
///
/// The **only** way a journal status changes. `reconcile` evaluates every candidate's guard
/// against the same observation, so two conditions that both apply come back as
/// [`jkb_fsm::Reconciliation::Ambiguous`] — reported, rather than resolved by whichever arm the
/// code reached first, which is what D45.5 concluded the hard way.
#[must_use]
pub fn observe(facts: &FileFacts) -> jkb_fsm::Reconciliation<FileState, FileEvent, FileEffect> {
    machine().reconcile(facts)
}

#[cfg(test)]
mod tests;
