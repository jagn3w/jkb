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
    require_no, require_yes, Denial, Dest, Event, EventKind, Fact, Machine, Reconciliation, State,
    Stateful, Transition, Verdict,
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
///
/// Each effect changes **the screen** or **the record**, and that split carries a rule: within a
/// plan, every screen effect comes before every record effect, and [`Request::perform`] stops at
/// the first one it cannot carry out. Together those mean a partly-applied plan always leaves the
/// record describing something that is still on screen — recoverable, because the next event
/// tries again — and never the reverse, which is unrecoverable: the record is the only thing that
/// remembers a notification exists. `plans_change_the_screen_before_the_record` walks every plan
/// the table can produce and holds it to that, so it is a property of the machine rather than a
/// rule each plan has to remember.
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

impl NotifEffect {
    /// Whether this effect changes what the user can see, as opposed to what we remember.
    ///
    /// Read by `plans_change_the_screen_before_the_record`, which is where the ordering rule is
    /// enforced — the production path relies on the rule holding, not on asking per effect.
    #[cfg_attr(not(test), allow(dead_code))]
    fn touches_screen(&self) -> bool {
        matches!(self, Self::Post | Self::Withdraw | Self::Banner)
    }
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
///
/// **REVERTED to `[Banner]`, and not re-patched.** It was briefly `[Withdraw, Banner, Forget]`,
/// to stop the record saying `awaiting_tool` while the destination said `absent`. That fixed a
/// divergence and caused a worse defect: with no notifier bundle at all, `perform` cannot apply
/// `Withdraw`, stops there, and the banner never goes out — so a permission prompt produced *no
/// notification whatsoever*, which is worse than before any of this existed.
///
/// Keeping the record is also the safer half of the divergence it was meant to fix. A record that
/// still names the notification is the route BACK: the next `TurnEnded` or `UserActed` finds
/// `awaiting_tool` and plans `[Withdraw, Forget]`, so what is on screen still comes down. Clearing
/// it would have made the machine's story tidy and the notification unreachable.
///
/// The divergence itself is real and stays open as a finding rather than being fixed here — the
/// honest repair is to the *destination* (stay put when we could not post), which is a design
/// change, and this round has already shown what re-patching a plan under time pressure costs.
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
        NotifyCmd::Events => {
            for (name, _) in HOOK_EVENTS {
                println!("{name}");
            }
            Ok(())
        }
        NotifyCmd::Plan => plan(),
        NotifyCmd::Hook => hook(),
        NotifyCmd::Sweep => {
            sweep();
            Ok(())
        }
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

/// Where the notifier bundle is, as told to us by the shim.
///
/// Deliberately **not** a second copy of the search list. `.claude/hooks/notify-sticky.sh` owns
/// it — `scripts/build-notifier.sh` and `scripts/setup.sh` already ask it, and those run in CI
/// and on a machine where `jkb` may not be built yet — so it resolves the path and passes it in.
/// A second copy is the defect this branch has already had to fix twice.
fn notifier_path() -> Option<PathBuf> {
    let bin = PathBuf::from(std::env::var("JKB_NOTIFIER").ok()?);
    bin.is_file().then_some(bin)
}

/// Can we post something the user will see *and* take back?
///
/// `Unknown` for anything we could not establish — never `No`, which would claim we asked. The
/// caller treats `Unknown` like `No` and shows a banner, because a banner the user sees beats a
/// silent success: macOS accepts an unauthorized post, and one with alert style `none`, without
/// any error at all.
fn usable(bin: &std::path::Path) -> Fact {
    let Ok(out) = Proc::new(bin).arg("status").output() else {
        return Fact::Unknown;
    };
    if !out.status.success() {
        return Fact::Unknown;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Fact::from(text.contains("authorization=authorized") && !text.contains("alert-style=none"))
}

fn state_dir() -> PathBuf {
    std::env::var("JKB_NOTIFY_STATE").map_or_else(
        |_| std::env::temp_dir().join("jkb-claude-notify"),
        PathBuf::from,
    )
}

/// One hook invocation: the payload, the paths it resolves to, and the record it reads.
///
/// Built once and shared by deciding and performing, so the two cannot disagree about which
/// session, which tool, or which record they are talking about.
struct Request {
    event: NotifEvent,
    /// The notification id — the session, so parallel worktree sessions cannot clear each
    /// other's, and a second prompt replaces the first rather than stacking.
    id: String,
    marker: PathBuf,
    /// What the record already holds: the tool the prompt named, and where the session's
    /// transcript lives. The transcript is the only evidence `sweep` has that a session died.
    recorded: Option<Record>,
    prompted_tool: Option<String>,
    finished_tool: String,
    message: String,
    subtitle: String,
    notifier: Option<PathBuf>,
    /// The process whose existence answers "is this session still alive?" — resolved once, here
    /// at the edge, so nothing below reads the environment for it.
    owner: String,
}

/// The per-session record: the tool the prompt named, and the process to ask about liveness.
///
/// The owner is handed over by the shim (`JKB_HOOK_OWNER`), which measurement shows is the
/// `claude` process itself. **`jkb` cannot ask for it**: its own parent is the shim, a bash script
/// that exits milliseconds later, so recording that made every record read as *provably dead* and
/// the next session's sweep withdrew a live session's pending prompt — the exact harm this
/// feature exists to prevent.
///
/// Liveness is by owner-existence, never by age, which is D27's rule for claims and holds for the
/// same reason here: a paused-but-alive session must keep the notification it is waiting on. A
/// recycled pid reads as alive, which leaves an orphan up — the safe direction.
struct Record {
    tool: String,
    owner: String,
}

impl Record {
    fn read(path: &std::path::Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        let mut lines = raw.lines();
        Some(Self {
            tool: lines.next().unwrap_or_default().trim().to_owned(),
            owner: lines.next().unwrap_or_default().trim().to_owned(),
        })
    }
}

impl Request {
    /// Build a request from the payload, reading every ambient value at the edge.
    ///
    /// The state directory, notifier and owner arrive as arguments rather than being fetched from
    /// the environment in here, because this is the payload-to-observation layer and it had no
    /// tests for exactly that reason — every defect the fifth review found lives on this path.
    fn parse(raw: &str) -> Result<Option<Self>> {
        Self::parse_with(raw, &state_dir(), notifier_path().as_deref(), &owner_id())
    }

    fn parse_with(
        raw: &str,
        state_dir: &std::path::Path,
        notifier: Option<&std::path::Path>,
        owner: &str,
    ) -> Result<Option<Self>> {
        let payload: serde_json::Value =
            serde_json::from_str(raw).context("the hook payload is not JSON")?;
        let field = |k: &str| {
            payload
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned()
        };

        let session = sanitize(&field("session_id"));
        let Some(event) = event_for(&field("hook_event_name")) else {
            return Ok(None);
        };
        if session.is_empty() {
            return Ok(None);
        }

        let marker = state_dir.join(&session);
        let cwd = field("cwd");
        let message = field("message");
        Ok(Some(Self {
            event,
            id: format!("jkb-claude-{session}"),
            recorded: Record::read(&marker),
            prompted_tool: tool_in(&message),
            finished_tool: field("tool_name"),
            subtitle: cwd.rsplit('/').next().unwrap_or_default().to_owned(),
            notifier: notifier.map(std::path::Path::to_path_buf),
            owner: owner.to_owned(),
            marker,
            message,
        }))
    }

    /// The observation the machine reads. The state comes out of the record and nowhere else.
    fn ctx(&self) -> NotifCtx {
        let at = match self.recorded.as_ref() {
            None => NotifState::Absent,
            Some(r) if r.tool.is_empty() => NotifState::AwaitingUser,
            Some(_) => NotifState::AwaitingTool,
        };
        NotifCtx {
            at,
            // Asked only where it is consulted: probing the notifier costs a subprocess, and the
            // frequent events do not need it.
            notifier_usable: if self.event == NotifEvent::Needed {
                self.notifier.as_deref().map_or(Fact::Unknown, usable)
            } else {
                Fact::Unknown
            },
            tool_named: self.prompted_tool.is_some(),
            tool_matches: match self.recorded.as_ref() {
                Some(r) if !r.tool.is_empty() && !self.finished_tool.is_empty() => {
                    Fact::from(r.tool == self.finished_tool)
                }
                _ => Fact::Unknown,
            },
            // One live session is being observed here; proving another one dead is `sweep`'s job.
            session_alive: Fact::Unknown,
        }
    }

    /// Carry out a plan, **stopping at the first effect that could not be carried out**.
    ///
    /// It used to skip what it could not do and continue, which is performing half a transition —
    /// the thing the plan-as-one-value design exists to prevent. Concretely: with no notifier,
    /// `Withdraw` was skipped and `Forget` still ran, deleting the only record that could ever
    /// bring that notification down. Stopping instead leaves the record intact for the next event
    /// to retry, and the screen-before-record ordering makes that the *only* way a plan can be
    /// left partly applied.
    ///
    /// Silent either way: a hook must not disturb the session, and there is nowhere for a
    /// complaint to go that is not the transcript. The returned effect is for tests and `plan`.
    fn perform(&self, effects: &[NotifEffect]) -> Result<(), NotifEffect> {
        for effect in effects {
            match effect {
                NotifEffect::Post => {
                    let bin = self.notifier.as_ref().ok_or(NotifEffect::Post)?;
                    let ok = Proc::new(bin)
                        .args(["post", "--id", &self.id, "--title", "Claude Code"])
                        .args(["--subtitle", &self.subtitle, "--body", &self.message])
                        .output()
                        .is_ok_and(|o| o.status.success());
                    if !ok {
                        return Err(NotifEffect::Post);
                    }
                }
                NotifEffect::Withdraw => {
                    let bin = self.notifier.as_ref().ok_or(NotifEffect::Withdraw)?;
                    let ok = Proc::new(bin)
                        .args(["remove", "--id", &self.id])
                        .output()
                        .is_ok_and(|o| o.status.success());
                    if !ok {
                        return Err(NotifEffect::Withdraw);
                    }
                }
                NotifEffect::Banner => banner(&self.message, &self.subtitle),
                NotifEffect::Remember => {
                    let tool = self.prompted_tool.clone().unwrap_or_default();
                    let owner = &self.owner;
                    // Beside the marker this request already holds — NOT `state_dir()` again.
                    // Re-reading the global here made the write and the path it was written for
                    // two different answers to one question, and made every test that exercised
                    // it mutate process-wide state while the others ran.
                    let dir = self.marker.parent().unwrap_or(std::path::Path::new("."));
                    std::fs::create_dir_all(dir).map_err(|_| NotifEffect::Remember)?;
                    std::fs::write(&self.marker, format!("{tool}\n{owner}\n"))
                        .map_err(|_| NotifEffect::Remember)?;
                }
                NotifEffect::Forget => match std::fs::remove_file(&self.marker) {
                    Ok(()) => {}
                    // Already gone is the outcome Forget wanted.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(NotifEffect::Forget),
                },
            }
        }
        Ok(())
    }
}

/// The process whose existence answers "is this session still alive?".
///
/// Refuses anything it cannot tell apart from this invocation's own short-lived ancestry, and
/// answers with an empty string when there is nothing trustworthy — which reads back as
/// [`Fact::Unknown`], so the sweep does nothing. Getting this wrong in the other direction takes
/// a live session's notification off the screen, so the default has to be to do nothing.
fn owner_id() -> String {
    owner_from(
        std::env::var("JKB_HOOK_OWNER").unwrap_or_default().trim(),
        std::os::unix::process::parent_id(),
        std::process::id(),
    )
}

/// The decision itself, with the two pids it must reject handed in — so the rule that carries the
/// whole must-fix can be tested, which it could not be while it read the environment.
fn owner_from(raw: &str, parent: u32, me: u32) -> String {
    let Ok(pid) = raw.parse::<u32>() else {
        return String::new();
    };
    // The shim is our parent and dies with this call; so does anything claiming to be us.
    if pid == 0 || pid == parent || pid == me {
        return String::new();
    }
    pid.to_string()
}

/// The Claude Code hook events this command answers to, and the ONE place they are spelled.
///
/// `SessionStart` maps to no machine event — it drives the sweep over records other sessions
/// left — but it belongs here because the question this table answers is "which registrations
/// must exist", and a name that drifts out of `.claude/settings.json` or the shim is silent in
/// the worst way: renaming the `SessionStart` literal alone disabled the sweep permanently with
/// every check still green. `jkb notify events` prints it so
/// `scripts/tests/notify-hook.test.sh` can diff all three spellings instead of two.
const HOOK_EVENTS: &[(&str, Option<NotifEvent>)] = &[
    ("Notification", Some(NotifEvent::Needed)),
    ("PostToolUse", Some(NotifEvent::ToolFinished)),
    ("UserPromptSubmit", Some(NotifEvent::UserActed)),
    ("Stop", Some(NotifEvent::TurnEnded)),
    ("SessionEnd", Some(NotifEvent::SessionEnded)),
    ("SessionStart", None),
];

/// The name that drives the sweep, taken from the table above so it cannot drift from it.
fn sweep_event_name() -> &'static str {
    HOOK_EVENTS
        .iter()
        .find(|(_, e)| e.is_none())
        .map_or("SessionStart", |(n, _)| n)
}

fn event_for(name: &str) -> Option<NotifEvent> {
    HOOK_EVENTS
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, e)| *e)
}

