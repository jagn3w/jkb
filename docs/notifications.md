# Permission notifications

Claude Code's "needs your permission" notification, made sticky and self-clearing. The pieces, in the
order an event passes through them:

1. the hook shim `.claude/hooks/notify-sticky.sh`, which hands the payload to
2. `jkb notify hook` (`crates/jkb-cli/src/notify.rs`), a client of `jkb serve` that never opens a
   database, which sends one `notify.event` to
3. the daemon, where the lifecycle table (`crates/jkb-core/src/notify.rs`, design
   `openspec/changes/jkb-notification-lifecycle/`) runs against its record of the session
   (`notify_sessions`) and sends posts and withdrawals on the `claude/notify` queue topic, consumed by
4. `jkb-notifier serve` (`macos/notifier/`, built and installed as the launchd agent
   `com.jkb.notifier` by `scripts/build-notifier.sh`), which displays them.

Steps 2–4 are design r3.2 N1/N2 (`openspec/changes/jkb-message-queue/design-r3.md`); the queue's own
rules are in [message-queue.md](message-queue.md). Read this before touching any of them.

## Where it runs, and why it moved (r3.2, 2026-09-14)

**The hook decides nothing and performs nothing any more.** Until r3.2 the hook ran the table itself
and exec'd the notifier per effect, with a marker file per session in its `$TMPDIR`. That works only
where the hook can reach the Mac's notification centre, and the dev container cannot — a hook in there
posted nothing at all. The fix is the substrate the message-queue change built: the hook tells the
daemon what it observed, the daemon decides against its own record, and a consumer on the Mac
displays. A hook in the container and one on the host now take the same path.

**The record and the sends are one transaction.** The old plan-ordering rule — screen effects before
record effects, and `perform` stops at the first it cannot carry out — existed because a subprocess
and a marker file could half-apply a plan. Inside one `write_txn` a plan applies whole or not at all:
a send the queue refuses leaves the record as it was. The ordering is kept (and still walked by
`plans_change_the_screen_before_the_record`) because it costs nothing.

**`Banner` and `notifier_usable` left the machine.** Whether a post can be *displayed* — installed,
authorized, alert style not `none` — is a fact about the Mac, which a producer in a container cannot
ask. `Needed` now always plans `[Post, Remember]`, and `jkb-notifier serve` decides per batch: through
the notification centre by id when usable, else a plain `osascript` banner. **Behaviour change,
stated:** the banner fallback used to need nothing installed; it now needs the notifier agent running,
because the agent is what reads the queue. With no agent, nothing is shown.

**The blind sweeps stayed, for a different reason.** `Stop`/`SessionEnd` from `absent` still send a
withdrawal. The record can no longer disagree with what was *sent*, but the screen is a consumer's,
across a queue: a consumer whose withdraw hit a notification centre that never answered has already
acked it (after 10 s — a deadline armed before the first call to the centre, so a centre that never
answers even the settings read cannot wedge the consumer). One withdrawal per turn bounds that to the
turn.

**How the consumer reads the stream.** `jkb-notifier serve` folds each burst to its net effect per
notification id and acks it only after display, so a crash redelivers rather than loses. A burst ends
at `caught_up` **or at `unreadable`**: `jkb mq subscribe` reports an unreadable row once and then sends
nothing more until that seq is acked, so waiting for `caught_up` there deadlocked on one corrupt row
(stage-5 review). A run of the subscription is over only when its process has exited **and** its
stdout has reached EOF, and the next starts only once that run's batch is off the screen — keyed on the
exit alone, a dead run's last post could land after the next run withdrew it. A failing subscription is
retried with backoff (1 s doubling to 30 s, reset only after a run that really started lasted a
minute), and only the first failure — with what the child wrote to stderr, or why it could not start —
and the recovery are logged, so `notifier.log` does not grow a line per retry for as long as a
failure lasts. Each run restarts once however many of its end signals arrive.

**A topic nobody reads is not written to.** A message no group consumes is never reapable, so a topic
with no groups fills to its 10,000-message cap and then refuses every hook call. On a machine with no
notifier that is the topic's whole life. So the machine still moves the record but sends only while
`claude/notify` has a group — and the only group is the notifier's (`macos-notifier`, created from
now by its `jkb mq subscribe`), so only a Mac running the agent receives anything. `scripts/setup.sh`
creates the topic on every platform, because a producer never creates one. On macOS it reports only
what it checked (`report_notifier` in `scripts/lib.sh`): the agent has a running process (a PID from
`launchctl list`, not merely "loaded", which a crash-looping agent also is) and the topic has a consumer
group — the question the daemon asks. That is deliberately not "notifications are shown": a group
outlives its consumer by 7 idle days, and a running notifier can have a failing subscription (its
`notifier.log` says). What it rules out is the two states setup once reported as healthy — an
authorized bundle with no agent, and an agent loaded but not running (both stage-5 reviews). A group
list that cannot be read is `undecided`, never "no group". `--no-service` leaves the agent off.

