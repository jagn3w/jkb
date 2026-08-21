//! The permission-notification lifecycle as a checkable table (design N, `openspec/changes/
//! jkb-notification-lifecycle/`).
//!
//! Claude Code's "needs your permission" notification has to stay on screen and then come down
//! by itself once you have acted. That rule lived as conditionals spread across a shell hook,
//! and four review rounds produced 4, then 8, then 11, then 13 findings — the count rising each
//! time, because each round's fix was where the next round's defect lived. Nothing could be
//! *asked* of the rules: not "can this notification always come down", not "which events move
//! it", not "is there a state nothing exits". Every answer came from re-reading the script.
//!
//! Here the rules are a `&'static` table [`jkb_fsm`] can walk, and the questions above are its
//! [`jkb_fsm::Defect`] checks. The one in [`tests`] asserting the table is defect-free is the
//! artefact this module exists for; without it this is the same conditionals in a new shape.

use jkb_fsm::{
    require_no, require_yes, Denial, Dest, Event, EventKind, Fact, Machine, State, Stateful,
    Transition, Verdict,
};

/// Where one session's notification stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifState {
    /// Nothing withdrawable of ours is on screen. Initial, and the only resting state.
    Absent,
    /// Posted, and the prompt named the tool it is about.
    AwaitingTool,
    /// Posted, with no tool known — the idle prompt, or a message we could not parse.
    AwaitingUser,
}

impl State for NotifState {
    const ALL: &'static [Self] = &[Self::Absent, Self::AwaitingTool, Self::AwaitingUser];

    fn name(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::AwaitingTool => "awaiting_tool",
            Self::AwaitingUser => "awaiting_user",
        }
    }

    /// Only `Absent`. A posted notification owes the system a withdrawal, which is the whole
    /// point: [`jkb_fsm::Defect::Wedged`] then insists every way of being posted has a way back,
    /// and the orphan left by a killed session — the defect a reviewer found by hand — is
    /// exactly a state with no such edge.
    fn is_settled(self) -> bool {
        matches!(self, Self::Absent)
    }

    /// Deliberately left `false` everywhere. A posted notification *is* waiting on a person, but
    /// the system can still move it — a tool finishes, the turn ends — so declaring it would make
    /// [`jkb_fsm::Defect::DeadEnd`] vacuous on the two states where it does the work.
    fn awaits_input(self) -> bool {
        false
    }
}

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifEvent {
    /// Claude Code's `Notification` hook: permission is needed, or it is idle waiting for input.
    Needed,
    /// `PostToolUse`. Evidence about the prompt only when it names the same tool.
    ToolFinished,
    /// `UserPromptSubmit` — the user is demonstrably back.
    UserActed,
    /// `Stop`. The turn is over, so nothing of this turn's is still waiting.
    TurnEnded,
    /// `SessionEnd`.
    SessionEnded,
    /// Nobody asked: we looked, and the session that posted this is provably gone.
    SessionGone,
}

impl Event for NotifEvent {
    const ALL: &'static [Self] = &[
        Self::Needed,
        Self::ToolFinished,
        Self::UserActed,
        Self::TurnEnded,
        Self::SessionEnded,
        Self::SessionGone,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Needed => "needed",
            Self::ToolFinished => "tool_finished",
            Self::UserActed => "user_acted",
            Self::TurnEnded => "turn_ended",
            Self::SessionEnded => "session_ended",
            Self::SessionGone => "session_gone",
        }
    }

    /// `SessionGone` is the only thing here nobody asks for, and the crate then refuses to let it
    /// be declared without a guard — which is the rule an age-based liveness shortcut breaks.
    fn kind(self) -> EventKind {
        match self {
            Self::SessionGone => EventKind::Reconciled,
            _ => EventKind::Applied,
        }
    }
}

/// Something the caller must perform. Produced *with* the move, as one value, so half a
/// transition cannot be applied — clearing the record without withdrawing, or withdrawing
/// without clearing, are both real defects from this feature's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifEffect {
    /// Post (or replace) this session's notification through the notifier bundle.
    Post,
    /// Withdraw it.
    Withdraw,
    /// Post a plain `osascript` banner: visible, but auto-hiding and impossible to withdraw.
    Banner,
    /// Record that a withdrawable notification is on screen, and which tool it is about.
    Remember,
    /// Forget that record.
    Forget,
}

/// What a guard reads. The state lives **in here** and is read out via [`Stateful`], never passed
/// beside it — the crate's own history records a machine whose guard branched on one state while
/// it was being asked about another.
pub struct NotifCtx {
    /// Read from the per-session record.
    pub at: NotifState,
    /// Can we post something the user will actually see *and* take back? Folds installed,
    /// authorized and alert-style-not-`none`. `Unknown` behaves as `No`: a banner the user sees
    /// beats a silent success, and macOS accepts an unauthorized post without error.
    pub notifier_usable: Fact,
    /// Did the prompt name a tool? Decides which of the two posted states we land in.
    pub tool_named: bool,
    /// Is the tool that just finished the one the prompt named?
    pub tool_matches: Fact,
    /// Is the session that posted this still alive? `No` only on positive evidence — never age.
    pub session_alive: Fact,
}

