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

// ---------------------------------------------------------------------------------------------
// The performing half. These were shell assertions until the decision moved into Rust; they run
// the real `Request::perform` against a stub notifier that records its argv, because what this
// code does is choose a command line and write a record.

use std::path::PathBuf;

/// A throwaway state directory plus a notifier that appends its argv to a file.
struct Fixture {
    dir: PathBuf,
    notifier: PathBuf,
    calls: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("jkb-notify-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let calls = dir.join("calls");
        let notifier = dir.join("notifier");
        std::fs::write(
            &notifier,
            format!(
                "#!/bin/sh\nprintf '%s ' \"$@\" >> {}\nprintf '\\n' >> {}\n",
                calls.display(),
                calls.display()
            ),
        )
        .expect("stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&notifier, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        Self {
            dir,
            notifier,
            calls,
        }
    }

    fn request(&self, event: NotifEvent, session: &str) -> super::Request {
        super::Request {
            event,
            id: format!("jkb-claude-{session}"),
            marker: self.dir.join(session),
            recorded: super::Record::read(&self.dir.join(session)),
            prompted_tool: Some("Bash".to_owned()),
            finished_tool: String::new(),
            message: "Claude needs your permission to use Bash".to_owned(),
            subtitle: "wt".to_owned(),
            notifier: Some(self.notifier.clone()),
        }
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    fn marker(&self, session: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(session)).ok()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Posting records the tool, so a later `PostToolUse` can tell its own prompt from a concurrent
/// one — and the record names the owner, which is the only thing `sweep` can ask about liveness.
#[test]
fn posting_records_the_tool_and_the_owner() {
    let f = Fixture::new("post");
    let req = f.request(NotifEvent::Needed, "s1");
    req.perform(&[NotifEffect::Post, NotifEffect::Remember]);
    assert_eq!(
        f.calls(),
        "post --id jkb-claude-s1 --title Claude Code --subtitle wt --body Claude needs your permission to use Bash"
    );
    let record = f.marker("s1").expect("a record");
    let mut lines = record.lines();
    assert_eq!(lines.next(), Some("Bash"));
    assert!(
        lines.next().is_some_and(|p| p.parse::<u32>().is_ok()),
        "the record names an owner pid: {record:?}"
    );
}

/// Withdraw and forget are one plan, and performing it leaves nothing behind.
#[test]
fn withdrawing_clears_the_record() {
    let f = Fixture::new("withdraw");
    f.request(NotifEvent::Needed, "s1")
        .perform(&[NotifEffect::Remember]);
    assert!(f.marker("s1").is_some());
    f.request(NotifEvent::ToolFinished, "s1")
        .perform(&[NotifEffect::Withdraw, NotifEffect::Forget]);
    assert_eq!(f.calls(), "remove --id jkb-claude-s1");
    assert!(f.marker("s1").is_none(), "the record is gone");
}

/// End to end through the real decision: a concurrent tool must not take the prompt down, and
/// the prompted one must.
#[test]
fn the_decision_and_the_effects_agree_about_a_concurrent_tool() {
    let f = Fixture::new("concurrent");
    {
        f.request(NotifEvent::Needed, "s1")
            .perform(&[NotifEffect::Remember]);

        let mut req = f.request(NotifEvent::ToolFinished, "s1");
        req.recorded = super::Record::read(&f.dir.join("s1"));
        req.finished_tool = "Read".to_owned();
        req.perform(machine().apply(&req.ctx(), req.event).effects());
        assert_eq!(f.calls(), "", "a different tool must withdraw nothing");
        assert!(f.marker("s1").is_some(), "and must not clear the record");

        req.finished_tool = "Bash".to_owned();
        req.perform(machine().apply(&req.ctx(), req.event).effects());
    }
    assert_eq!(f.calls(), "remove --id jkb-claude-s1");
    assert!(f.marker("s1").is_none());
}

/// A path-like session id must not be able to name a path.
#[test]
fn a_path_like_session_id_is_sanitised() {
    assert_eq!(super::sanitize("../../etc/passwd"), "______etc_passwd");
    assert_eq!(super::sanitize("ok-9_A"), "ok-9_A");
}

/// The tool is read out of prose, so its failure mode matters more than its success.
#[test]
fn the_tool_is_read_out_of_the_message() {
    assert_eq!(
        super::tool_in("Claude needs your permission to use Bash").as_deref(),
        Some("Bash")
    );
    assert_eq!(
        super::tool_in("permission to use Read the file").as_deref(),
        Some("Read")
    );
    assert_eq!(super::tool_in("Claude is waiting for your input"), None);
    assert_eq!(super::tool_in("permission to use "), None);
}

/// The sweep withdraws a provably-dead session's notification and spares every other kind.
///
/// This is the only route by which a killed session's Alerts-style notification — which waits
/// for ever by design — ever comes down, and the one place a wrong answer is expensive in the
/// other direction: withdrawing a live session's prompt is exactly the harm the feature exists
/// to prevent, so `Unknown` must keep the record rather than clear it.
#[test]
fn the_sweep_spares_everything_it_cannot_prove_dead() {
    let f = Fixture::new("sweep");
    // A pid that cannot be running, this process (certainly alive), and no owner at all.
    std::fs::write(f.dir.join("dead"), "Bash\n4294967294\n").expect("dead");
    std::fs::write(
        f.dir.join("live"),
        format!("Bash\n{}\n", std::process::id()),
    )
    .expect("live");
    std::fs::write(f.dir.join("unknown"), "Bash\n\n").expect("unknown");

    super::sweep_in(&f.dir, Some(&f.notifier));

    assert_eq!(f.calls(), "remove --id jkb-claude-dead");
    assert!(
        f.marker("dead").is_none(),
        "a dead session's record is cleared"
    );
    assert!(
        f.marker("live").is_some(),
        "a live session keeps its notification"
    );
    assert!(
        f.marker("unknown").is_some(),
        "an unprovable owner keeps its record: Unknown refuses"
    );
}

/// The banner is the path that runs when the notifier is unusable, and its script is built by
/// string concatenation out of Claude's own message text — which can carry every character that
/// ends an `AppleScript` string early. Getting it wrong means no notification at all, silently.
#[test]
fn the_banner_script_escapes_and_folds() {
    let script = super::banner_script("say \"hi\" \\ now\nplease", "wt");
    assert_eq!(
        script,
        r#"display notification "say \"hi\" \\ now please" with title "Claude Code" subtitle "wt""#
    );

    // ...and checked against a real compiler where there is one, rather than only against our own
    // idea of the grammar. Absent off macOS, so the suite skips it there.
    if std::process::Command::new("osacompile")
        .arg("-h")
        .output()
        .is_ok()
    {
        let out = std::process::Command::new("osacompile")
            .args(["-o", "/dev/null", "-e", &script])
            .output()
            .expect("osacompile runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