**The hook never opens a database, and has no local fallback.** It runs after every tool call, and
opening the database costs ~110 ms (design N7) — worse, a database a newer migration locked this
binary out of would stop every withdrawal. `tests/cli.rs` `notify_needs_no_database` pins it, now
with the daemon unreachable. So when `jkb serve` is down, notifications stop and the hook stays silent
(design R1's residual). Design N1 had the host hook fall back to opening the database locally; that
contradicted N7 and the pinned test, and was not built.

**Bounded and silent.** 200 ms to connect, 1 s per request (`RemoteBackend::with_deadlines`, pinned
against a daemon that accepts and never answers), nothing on stdout, and every failure appended to
`~/.jkb/logs/notify-hook.log`, which is moved aside to `.log.1` at 256 KiB so a daemon that stays down
cannot fill the disk. The address is `JKB_REMOTE` if set, else `JKB_DAEMON_ADDR` (the dev container
sets it to `host.docker.internal:7117`; `.container/check-config.sh` reads that variable's name out of
`remote.rs` and holds the value to the firewall's opening), else `jkb serve`'s default loopback. The
token is `~/.jkb/daemon/<port>/token`, **keyed by what a client knows** — the daemon's address, never
its database: beside the database, a host set up with `--db` wrote it where no client looked, and one
path per home let a second daemon on another port overwrite the first's live token. `jkb serve`
refuses its default token path inside the dev container (its image sets `JKB_NS_MARKER`) and on a
filesystem shared with another kernel, so a daemon started in there cannot replace the host daemon's
token through the `~/.jkb` bind — the filesystem check alone missed a native-Linux engine, whose bind
is plain ext4. Residual: a different container, with no marker, on a native-Linux bind. Port 0 has no
default token path (no client could derive it) and needs `--token-file`. The daemon-unreachable
marker is keyed by port too. The hook runs even where remote mode would refuse
(`JKB_REMOTE` beside `JKB_DB`), since it opens no database. **Only a failed connect marks the daemon
down** for the 5 s other clients skip it: a request that connected and then outran the hook's 1 s
reached a daemon busy on a write lock, and marking that down made the next permission prompt give up
untried. **Not yet measured:** the round trip from the container on the Mac.

**The sweep is the producer's, because only the producer can probe a pid.** At `SessionStart` the hook
asks `notify.open_sessions`, decides for each record whether its session is provably gone, and sends
`notify.gone` for those. A record is gone when (a) it was written from **this instance** and its owner
pid is dead by `kill(pid, 0)` (`EPERM` counts as alive), or (b) it was written from **another boot of
this same container**. The instance is `host[#boot][/pidns]`: the hostname; in the container, the
boot — the pid namespace the entrypoint recorded in `JKB_NS_MARKER`; and the pid namespace the
writing process is actually in. A container keeps its hostname across `docker stop`/`start` but writes
a new marker, and runs one boot at a time, so a different boot on the same host is gone. **The
process's own namespace is what makes rule (a) sound:** a nested sandbox shares the hostname and the
marker but not the pid namespace (measured in the Bash sandbox: `pid:[4026532823]` against the
marker's `pid:[4026532556]`), so without it a `claude` started in a sandbox would have probed the
outer sessions' pids where they do not exist and withdrawn every live prompt in the container — the
stage-5 review's finding. Everything else — the host seen from a container, another container, a
nested sandbox of this boot, a record with no owner — is `Unknown`, and `Unknown` never withdraws. The
sweep starts no request once 1 s has passed, so `SessionStart` is bounded by about 2 s.

**`notify.gone` carries the owner and instance it judged**, and the daemon withdraws only if the
record still names both. `claude --resume` keeps the session id and runs a new process; a session
resumed between the sweep's read and its withdrawal would otherwise lose a live prompt. The owner
alone was not enough: after a container restart a resumed session can draw the same pid in the new
boot. Pinned by `the_sweep_spares_a_session_resumed_after_it_looked` and
`gone_withdraws_only_the_owner_that_was_probed`.

**Residuals, stated:** a *rebuilt* container gets a new hostname, so notifications its sessions left
cannot be proved gone from anywhere and stay until dismissed by hand (as before r3.2, whose markers
died with the container). Two containers given the same `--hostname` would read as each other's
earlier boot. Any process that can reach the port and read the token can post and withdraw
notifications (design R1).

## The session registry (tasks S6.4, 2026-09-16)

The hook also feeds a registry of Claude Code sessions (`claude_sessions`, V019;
`jkb_core::claude_session`), so that the session verbs of S6.4 can ask whether the session holding a
claim or a lock has ended (`openspec/changes/jkb-message-queue/design-s6-4.md`). The hook sends:

- `session.started` on `SessionStart`;
- `session.ended` on `SessionEnd`, after that event's `notify.event`.

Every `notify.event` except the end's also marks its process running. The `SessionStart` sweep, after
the notification records, lists the live rows (`session.list`) and sends `session.gone` for those its
[verdict](#where-it-runs-and-why-it-moved-r32-2026-09-14) proves gone. It is the same function, asked of
the row's `pid` and `instance`. `jkb notify sessions [--all]` prints the registry.

**What the design rests on was measured, not read** (2026-09-16, the dev container, a logging hook
recording each payload while the user ended sessions by hand):

| action | events | session id |
|---|---|---|
| launch, no prompt sent | `SessionStart` `startup` | new |
| `/exit` | `SessionEnd` `prompt_input_exit` | — |
| `/clear` | `SessionEnd` `clear`, then `SessionStart` `clear`, same second | **new** |
| `claude --resume` | `SessionStart` `resume` | kept |
| `/resume` in a session | `SessionEnd` `resume` for the one left, `SessionStart` `resume` for the one entered | kept |
| compaction | `SessionStart` `compact`, no end | kept |
| closing the terminal tab | `SessionEnd` `other` | — |
| `kill -9` of `claude` | nothing | — |
| `docker restart` | nothing | — |

Other measured facts:

- `$CLAUDE_CODE_SESSION_ID` equalled the payload's `session_id` in every event.
- Hooks edited into settings took effect in a running session.
- Hooks in the project's `.claude/settings.json` did not run for a session started outside the
  repository. That first looked like a killed session that had never started.
- A first attempt to log the `claude` pid from `sh -c '…$PPID…'` recorded a different pid for each
  event. That `$PPID` is the shell Claude Code wraps a hook command in, not `claude`. The shim's
  single-command form, `bash <script>`, is what makes its `$PPID` `claude` (above).

Not measured: whether Claude Code lets two processes hold one session id at once. The design assumes
it can.

**A row is a process holding a session**, keyed by (session, pid, instance). A session is **live**
while any of its rows is live, **ended** once every row has ended, and **unknown** with no rows.

- **Only an ended session is evidence.** A killed process stays live until the next `SessionStart` in
  its instance sweeps it, and one on a rebuilt container never can be. Unknown licenses nothing.
- **Ended is not final.** A `resume` or any later event from the process makes its row live again.
- **One process's end or death ends only its own row**, because `claude --resume` can run an id in a
  second process while the first still runs it. The first version kept one row per session, and the
  stage-1 review showed that the later process's exit then recorded as ended a session the earlier one
  was still running. `session.gone` also ends only a live row that still names exactly the pid and
  instance that were probed. That is `notify.gone`'s rule, for the same race.
- A verdict with no pid proves nothing. A pid-less process's own end still ends its own row, since it
  is a report rather than a probe.

**Four repairs came from the stage-1 review, each pinned by a test:**

- **A lost start is repaired by the next event.** If `session.started` is lost (the daemon is busy or
  restarting), a resumed session would stay recorded as ended while it ran, because nothing else makes
  a row live. So every `notify.event` from a process makes its row live. The one exception is the end's
  own `notify.event`, which must not revive what `session.ended` is about to end. A live row's
  `seen_at` is rewritten at most hourly, so a tool call on a known session writes nothing. Pinned by
  `a_notify_event_marks_its_process_running_except_at_the_end` and
  `any_event_from_a_process_makes_it_live`.
- **The starting session is never judged by its own sweep.** Suppose it was killed and is now being
  resumed. Its earlier process is provably dead, but ending that row would record a running session
  as ended if this start's `session.started` had been lost. A later sweep, from another session, ends
  the row. Pinned by `the_sweep_never_judges_the_session_that_is_starting`.
- **The macOS host's name may change.** Its instance is the hostname alone, and macOS renames the host
  on a network change. Two *bare* instances (no boot, no namespace) are therefore taken to be the same
  machine, and the pid is probed. **The assumption, stated:** every bare instance is the machine
  `jkb serve` runs on, because its clients are that host and its containers, and a container always
  records a boot and a namespace. A second bare machine reaching the daemon would break it. Pinned in
  `only_a_dead_pid_here_or_an_earlier_boot_of_this_container_is_gone`.
- **What is only shown is normalised, not refused.** An unusual start source or end reason is recorded
  as `unknown`, and a long working directory is cut, because a refused start is exactly the lost write
  above. Identity (session, pid, instance) is still refused when malformed, with the notification ops'
  own checks, shared rather than copied.

**Budgets.** `SessionEnd` hooks get 1.5 s (Claude Code's documentation), and the end sends two requests.
The second starts only within 300 ms of the hook's first line (`SESSION_END_SECOND_REQUEST`), because it
may itself take the full 1 s request deadline. That leaves room for the shim's and the binary's
start-up. A warm round trip takes a few milliseconds. What is not sent is logged, and a later sweep
proves the same end from the pid; a hook killed at the budget would have logged nothing.
`SessionStart` starts nothing 1 s after the hook began, so a start plus both sweeps is bounded by about
2 s. Pinned by `a_slow_first_request_leaves_the_rest_unsent`.

**Pruning.** A session starting deletes rows not seen for 90 days, except its own. Deleting a row makes
that process unknown, which licenses nothing. That is the safe direction for a record nobody can prove
anything about any more, such as a rebuilt container's sessions.

**Not changelogged**, like `notify_sessions`: it is observation, and `jkb undo` must not revive or
end a session.

**Residuals, stated:**

- **The daemon cannot tell a host client from a container client**, so a misbehaving container could
  report a host session's process ended. An honest one never does, because its verdict about a host row
  is `Unknown`. That grants nothing a container could not already do with `task.release` and a claim's
  owner string.
- **Two containers given the same `--hostname`** read as each other's earlier boot, and each one's
  sweep ends the other's live rows. For notifications that cost a withdrawn prompt; here it records
  running sessions as ended until their next hook event revives them. Stage 2 must therefore confirm
  "ended" against something it can observe, the worktree above all, before it takes anything over.

## The hook, the table and the notifier

**The `.claude/` hooks are code, and `cargo test` never reaches them** — `scripts/tests/notify-hook.test.sh`
is their suite, run first by `check.sh` and by CI (bash + `jq` only, so it passes on the Linux
runner). It also checks the launchd agent `build-notifier.sh --print-agent` writes, and — on macOS,
where `swiftc` exists — compiles `macos/notifier/main.swift` and drives `serve --dry-run` (the fold,
stale marking and acks) and the fallback banner's AppleScript through `osacompile`. **Portable
means portable:** every macOS-only tool it touches is `command -v`-guarded, and the plist check
goes through `scripts/build-notifier.sh --check` — which answers *before* its own Darwin gate and
reads the plist with `awk` — so the assertion is live on the Linux runner rather than skipped
there. It once called `/usr/libexec/PlistBuddy` directly, whose `|| echo MISSING` fallback turned
*cannot read* into a wrong **value** and reddened CI on every push. The live
post-then-withdraw round-trip — through the hook, the running `jkb serve` and the running
`com.jkb.notifier` agent to the real notification centre — is opt-in behind `JKB_HOOK_LIVE_TEST=1`, for
the same reason the ollama and Chrome smokes are `#[ignore]`d — it puts a real notification on
screen, and a gate that runs before every commit must not flash one.

**Permission notifications are sticky and self-clearing.** A banner that hides after a few
seconds is exactly wrong for "Claude needs your permission": the session sits blocked until you
happen to look. The table posts on `Notification` under an id derived from `session_id` and
withdraws that id on `PostToolUse`/`UserPromptSubmit`/`Stop`/`SessionEnd` — **the
id comes from the session, so dismissing owns no pid and no window handle**, and parallel worktree
sessions (D36) cannot clear each other's. `PostToolUse` is the closest observable "permission was
given" (`PreToolUse` runs *before* the prompt), so granting a slow command clears on completion,
not on the click.

**The dismiss events are not interchangeable, and treating them as one set was a defect.** One
assistant message routinely batches several tool calls, so a slow allowlisted one finishes while
another's permission prompt is still on screen — and a `PostToolUse` that withdrew on *any* tool
left the session blocked with nothing on screen, which is the state the hook exists to prevent.
So the events are split by how far each can be trusted: a **tool** event withdraws only when the
finished tool is the one the prompt named (read out of the notification message, since the payload
names no tool); a **user** event withdraws unconditionally; and `Stop`/`SessionEnd` **sweep without
consulting the record at all**. The sweep is what makes every failure above temporary — before r3.2
including a marker file that could never be written, and since then a consumer whose withdrawal did
not reach the screen — either of which would otherwise leave an `alert`-style notification, which
waits forever by design, on screen after the session ends.
Residual, stated rather than guarded: two calls to the *same* tool, one allowlisted and one
prompting, are indistinguishable, so the first to finish withdraws; the sweep bounds it to the
turn.

**The per-session marker file is superseded by the daemon's `notify_sessions` record** (r3.2). The
marker (`$TMPDIR/jkb-claude-notify/<session>`) held the prompted tool's name, and existed because
`PostToolUse` fires after *every* tool call and that path had to be a stat rather than an exec. It
is now one HTTP round trip instead, and the record lives beside the queue it is transacted with. The
old marker could also be orphaned by a container restart, taking the only memory of a notification
with it; the record cannot. The retry for a failed withdraw is still the turn-end sweep, bounded by
construction.

**The notifier is ours (`macos/notifier/`), and the withdraw half is what forced that.** Nothing
shipping on macOS both stays up and takes itself down: `osascript` cannot withdraw what it posted,
and `terminal-notifier` can but was last released in **2017** on `NSUserNotification`, deprecated
since macOS 11. So ~200 lines of Swift call Apple's current `UserNotifications` framework
directly, for `removeDeliveredNotifications(withIdentifiers:)`. **Swift, not Rust, to avoid a
dependency rather than add one** — the Rust route is `objc2`, where every message send is
`unsafe`, and this workspace's one `unsafe_code` carve-out is spoken for. It links nothing but
system frameworks. Rejected: the `user-notify` crate (one maintainer, wraps the same `unsafe`,
and its README asserts an Apple developer account is required — which we measured to be false).

**Three facts about it were measured, not assumed, and each one silently breaks it:**
`UNUserNotificationCenter.current()` refuses a process whose bundle has no identifier, so the
binary must live in an `.app`; the bundle must be **ad-hoc signed** (`codesign -s -` — no Apple
developer account, which only costs distribution) and **registered with Launch Services**, or the
framework answers "Notifications are not allowed for this application" however it is signed; and
the authorization prompt **dies with the process that raised it**, so `authorize` waits 5 minutes
— a prompt that vanishes is recorded as a *denial* only System Settings can undo.
`scripts/build-notifier.sh` does all four steps; `scripts/setup.sh` runs it.

**Two things no installer can do, so both are reported instead:** the one-time Allow, and the
sticky **Alerts** style (a per-app System Settings choice; `banner` hides itself). `jkb-notifier
status` reads both back — which terminal-notifier could not — so setup says which is missing.
`post` **refuses** when unauthorized rather than succeeding invisibly (macOS accepts it, displays
nothing and returns no error); `serve` asks the same question per batch and falls back to a plain
`osascript` banner, with the escaping the Rust hook's `banner_script` had — which matters more now,
because the text can come from inside the dev container. `NSUserNotificationAlertStyle` in `Info.plist` is
deliberately absent: it is the legacy key, ignored by the modern framework — also measured.

**Do not try to set the alert style in code** — this was attempted and abandoned on purpose. It
lives in `com.apple.ncprefs`, which on macOS 26 is TCC-protected (unreadable without Full Disk
Access) and stores the style as an undocumented bitfield in a file shared by **every** app's
notification settings, so a wrong write breaks notifications system-wide to save one click. The
supported path is the System Settings pane, which `open
"x-apple.systempreferences:com.apple.Notifications-Settings.extension"` goes straight to.

**Focus suppresses these notifications, and that is not fixable here.** `.timeSensitive` is
requested but `status` reports `time-sensitive=not-supported`: the capability comes from
`com.apple.developer.usernotifications.time-sensitive`, an ad-hoc bundle carries no entitlements,
and the system **downgrades silently rather than erroring**. Embedding the entitlement anyway was
measured and is strictly worse — an ad-hoc signature carrying a restricted entitlement is rejected
and the binary is **SIGKILLed on launch**, so the notifier stops working altogether. Honouring it
needs a real signing identity. The request is kept (it costs nothing and starts working the day
this is signed), and the claim lives in `status` where it is read back, never in a comment
asserting a capability we do not have.

**Stickiness cannot be asserted, only the style can.** `jkb-notifier list` reports *delivered*
notifications, which includes one whose banner has hidden and is resting in Notification Center —
so no test can tell "still visible" from "already hidden", because no API exposes it. The live
test asserts the round-trip and reads the style back; that it stays on screen was confirmed by
looking. Worth knowing before writing a test that appears to prove more than it does.