/// The plain `osascript` banner: visible, auto-hiding, impossible to withdraw. Newlines are
/// folded because `AppleScript` has no escape for one inside a string literal, so one would end
/// the statement mid-string.
fn banner(message: &str, subtitle: &str) {
    let _ = Proc::new("osascript")
        .args(["-e", &banner_script(message, subtitle)])
        .output();
}

/// The `AppleScript` a banner runs, as a value so it can be compiled in a test rather than only
/// fired and hoped for. The shell version was checked with `osacompile`; that coverage came back
/// here when the code did.
fn banner_script(message: &str, subtitle: &str) -> String {
    let quote = |s: &str| {
        let folded: String = s
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect();
        format!("\"{}\"", folded.replace('\\', "\\\\").replace('"', "\\\""))
    };
    format!(
        "display notification {} with title \"Claude Code\" subtitle {}",
        quote(message),
        quote(subtitle)
    )
}

/// Is the session that posted this still running? By owner-existence, never by age (D27).
///
/// `ps -p` rather than `kill -0`, which exits non-zero on `EPERM` for a live process owned by
/// somebody else and would therefore report a running session dead — the same trap `owner.rs`
/// documents. An unreadable or missing owner is `Unknown`, which refuses: leaving an orphan on
/// screen is better than withdrawing a live session's prompt.
fn session_alive(rec: &Record) -> Fact {
    if rec.owner.is_empty() || rec.owner.parse::<u32>().is_err() {
        return Fact::Unknown;
    }
    match Proc::new("ps").args(["-p", &rec.owner]).output() {
        Ok(out) => Fact::from(out.status.success()),
        Err(_) => Fact::Unknown,
    }
}