impl Stateful<NotifState> for NotifCtx {
    fn state(&self) -> NotifState {
        self.at
    }
}

/// Where a `Needed` lands, stated by the context rather than by the table.
///
/// Three destinations share one `(state, event)` pair — no notifier, tool named, tool not named —
/// and three rows would be [`jkb_fsm::Defect::Nondeterministic`], which is the check earning its
/// keep before this ever ran: the shell version really did have three competing branches and
/// nothing that could say so.
///
/// It always answers: the three cases are exhaustive, so there is no "named nothing usable" state
/// to refuse from. The `Option` is [`Dest::Stated`]'s signature, not a possibility this function
/// has — hence the narrow allow rather than a `None` arm nothing could reach.
#[allow(clippy::unnecessary_wraps)]
fn needed_dest(c: &NotifCtx) -> Option<NotifState> {
    Some(if !c.notifier_usable.is_yes() {
        NotifState::Absent
    } else if c.tool_named {
        NotifState::AwaitingTool
    } else {
        NotifState::AwaitingUser
    })
}

/// What a `Needed` does: post and record, or fall back to a banner nobody can withdraw.
fn needed_plan(c: &NotifCtx) -> Vec<NotifEffect> {
    if c.notifier_usable.is_yes() {
        vec![NotifEffect::Post, NotifEffect::Remember]
    } else {
        vec![NotifEffect::Banner]
    }
}

/// Withdraw and forget, together, always.
fn clear_plan(_: &NotifCtx) -> Vec<NotifEffect> {
    vec![NotifEffect::Withdraw, NotifEffect::Forget]
}

/// The blind sweep: withdraw without believing anything is there.
fn sweep_plan(_: &NotifCtx) -> Vec<NotifEffect> {
    vec![NotifEffect::Withdraw]
}

/// A tool finishing is evidence about *this* prompt only when it is the same tool. One assistant
/// message routinely batches several calls, so a slow allowlisted one finishes while another's
/// prompt is still unanswered — withdrawing there leaves the session blocked with nothing on
/// screen, which is the state the feature exists to prevent.
fn tool_matches_guard(c: &NotifCtx) -> Verdict<NotifEvent> {
    require_yes(c.tool_matches, || {
        Denial::new("a different tool finished; this prompt is still waiting.")
    })
}

/// Liveness is by evidence, never by age (D27). `Unknown` refuses, so an orphan survives rather
/// than a paused-but-alive session losing the notification it is waiting on.
fn session_gone_guard(c: &NotifCtx) -> Verdict<NotifEvent> {
    require_no(c.session_alive, || {
        Denial::new("the session that posted this may still be alive.")
    })
}

const ROWS: &[Transition<NotifState, NotifEvent, NotifCtx, NotifEffect>] = &[
    // --- something needs attention -----------------------------------------------------------
    // Re-posting over a live notification is deliberate: the id is the session's, so a second
    // prompt replaces the first rather than stacking.
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::Needed,
        to: Dest::Stated(needed_dest),
        guard: None,
        plan: Some(needed_plan),
    },
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::Needed,
        to: Dest::Stated(needed_dest),
        guard: None,
        plan: Some(needed_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::Needed,
        to: Dest::Stated(needed_dest),
        guard: None,
        plan: Some(needed_plan),
    },
    // --- the user dealt with it --------------------------------------------------------------
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::ToolFinished,
        to: Dest::To(NotifState::Absent),
        guard: Some(tool_matches_guard),
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::UserActed,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::UserActed,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    // --- the turn or session ended -----------------------------------------------------------
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::TurnEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::TurnEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::SessionEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::SessionEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(clear_plan),
    },
    // The blind sweeps. `Absent` can be WRONG — if the record could not be written, every
    // state read from it says `absent` while an Alerts-style notification, which waits for ever
    // by design, sits on screen. These carry a plan, so the destination does not absorb them,
    // and one call per turn bounds that failure to the turn.
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::TurnEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(sweep_plan),
    },
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::SessionEnded,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: Some(sweep_plan),
    },
    // Nothing pending. These are the two most frequent events in the whole system — a tool
    // finishes after every single call — and `check()` named them `Unrepeatable` before this
    // ever ran: withdraw once and the next `PostToolUse` had no row at all. Declared with no
    // guard and no plan, so the destination absorbs them and they cost nothing, which is exactly
    // what "there is no notification, carry on" should cost.
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::ToolFinished,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: None,
    },
    Transition {
        from: NotifState::Absent,
        event: NotifEvent::UserActed,
        to: Dest::To(NotifState::Absent),
        guard: None,
        plan: None,
    },
    // --- the world moved -------------------------------------------------------------------
    Transition {
        from: NotifState::AwaitingTool,
        event: NotifEvent::SessionGone,
        to: Dest::To(NotifState::Absent),
        guard: Some(session_gone_guard),
        plan: Some(clear_plan),
    },
    Transition {
        from: NotifState::AwaitingUser,
        event: NotifEvent::SessionGone,
        to: Dest::To(NotifState::Absent),
        guard: Some(session_gone_guard),
        plan: Some(clear_plan),
    },
];

