# Permission notifications

Claude Code's "needs your permission" notification, made sticky and self-clearing: the hook
(`.claude/hooks/notify-sticky.sh`), the lifecycle table that decides post/withdraw
(`crates/jkb-cli/src/notify.rs`, design `openspec/changes/jkb-notification-lifecycle/`), and the
macOS notifier we build for the withdraw half (`macos/notifier/`, `scripts/build-notifier.sh`).
Read this before touching any of them.

**The `.claude/` hooks are code, and `cargo test` never reaches them** — `scripts/tests/notify-hook.test.sh`
is their suite, run first by `check.sh` and by CI (bash + `jq` only, so it passes on the Linux
runner). `notify-sticky.sh` takes its notifier and state directory from `JKB_NOTIFIER` /
`JKB_NOTIFY_STATE`, which is what lets the macOS half be driven by a recorder stub. **Portable
means portable:** every macOS-only tool it touches is `command -v`-guarded, and the plist check
goes through `scripts/build-notifier.sh --check` — which answers *before* its own Darwin gate and
reads the plist with `awk` — so the assertion is live on the Linux runner rather than skipped
there. It once called `/usr/libexec/PlistBuddy` directly, whose `|| echo MISSING` fallback turned
*cannot read* into a wrong **value** and reddened CI on every push. The live
post-then-withdraw round-trip against the real bundle is opt-in behind `JKB_HOOK_LIVE_TEST=1`, for
the same reason the ollama and Chrome smokes are `#[ignore]`d — it puts a real notification on
screen, and a gate that runs before every commit must not flash one.

**Permission notifications are sticky and self-clearing.** A banner that hides after a few
seconds is exactly wrong for "Claude needs your permission": the session sits blocked until you
happen to look. `.claude/hooks/notify-sticky.sh` posts on `Notification` under an id derived from
`session_id` and withdraws that id on `PostToolUse`/`UserPromptSubmit`/`Stop`/`SessionEnd` — **the
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
consulting the marker at all**. The sweep is what makes every failure above temporary — including
a marker that could never be written, which otherwise short-circuits every gated event and leaves
an `alert`-style notification, which waits forever by design, on screen after the session ends.
Residual, stated rather than guarded: two calls to the *same* tool, one allowlisted and one
prompting, are indistinguishable, so the first to finish withdraws; the sweep bounds it to the
turn.

**The one file it keeps is a per-session marker** (`$TMPDIR/jkb-claude-notify/<session>`, or
`JKB_NOTIFY_STATE`), holding the prompted tool's name. It exists because `PostToolUse` fires after
*every* tool call and that path must be a stat rather than an exec. A failed withdraw does **not**
re-arm it: the only realistic non-zero exit is a 10s timeout against a wedged notification centre,
so retrying per tool call would add that to each one, with the reason swallowed. The retry is the
sweep, bounded by construction.

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
nothing and returns no error), and the hook falls back to a plain `osascript` banner, so behaviour
is never worse than before this existed. `NSUserNotificationAlertStyle` in `Info.plist` is
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