/// Withdraw notifications left behind by sessions that are provably gone.
///
/// This is the ONLY route by which a notification from a killed session ever comes down. Every
/// other event is scoped to a session id that will never occur again, so without this an
/// Alerts-style notification — which waits for ever by design — sits on screen naming a session
/// that no longer exists.
fn sweep() {
    sweep_in(&state_dir(), notifier_path().as_deref());
}

/// The sweep proper, over a stated directory and notifier — so it is drivable without touching
/// the environment, which is both testable and one less global read.
///
/// It goes through [`Machine::reconcile`] rather than calling the guard itself, so two conditions
/// that both applied would be **reported** rather than resolved by whichever arm ran first.
fn sweep_in(dir: &std::path::Path, notifier: Option<&std::path::Path>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let machine = machine();
    for entry in entries.flatten() {
        let marker = entry.path();
        let Some(rec) = Record::read(&marker) else {
            continue;
        };
        let Some(session) = marker.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let req = Request {
            event: NotifEvent::SessionGone,
            id: format!("jkb-claude-{session}"),
            marker: marker.clone(),
            prompted_tool: None,
            finished_tool: String::new(),
            message: String::new(),
            subtitle: String::new(),
            notifier: notifier.map(std::path::Path::to_path_buf),
            // The sweep never writes a record, only withdraws and forgets.
            owner: String::new(),
            recorded: None,
        };
        let ctx = NotifCtx {
            at: if rec.tool.is_empty() {
                NotifState::AwaitingUser
            } else {
                NotifState::AwaitingTool
            },
            notifier_usable: Fact::Unknown,
            tool_named: !rec.tool.is_empty(),
            tool_matches: Fact::Unknown,
            session_alive: session_alive(&rec),
        };
        if let Reconciliation::Fired(out) = machine.reconcile(&ctx) {
            let _ = req.perform(out.effects());
        }
    }
}