/// The lifecycle.
#[must_use]
pub fn machine() -> Machine<NotifState, NotifEvent, NotifCtx, NotifEffect> {
    Machine {
        transitions: ROWS,
        initial: NotifState::Absent,
    }
}

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------------------------
// The read-only half of the seam.
//
// `jkb notify plan` decides and prints; the shell hook still performs. That split is the crate's
// own model — the machine performs nothing and knows nothing about notifiers — and it is what
// lets the decision move to Rust without the shipped feature changing behaviour on the way.
//
// It deliberately owns NEITHER the notifier's path nor the state directory: both are read from
// the environment seams the hook already exports (`JKB_NOTIFIER`, `JKB_NOTIFY_STATE`). A second
// copy of the install path is precisely the defect this branch has already had to fix twice.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command as Proc;

use anyhow::{Context, Result};

use crate::NotifyCmd;

/// Run a `jkb notify` verb.
///
/// # Errors
/// If stdin cannot be read or does not parse as a hook payload.
pub fn run(cmd: &NotifyCmd) -> Result<()> {
    match cmd {
        NotifyCmd::Plan => plan(),
    }
}

/// Reduce a session id to characters inert in both a notification id and a filename.
fn sanitize(session: &str) -> String {
    session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The tool a permission prompt is about, read out of the message text because the payload names
/// none. A failure here is not an error: it lands the notification in `AwaitingUser`, which is a
/// declared state with declared behaviour rather than a fallen-through branch.
fn tool_in(message: &str) -> Option<String> {
    let rest = message.split("permission to use ").nth(1)?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Can we post something the user will see *and* take back? `Unknown` on anything we could not
/// establish — never `No`, which would claim we asked.
fn notifier_usable() -> Fact {
    let Ok(bin) = std::env::var("JKB_NOTIFIER") else {
        return Fact::Unknown;
    };
    let Ok(out) = Proc::new(&bin).arg("status").output() else {
        return Fact::Unknown;
    };
    if !out.status.success() {
        return Fact::Unknown;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let authorized = text.contains("authorization=authorized");
    // `alert-style=none` is accepted by macOS and displays nothing, so it is not usable even
    // though the post would succeed and report success.
    let visible = !text.contains("alert-style=none");
    if authorized && visible {
        Fact::Yes
    } else {
        Fact::No
    }
}

fn state_dir() -> PathBuf {
    std::env::var("JKB_NOTIFY_STATE").map_or_else(
        |_| std::env::temp_dir().join("jkb-claude-notify"),
        PathBuf::from,
    )
}

fn plan() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .context("reading the hook payload from stdin")?;
    let payload: serde_json::Value =
        serde_json::from_str(&raw).context("the hook payload is not JSON")?;

    let field = |k: &str| {
        payload
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned()
    };
    let session = sanitize(&field("session_id"));
    let event = match field("hook_event_name").as_str() {
        "Notification" => Some(NotifEvent::Needed),
        "PostToolUse" => Some(NotifEvent::ToolFinished),
        "UserPromptSubmit" => Some(NotifEvent::UserActed),
        "Stop" => Some(NotifEvent::TurnEnded),
        "SessionEnd" => Some(NotifEvent::SessionEnded),
        _ => None,
    };
    let (Some(event), false) = (event, session.is_empty()) else {
        println!(
            "{}",
            serde_json::json!({ "effects": [], "reason": "not an event we act on" })
        );
        return Ok(());
    };

    // State comes from the record, which is the single source the machine reads through
    // `Stateful` — never a second value passed beside it.
    let recorded = std::fs::read_to_string(state_dir().join(&session)).ok();
    let remembered_tool = recorded.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let at = match (&recorded, remembered_tool) {
        (None, _) => NotifState::Absent,
        (Some(_), Some(_)) => NotifState::AwaitingTool,
        (Some(_), None) => NotifState::AwaitingUser,
    };

    let message = field("message");
    let prompted_tool = tool_in(&message);
    let finished_tool = field("tool_name");
    let ctx = NotifCtx {
        at,
        notifier_usable: if event == NotifEvent::Needed {
            notifier_usable()
        } else {
            Fact::Unknown
        },
        tool_named: prompted_tool.is_some(),
        tool_matches: match remembered_tool {
            Some(want) if !finished_tool.is_empty() => Fact::from(want == finished_tool),
            _ => Fact::Unknown,
        },
        // `plan` observes one live session; proving another one dead is `sweep`'s job (N10).
        session_alive: Fact::Unknown,
    };

    let out = machine().apply(&ctx, event);
    let effects: Vec<&str> = out
        .effects()
        .iter()
        .map(|e| match e {
            NotifEffect::Post => "post",
            NotifEffect::Withdraw => "withdraw",
            NotifEffect::Banner => "banner",
            NotifEffect::Remember => "remember",
            NotifEffect::Forget => "forget",
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "state": out.state().name(),
            "moved": out.moved(),
            "effects": effects,
            "tool": prompted_tool,
            "refusal": out.refusal(),
        })
    );
    Ok(())
}
