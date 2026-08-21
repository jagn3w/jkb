//! The check is the point of the module; the rest pin the behaviours four review rounds argued
//! about, so a later edit that reopens one of them fails here rather than in a fifth round.

use super::{machine, NotifCtx, NotifEffect, NotifEvent, NotifState};
use jkb_fsm::{Fact, Outcome};

fn ctx(at: NotifState) -> NotifCtx {
    NotifCtx {
        at,
        notifier_usable: Fact::Yes,
        tool_named: true,
        tool_matches: Fact::Yes,
        session_alive: Fact::Unknown,
    }
}

/// **The artefact.** Every state reaches rest, every state is reachable, no two rows compete, no
/// reconciliation is unguarded, every verb runs twice, and nothing dead-ends. Each of those was a
/// question this feature could not answer while its rules were shell conditionals.
#[test]
fn the_table_has_no_defects() {
    let defects = machine().check();
    assert!(defects.is_empty(), "{defects:#?}");
}

/// The concurrency defect, which is what forced the tool-scoped state in the first place: one
/// assistant message batches several calls, and a slow allowlisted one finishing must not take
/// down a prompt nobody has answered.
#[test]
fn a_different_tool_finishing_does_not_withdraw() {
    let m = machine();
    let mut c = ctx(NotifState::AwaitingTool);
    c.tool_matches = Fact::No;

    let out = m.apply(&c, NotifEvent::ToolFinished);
    assert!(!out.moved(), "a mismatched tool must not withdraw: {out:?}");
    assert!(out.effects().is_empty());

    c.tool_matches = Fact::Yes;
    let out = m.apply(&c, NotifEvent::ToolFinished);
    assert_eq!(out.state(), NotifState::Absent);
    assert_eq!(
        out.effects(),
        &[NotifEffect::Withdraw, NotifEffect::Forget],
        "withdrawing and forgetting are one plan, never half applied"
    );
}

/// A tool finishing says nothing about a notification whose prompt named no tool. There is no
/// question to ask, so there is no row — a named refusal rather than a silent no-op.
#[test]
fn a_tool_event_is_undefined_for_an_untooled_notification() {
    let out = machine().apply(&ctx(NotifState::AwaitingUser), NotifEvent::ToolFinished);
    assert!(matches!(out, Outcome::Undefined { .. }), "{out:?}");
}

/// The sweep does not consult the record, because the record may be the thing that failed. An
/// unwritable marker reads as `Absent` while an Alerts-style notification waits on screen for
/// ever, so `Stop` withdraws anyway.
#[test]
fn the_sweep_withdraws_from_absent() {
    for event in [NotifEvent::TurnEnded, NotifEvent::SessionEnded] {
        let out = machine().apply(&ctx(NotifState::Absent), event);
        assert_eq!(out.effects(), &[NotifEffect::Withdraw], "{event:?}");
    }
}

/// Liveness by evidence, never by age (D27): `Unknown` must refuse, or a paused-but-alive
/// session loses the notification it is waiting on.
#[test]
fn an_unprovable_session_death_refuses() {
    let m = machine();
    let mut c = ctx(NotifState::AwaitingTool);

    c.session_alive = Fact::Unknown;
    assert!(!m.apply(&c, NotifEvent::SessionGone).moved());
    c.session_alive = Fact::Yes;
    assert!(!m.apply(&c, NotifEvent::SessionGone).moved());

    c.session_alive = Fact::No;
    let out = m.apply(&c, NotifEvent::SessionGone);
    assert_eq!(out.state(), NotifState::Absent);
    assert_eq!(out.effects(), &[NotifEffect::Withdraw, NotifEffect::Forget]);
}

/// An unusable notifier — absent, unauthorized, or with its alert style set to `none`, which
/// macOS accepts while showing nothing — must fall back to a banner the user can actually see,
/// and must not record a notification that cannot be withdrawn.
#[test]
fn an_unusable_notifier_falls_back_to_a_banner() {
    let m = machine();
    for fact in [Fact::No, Fact::Unknown] {
        let mut c = ctx(NotifState::Absent);
        c.notifier_usable = fact;
        let out = m.apply(&c, NotifEvent::Needed);
        assert_eq!(out.state(), NotifState::Absent, "{fact:?}");
        assert_eq!(out.effects(), &[NotifEffect::Banner], "{fact:?}");
    }
}

/// Whether the prompt named a tool decides which posted state we land in — the parse failing is
/// a declared destination, not a fallen-through branch.
#[test]
fn a_needed_lands_by_whether_a_tool_was_named() {
    let m = machine();
    let mut c = ctx(NotifState::Absent);

    let out = m.apply(&c, NotifEvent::Needed);
    assert_eq!(out.state(), NotifState::AwaitingTool);
    assert_eq!(out.effects(), &[NotifEffect::Post, NotifEffect::Remember]);

    c.tool_named = false;
    let out = m.apply(&c, NotifEvent::Needed);
    assert_eq!(out.state(), NotifState::AwaitingUser);
    assert_eq!(out.effects(), &[NotifEffect::Post, NotifEffect::Remember]);
}

/// A second prompt replaces the first rather than stacking, and re-posting over a live
/// notification is allowed from either posted state.
#[test]
fn a_second_prompt_re_posts() {
    let m = machine();
    for at in [NotifState::AwaitingTool, NotifState::AwaitingUser] {
        let out = m.apply(&ctx(at), NotifEvent::Needed);
        assert_eq!(out.state(), NotifState::AwaitingTool);
        assert_eq!(out.effects(), &[NotifEffect::Post, NotifEffect::Remember]);
    }
}