fn read_stdin() -> Result<String> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .context("reading the hook payload from stdin")?;
    Ok(raw)
}

/// Decide and print, performing nothing.
fn plan() -> Result<()> {
    let Some(req) = Request::parse(&read_stdin()?)? else {
        println!(
            "{}",
            serde_json::json!({ "effects": [], "reason": "not an event we act on" })
        );
        return Ok(());
    };
    let out = machine().apply(&req.ctx(), req.event);
    println!(
        "{}",
        serde_json::json!({
            "state": out.state().name(),
            "moved": out.moved(),
            "effects": effect_names(out.effects()),
            "tool": req.prompted_tool,
            "refusal": out.refusal(),
        })
    );
    Ok(())
}

/// Decide and carry it out. This is what the hook shim calls.
fn hook() -> Result<()> {
    let raw = read_stdin()?;
    // `SessionStart` is not an event of the machine — it drives the machine over every OTHER
    // session's record, which is a different question from "what should this session do now".
    if serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("hook_event_name")?.as_str().map(str::to_owned))
        .as_deref()
        == Some(sweep_event_name())
    {
        sweep();
        return Ok(());
    }
    let Some(req) = Request::parse(&raw)? else {
        return Ok(());
    };
    let _ = req.perform(machine().apply(&req.ctx(), req.event).effects());
    Ok(())
}

fn effect_names(effects: &[NotifEffect]) -> Vec<&'static str> {
    effects
        .iter()
        .map(|e| match e {
            NotifEffect::Post => "post",
            NotifEffect::Withdraw => "withdraw",
            NotifEffect::Banner => "banner",
            NotifEffect::Remember => "remember",
            NotifEffect::Forget => "forget",
        })
        .collect()
}
