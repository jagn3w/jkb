//! A declarative, checkable state machine (design S1, `openspec/changes/jkb-state-machine/`).
//!
//! A lifecycle declared here is a `&'static` **table** — states, guarded transitions, and the
//! effects each transition must be accompanied by — rather than a `match` spread over the
//! commands that touch the object. That difference is the whole crate: a table can be walked,
//! and walking it is what makes these checkable at all —
//!
//! * every state can reach a terminal one ([`Defect::Wedged`]), so nothing is held for ever;
//! * every state is reachable ([`Defect::UnreachableState`]);
//! * no pair of rows silently competes ([`Defect::Nondeterministic`]);
//! * every *reconciliation* — a transition the world forces on us, rather than one somebody
//!   asked for — carries evidence ([`Defect::UnguardedReconciliation`]);
//! * a refusal's advice actually works ([`Defect::UnreachableRemedy`]);
//! * and under every observation, something can still move the object ([`Defect::DeadEnd`]).
//!
//! Three ideas do most of the work:
//!
//! **[`Fact`] is three-valued.** Facts about the outside world can be *unobtainable*, and
//! spelling that as `false` is this repository's most-repeated defect. `Fact` has `is_yes` and
//! `is_no` — both meaning *proven* — and nothing that collapses [`Fact::Unknown`] to a `bool`,
//! so a guard has to say which direction is safe for it.
//!
//! **A transition yields its effects as data.** [`Outcome::Moved`] carries a `Vec` of the
//! domain's own effect type, produced with the move, so a caller cannot perform half of a
//! transition. The machine performs nothing itself and knows nothing about databases or git.
//!
//! **A refusal names an event, not a sentence.** [`Denial::remedy`] is a [`Remedy`] holding an
//! event of this machine, so "here is what to do instead" is checkable — and a remedy that
//! cannot be expressed as an event means the machine is missing a transition.
//!
//! ```
//! use jkb_fsm::{Denial, Dest, Event, EventKind, Fact, Machine, Outcome, State, Stateful, Transition, Verdict};
//!
//! #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
//! enum Door { Open, Closed, Welded }
//! impl State for Door {
//!     const ALL: &'static [Self] = &[Self::Open, Self::Closed, Self::Welded];
//!     fn name(self) -> &'static str {
//!         match self { Self::Open => "open", Self::Closed => "closed", Self::Welded => "welded" }
//!     }
//!     fn is_settled(self) -> bool { matches!(self, Self::Welded) }
//! }
//!
//! #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
//! enum Act { Shut, Weld }
//! impl Event for Act {
//!     const ALL: &'static [Self] = &[Self::Shut, Self::Weld];
//!     fn name(self) -> &'static str { match self { Self::Shut => "shut", Self::Weld => "weld" } }
//!     fn kind(self) -> EventKind { EventKind::Applied }
//! }
//!
//! struct Ctx { at: Door, latched: Fact }
//! impl Stateful<Door> for Ctx { fn state(&self) -> Door { self.at } }
//!
//! static ROWS: &[Transition<Door, Act, Ctx, ()>] = &[
//!     Transition { from: Door::Open, event: Act::Shut, to: Dest::To(Door::Closed), guard: None, plan: None },
//!     Transition {
//!         from: Door::Closed, event: Act::Weld, to: Dest::To(Door::Welded),
//!         guard: Some(|c: &Ctx| if c.latched.is_yes() { Verdict::Allow }
//!                    else { Verdict::Deny(Denial::with_remedy("it is not latched.", Act::Shut, "Shut it first.")) }),
//!         plan: None,
//!     },
//! ];
//! let m = Machine { transitions: ROWS, initial: Door::Open };
//! assert!(m.check().is_empty());
//!
//! // Asking twice is not an error: `shut` on a shut door is a no-op, declared by nobody.
//! let ctx = Ctx { at: Door::Closed, latched: Fact::Yes };
//! assert!(matches!(m.apply(&ctx, Act::Shut), Outcome::Idempotent { .. }));
//!
//! // A fact we could not establish refuses, and says what would fix it.
//! let unknown = Ctx { at: Door::Closed, latched: Fact::Unknown };
//! let out = m.apply(&unknown, Act::Weld);
//! assert_eq!(out.refusal().as_deref(), Some("it is not latched. Shut it first."));
//! ```

//! ## What a second machine changed
//!
//! The library was written against one lifecycle, and the second one — the sync journal, a
//! *reconciler* rather than a lifecycle — moved three things. Recorded here because "it
//! generalizes" is a claim, and this is the evidence:
//!
//! 1. **`is_terminal` became [`State::is_settled`].** A synced file is never finished; it
//!    settles and is then edited again. Under the old name the machine either had no terminal
//!    state — making [`Defect::Wedged`] vacuous — or had to lie about one. The property the
//!    checks want is *rest*: the object owes the system nothing.
//! 2. **[`State::awaits_input`] was added.** A conflicted file is waiting on a person; no
//!    observation moves it, and that is correct rather than stuck. Without it,
//!    [`Defect::DeadEnd`] fired on every such observation. A lifecycle leaves the default
//!    (`false`) because an operator escape is always available.
//! 3. **The *initial* state can be at rest.** A file an export-only mount has never imported and
//!    holds no items for is nobody's business. Reachability is measured from `initial` and is
//!    indifferent to whether it settles, so nothing else had to move.
//!
//! What did **not** move is the part that carries the value: [`Fact`], the plan-as-data, the
//! remedy-as-event, and [`Machine::reconcile`] refusing ambiguity — which in the reconciler turns
//! out to be the central property rather than a corner case, because *"a route is not a cause;
//! the condition must dominate every arm"* is precisely what evaluating every candidate's guard
//! against one observation gives you.

mod check;
mod fact;
mod machine;

pub use check::Defect;
pub use fact::Fact;
pub use machine::{
    all_of, require_no, require_yes, Acceptance, Denial, Dest, Event, EventKind, Machine, Outcome,
    Reconciliation, Remedy, State, Stateful, Transition, Verdict,
};
