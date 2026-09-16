// jkb-notifier — post and withdraw macOS notifications by identifier, and serve them off jkb's queue.
//
// This exists because a notification you must ACT on has to stay on screen and then go away by
// itself once you have acted, and nothing shipping on macOS does both. `osascript display
// notification` cannot withdraw what it posted. terminal-notifier can, but its last release was
// 2017 and it is built on NSUserNotification, deprecated since macOS 11.
//
// So it is written against `UserNotifications` directly — Apple's current framework, and the one
// with `removeDeliveredNotifications(withIdentifiers:)`, which is the whole feature. It is Swift
// rather than Rust because that removes a dependency instead of adding one: the Rust route needs
// `objc2`, where every message send is `unsafe`, and this workspace denies `unsafe_code` with a
// single carve-out that is spoken for. Nothing here links anything but the system frameworks.
//
// **It must live inside an .app bundle.** `UNUserNotificationCenter.current()` refuses a process
// whose main bundle has no identifier, which is why `scripts/build-notifier.sh` wraps this binary
// in one, ad-hoc signs it and registers it with Launch Services. An Apple developer account is
// NOT required — ad-hoc is enough to be granted authorization, which was measured rather than
// assumed. What ad-hoc signing costs is distribution, which we do not do.
//
// **`serve` is the consumer of the `claude/notify` queue** (design r3.2 N2,
// openspec/changes/jkb-message-queue/design-r3.md). The Claude Code hook no longer runs this
// binary: it tells `jkb serve` what happened, jkb's notification machine decides, and its posts and
// withdrawals arrive here through `jkb mq subscribe` — the NDJSON protocol in docs/message-queue.md.
// Run by launchd as `com.jkb.notifier`, pointing at the BUNDLE's binary so the notification centre
// finds the bundle identifier. Deciding whether a post can be *displayed* is this process's job,
// because it is the only one that can ask: authorized and not alert-style `none` posts through the
// notification centre by id; anything else falls back to a plain `osascript` banner.
//
// Usage:
//   jkb-notifier post --id <id> --title <t> [--subtitle <s>] [--body <b>]
//   jkb-notifier remove --id <id>
//   jkb-notifier list
//   jkb-notifier status
//   jkb-notifier authorize
//   jkb-notifier serve --jkb <path> --topic <topic> [--db <path>] [--group <name>]
//   jkb-notifier serve --dry-run [--now-ms <ms>]   (NDJSON events on stdin; actions and acks on stdout)
//   jkb-notifier banner-script --title <t> --subtitle <s> --body <b>   (print the osascript fallback)
//
// Exit codes: 0 ok · 1 usage · 2 not authorized · 3 timed out talking to the notification centre.

import Foundation
import UserNotifications

let EXIT_OK: Int32 = 0
let EXIT_USAGE: Int32 = 1
let EXIT_UNAUTHORIZED: Int32 = 2
let EXIT_TIMEOUT: Int32 = 3

func fail(_ message: String, _ code: Int32) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(code)
}

func log(_ message: String) {
    FileHandle.standardError.write(("jkb-notifier: " + message + "\n").data(using: .utf8)!)
}

// MARK: - arguments

/// `--flag value` pairs after the verb, plus the bare `--dry-run` switch. Deliberately not a general
/// parser: its callers are a launchd plist written by `scripts/build-notifier.sh` and the tests.
func parseFlags(_ args: ArraySlice<String>) -> [String: String] {
    var out: [String: String] = [:]
    var rest = Array(args)
    while !rest.isEmpty, rest[0].hasPrefix("--") {
        if rest[0] == "--dry-run" {
            out["dry-run"] = "1"
            rest.removeFirst()
            continue
        }
        guard rest.count >= 2 else { break }
        out[String(rest[0].dropFirst(2))] = rest[1]
        rest.removeFirst(2)
    }
    return out
}

let argv = CommandLine.arguments
guard argv.count > 1 else {
    fail("usage: jkb-notifier post|remove|list|status|authorize|serve [--flag value ...]", EXIT_USAGE)
}
let verb = argv[1]
let flags = parseFlags(argv.dropFirst(2))

/// Asked for only when used. `serve --dry-run` must run with no bundle at all — it is how the fold
/// is tested — and `UNUserNotificationCenter.current()` refuses a process without one.
var center: UNUserNotificationCenter { UNUserNotificationCenter.current() }

// MARK: - reporting

func name(_ status: UNAuthorizationStatus) -> String {
    switch status {
    case .notDetermined: return "not-determined"
    case .denied: return "denied"
    case .authorized: return "authorized"
    case .provisional: return "provisional"
    case .ephemeral: return "ephemeral"
    @unknown default: return "unknown"
    }
}

/// `banner` hides itself after a few seconds; `alert` waits for the user. Which one you get is a
/// per-app setting the user owns and no API can set — so the most an installer can do is REPORT
/// it, which is the reason this is readable at all. A hook that silently posts self-hiding
/// banners looks exactly like a broken hook.
func name(_ style: UNAlertStyle) -> String {
    switch style {
    case .none: return "none"
    case .banner: return "banner"
    case .alert: return "alert"
    @unknown default: return "unknown"
    }
}

// MARK: - verbs

func withSettings(_ body: @escaping (UNNotificationSettings) -> Void) {
    center.getNotificationSettings { body($0) }
}

func authorized(_ s: UNNotificationSettings) -> Bool {
    switch s.authorizationStatus {
    case .authorized, .provisional, .ephemeral: return true
    default: return false
    }
}

/// Whether the system will actually honour `.timeSensitive` for this bundle. Reported because it
/// cannot be assumed: the capability is normally granted by an entitlement, and this bundle is
/// ad-hoc signed with none, so the honest thing is to read back what the system decided rather
/// than to claim Focus is covered.
func timeSensitive(_ s: UNNotificationSettings) -> String {
    if #available(macOS 12.0, *) {
        switch s.timeSensitiveSetting {
        case .enabled: return "enabled"
        case .disabled: return "disabled"
        case .notSupported: return "not-supported"
        @unknown default: return "unknown"
        }
    }
    return "unavailable"
}

func doStatus() {
    withSettings { s in
        print(
            "authorization=\(name(s.authorizationStatus)) alert-style=\(name(s.alertStyle)) "
                + "time-sensitive=\(timeSensitive(s))")
        exit(EXIT_OK)
    }
}

/// Asks, and then WAITS. The prompt is owned by this process and dies with it: an `authorize`
/// that returned promptly would take its own dialog off the screen before the user reached it,
/// and macOS records the un-answered prompt as a denial that only System Settings can undo.
func doAuthorize() {
    center.requestAuthorization(options: [.alert, .sound]) { granted, err in
        if let err = err {
            fail("authorization failed: \(err.localizedDescription)", EXIT_UNAUTHORIZED)
        }
        if !granted {
            fail("notifications were not allowed", EXIT_UNAUTHORIZED)
        }
        withSettings { s in
            print("authorization=\(name(s.authorizationStatus)) alert-style=\(name(s.alertStyle))")
            exit(EXIT_OK)
        }
    }
}

/// The notification request for an id. Reusing an identifier REPLACES the delivered notification
/// rather than stacking a second one, which is what makes a per-session id the whole bookkeeping.
func request(id: String, title: String, subtitle: String, body: String) -> UNNotificationRequest {
    let content = UNMutableNotificationContent()
    content.title = title
    if !subtitle.isEmpty { content.subtitle = subtitle }
    if !body.isEmpty { content.body = body }
    // Requested, but DO NOT read this as "Focus is handled" — on an ad-hoc signed bundle it is
    // not. `status` reports `time-sensitive=not-supported` here, because the capability comes
    // from `com.apple.developer.usernotifications.time-sensitive` and this bundle carries no
    // entitlements. The system does not error; it silently delivers at the ordinary level, so
    // a Focus mode WILL suppress the very prompts this exists to surface.
    //
    // Embedding the entitlement anyway was measured and is worse: an ad-hoc signature carrying
    // a restricted entitlement is rejected outright and the binary is SIGKILLed on launch, so
    // the notifier stops working entirely. Honouring it needs a real signing identity.
    //
    // Kept set regardless: it costs nothing, it is the correct request, and it starts working
    // the day this is signed properly. The claim lives in `status` where it can be read back,
    // not in a comment asserting a capability we do not have.
    content.interruptionLevel = .timeSensitive
    return UNNotificationRequest(identifier: id, content: content, trigger: nil)
}

func doPost() {
    guard let id = flags["id"], !id.isEmpty else { fail("post: --id is required", EXIT_USAGE) }
    guard let title = flags["title"], !title.isEmpty else {
        fail("post: --title is required", EXIT_USAGE)
    }

    withSettings { s in
        // Refuse rather than post into the void. An unauthorized `add` returns no error and
        // displays nothing, so posting anyway would report success for a notification the user
        // will never see — and the caller could not tell that from a working one.
        if !authorized(s) {
            fail(
                "not authorized to post (\(name(s.authorizationStatus))) — run: jkb-notifier authorize",
                EXIT_UNAUTHORIZED)
        }
        let req = request(
            id: id, title: title, subtitle: flags["subtitle"] ?? "", body: flags["body"] ?? "")
        center.add(req) { err in
            if let err = err { fail("post failed: \(err.localizedDescription)", EXIT_UNAUTHORIZED) }
            exit(EXIT_OK)
        }
    }
}

/// The half that does not exist anywhere else. Withdrawing is silent about whether anything was
/// there to withdraw, and that is the right shape for the caller: "make sure nothing of mine is
/// on screen" needs no answer, and a session that posted nothing must not be an error.
func doRemove() {
    guard let id = flags["id"], !id.isEmpty else { fail("remove: --id is required", EXIT_USAGE) }
    center.removeDeliveredNotifications(withIdentifiers: [id])
    // `removeDeliveredNotifications` is fire-and-forget with no completion handler, so give the
    // XPC call a moment to leave the process before exiting out from under it.
    center.getDeliveredNotifications { _ in exit(EXIT_OK) }
}

/// Delivered notification ids, one per line. A diagnostic, and the only way a test can tell a
/// `remove` that worked from one that merely exited 0 — which matters here, because withdrawing
/// is the single behaviour that justifies this program existing.
func doList() {
    center.getDeliveredNotifications { list in
        for n in list { print(n.request.identifier) }
        exit(EXIT_OK)
    }
}

// MARK: - serve: the queue consumer

/// A post as the queue carries it (`jkb_core::notify`'s `notify.post` payload).
struct Note: Equatable {
    let id: String
    let title: String
    let subtitle: String
    let body: String
    let enqueuedAt: Int64
    let expired: Bool
}

enum Action: Equatable {
    case post(Note)
    case remove(String)
}

func int64(_ v: Any?) -> Int64? { (v as? NSNumber)?.int64Value }

/// One `message` event's message, read into an action — or nil for a kind this consumer does not
/// handle, which it skips (consumers must ignore what they do not know).
func action(_ message: [String: Any]) -> Action? {
    guard let kind = message["kind"] as? String,
        let payload = message["payload"] as? [String: Any],
        let id = payload["id"] as? String, !id.isEmpty
    else { return nil }
    switch kind {
    case "notify.post":
        return .post(
            Note(
                id: id,
                title: payload["title"] as? String ?? "Claude Code",
                subtitle: payload["subtitle"] as? String ?? "",
                body: payload["body"] as? String ?? "",
                enqueuedAt: int64(message["enqueued_at"]) ?? 0,
                expired: message["expired"] as? Bool ?? false))
    case "notify.withdraw":
        return .remove(id)
    default:
        return nil
    }
}

/// **Fold a batch to its net effect per notification id** before touching the notification centre,
/// so a consumer that was down does not flash every post and withdrawal it missed: the last event
/// for an id wins, and ids keep the order they first appeared in.
func fold(_ messages: [[String: Any]]) -> [Action] {
    var order: [String] = []
    var last: [String: Action] = [:]
    for m in messages {
        guard let a = action(m) else { continue }
        let id: String
        switch a {
        case .post(let n): id = n.id
        case .remove(let i): id = i
        }
        if last[id] == nil { order.append(id) }
        last[id] = a
    }
    return order.compactMap { last[$0] }
}

/// "35m", "14h", "3d".
func age(_ ms: Int64) -> String {
    let minutes = max(Int64(0), ms) / 60_000
    if minutes < 60 { return "\(minutes)m" }
    let hours = minutes / 60
    if hours < 48 { return "\(hours)h" }
    return "\(hours / 24)d"
}

/// An expired post is still shown — decided by the user, 2026-09-13 — but marked stale with its
/// age, so a prompt from last night is not mistaken for one from now. The `SessionStart` sweep
/// withdraws it if its session is gone.
func subtitle(_ n: Note, now: Int64) -> String {
    guard n.expired else { return n.subtitle }
    let mark = "stale, \(age(now - n.enqueuedAt)) ago"
    return n.subtitle.isEmpty ? mark : "\(n.subtitle) · \(mark)"
}

/// An AppleScript string literal. Line breaks are folded to spaces — AppleScript has no escape for
/// a newline inside `"…"` — and backslash and quote escaped: the same rule as `banner_script` had in
/// the Rust hook, and it matters more now, because this text can come from inside the dev container.
func appleScriptString(_ s: String) -> String {
    let folded = s.replacingOccurrences(of: "\r", with: " ").replacingOccurrences(of: "\n", with: " ")
    let escaped = folded.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(
        of: "\"", with: "\\\"")
    return "\"\(escaped)\""
}

func bannerScript(title: String, subtitle: String, body: String) -> String {
    "display notification \(appleScriptString(body)) with title \(appleScriptString(title)) "
        + "subtitle \(appleScriptString(subtitle))"
}

let nowMs: () -> Int64 = { Int64(Date().timeIntervalSince1970 * 1000) }

/// Where a folded batch goes. `done` must be called exactly once, when every action has been
/// handed over, and only then is the batch acked.
protocol Display {
    func apply(_ actions: [Action], done: @escaping () -> Void)
}

/// Prints what it would do: `serve --dry-run`, for tests.
struct PrintDisplay: Display {
    let now: Int64
    func apply(_ actions: [Action], done: @escaping () -> Void) {
        for a in actions {
            switch a {
            case .post(let n):
                print("post \(n.id) \(n.title) | \(subtitle(n, now: now)) | \(n.body)")
            case .remove(let id):
                print("remove \(id)")
            }
        }
        done()
    }
}

/// The notification centre, or a plain banner where it cannot display anything.
struct CenterDisplay: Display {
    func apply(_ actions: [Action], done: @escaping () -> Void) {
        guard !actions.isEmpty else {
            done()
            return
        }
        // A notification centre that never answers must not wedge the consumer, so the deadline is
        // armed BEFORE the first call to it — the settings read included, which is where a hung
        // usernoted stops answering first. (It was armed inside that call's completion, so the one
        // case it existed for never reached it. Stage-5 review.) After the window the batch is acked
        // anyway: redelivering it would wedge the same way, and the blind withdrawal at every turn's
        // end bounds a lost one to the turn.
        let finished = Once(done)
        DispatchQueue.main.asyncAfter(deadline: .now() + 10) {
            if finished.run() { log("the notification centre did not answer within 10s") }
        }
        withSettings { s in
            // The rule the hook used to apply before it posted: macOS accepts an unauthorized post,
            // and one to an app whose style is `none`, without error and shows nothing.
            let usable = authorized(s) && s.alertStyle != .none
            let group = DispatchGroup()
            let now = nowMs()
            for a in actions {
                switch a {
                case .post(let n):
                    if usable {
                        group.enter()
                        let req = request(
                            id: n.id, title: n.title, subtitle: subtitle(n, now: now), body: n.body)
                        center.add(req) { err in
                            if let err = err { log("post \(n.id) failed: \(err.localizedDescription)") }
                            group.leave()
                        }
                    } else {
                        group.enter()
                        // The script is built here, on the main queue, and only the finished string
                        // crosses to the background one: Swift 6 flags any global function run from a
                        // concurrently-executed closure (`bannerScript` warned under `-swift-version 5`).
                        let script = bannerScript(
                            title: n.title, subtitle: subtitle(n, now: now), body: n.body)
                        DispatchQueue.global().async {
                            runOsascript(script)
                            group.leave()
                        }
                    }
                case .remove(let id):
                    center.removeDeliveredNotifications(withIdentifiers: [id])
                }
            }
            // `removeDeliveredNotifications` has no completion handler; a round trip after it is
            // the moment it has left the process.
            group.enter()
            center.getDeliveredNotifications { _ in group.leave() }
            group.notify(queue: .main) { finished.run() }
        }
    }
}

/// Runs its closure the first time only; `run` says whether this call was that time.
final class Once {
    private var body: (() -> Void)?
    init(_ body: @escaping () -> Void) { self.body = body }
    @discardableResult func run() -> Bool {
        guard let b = body else { return false }
        body = nil
        b()
        return true
    }
}

/// Show a plain banner by running `script` (from `bannerScript`) with `osascript`, and wait for it.
func runOsascript(_ script: String) {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
    p.arguments = ["-e", script]
    do {
        try p.run()
        p.waitUntilExit()
    } catch {
        log("osascript: \(error.localizedDescription)")
    }
}

/// Reads the subscription's NDJSON events, folds each burst at its `caught_up` boundary, displays
/// it, and acks it. Everything runs on the main queue.
final class Consumer {
    let display: Display
    let ack: (Int64) -> Void
    private var buffer = Data()
    private var pending: [[String: Any]] = []
    private var pendingSeq: Int64?
    private var caughtUp = false
    private var busy = false

    init(display: Display, ack: @escaping (Int64) -> Void) {
        self.display = display
        self.ack = ack
    }

    /// Bytes from the subscription's stdout, split into lines.
    func receive(_ data: Data) {
        buffer.append(data)
        while let nl = buffer.firstIndex(of: 0x0A) {
            let line = buffer.subdata(in: buffer.startIndex..<nl)
            buffer.removeSubrange(buffer.startIndex...nl)
            handle(line)
        }
    }

    func handle(_ line: Data) {
        guard !line.isEmpty,
            let event = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any],
            let kind = event["event"] as? String
        else { return }
        switch kind {
        case "message":
            guard let m = event["message"] as? [String: Any], let seq = int64(m["seq"]) else { return }
            pending.append(m)
            pendingSeq = max(pendingSeq ?? seq, seq)
        case "unreadable":
            // A batch boundary, not just a seq to fold in. `jkb mq subscribe` reports an unreadable
            // row once and then sends nothing more — no `caught_up` either — until its seq is acked,
            // so waiting for `caught_up` here deadlocked: every notification after one corrupt row
            // was never shown, with the process alive and launchd seeing nothing wrong. (Stage-5
            // review.) Nothing to display for it; acking it with the batch before it is how the
            // stream moves past it.
            if let seq = int64(event["seq"]) { pendingSeq = max(pendingSeq ?? seq, seq) }
            caughtUp = true
            flush()
        case "caught_up":
            caughtUp = true
            flush()
        case "error":
            log("subscription: \(event["code"] ?? "?"): \(event["reason"] ?? "")")
        default:
            break  // events are only ever added; unknown ones are ignored
        }
    }

    /// One batch at a time, so an ack never commits past messages still being displayed: a crash
    /// then redelivers them, at least once, and posting or withdrawing by id twice is harmless.
    func flush() {
        guard caughtUp, !busy, let seq = pendingSeq else { return }
        let batch = fold(pending)
        pending = []
        pendingSeq = nil
        caughtUp = false
        busy = true
        display.apply(batch) { [weak self] in
            guard let self = self else { return }
            self.ack(seq)
            self.busy = false
            // A burst that ended while this one was on screen.
            self.flush()
        }
    }

    /// Whether a batch is being displayed and not yet acked.
    var isBusy: Bool { busy }
}

/// `serve --dry-run`: events on stdin, actions and acks on stdout, exit at EOF.
func serveDryRun() -> Never {
    let now = flags["now-ms"].flatMap { Int64($0) } ?? nowMs()
    let consumer = Consumer(display: PrintDisplay(now: now)) { seq in print("{\"ack\":\(seq)}") }
    while let line = readLine(strippingNewline: false) {
        consumer.receive(line.data(using: .utf8) ?? Data())
    }
    exit(EXIT_OK)
}

/// `serve`: run `jkb mq subscribe` and respawn it for as long as this process lives. A subscription
/// that ends — `jkb` upgraded under it, a fatal error, a database a newer jkb migrated — is started
/// again after a backoff that resets once one has run for a minute.
///
/// **Each run gets its own `Consumer`**, so a partial line or a half-folded burst from a run that died
/// cannot be mixed into the next one's; what it had not acked is delivered again by the next run.
/// **A run is over only when its process has exited AND its stdout has reached EOF**, and the next
/// starts only once that run's last batch has left the screen. The two signals reach the main queue
/// in no fixed order; keyed on the exit alone, a dead run's final burst could still be displayed
/// after the next run had redelivered and withdrawn it, leaving a stale post up. (Stage-5 review.)
final class Run {
    let consumer: Consumer
    var exited: Int32?
    var eof = false
    /// stderr's EOF too: the child's last `error: …` can reach the main queue after its exit and
    /// stdout's EOF, and a first-failure line logged before it loses the only explanation.
    var errEOF = false
    /// Set only once the process actually started, so a binary that cannot be launched never counts
    /// as a run that lasted.
    var startedAt: Date?
    /// Set by the first `finish` that acts. Both end signals can arrive after a launch failure too —
    /// the catch block and the pipe's EOF — and each scheduling a restart doubled the restarts every
    /// cycle (stage-5 re-review).
    var finishing = false
    /// The end of what the child wrote to stderr (or why it could not start), logged only if this run
    /// is the first of a failure — never once per retry into a log nothing rotates.
    var stderrTail = Data()

    init(consumer: Consumer) { self.consumer = consumer }

    func noteStderr(_ data: Data) {
        stderrTail.append(data)
        if stderrTail.count > 4096 { stderrTail = Data(stderrTail.suffix(4096)) }
    }
}

final class Subscription {
    let jkb: String
    let args: [String]
    private var backoff: TimeInterval = 1
    /// Consecutive runs that ended within a minute. A persistent failure — a missing jkb, a database a
    /// newer migration locked — would otherwise log a line per restart, for as long as it lasts, into
    /// a log nothing rotates; so only the first failure and the recovery are logged.
    private var failures = 0

    init(jkb: String, args: [String]) {
        self.jkb = jkb
        self.args = args
    }

    func start() {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: jkb)
        p.arguments = args
        let out = Pipe()
        let inp = Pipe()
        let err = Pipe()
        p.standardOutput = out
        p.standardInput = inp
        p.standardError = err
        let input = inp.fileHandleForWriting
        let run = Run(
            consumer: Consumer(display: CenterDisplay()) { seq in
                // Throws (rather than raising) on a pipe the child has closed; SIGPIPE is ignored.
                do {
                    try input.write(contentsOf: Data("{\"ack\":\(seq)}\n".utf8))
                } catch {
                    log("ack \(seq) not sent: \(error.localizedDescription)")
                }
            })
        out.fileHandleForReading.readabilityHandler = { [weak self] h in
            let data = h.availableData
            if data.isEmpty {
                h.readabilityHandler = nil
                DispatchQueue.main.async {
                    run.eof = true
                    self?.finish(run)
                }
                return
            }
            DispatchQueue.main.async { run.consumer.receive(data) }
        }
        err.fileHandleForReading.readabilityHandler = { [weak self] h in
            let data = h.availableData
            if data.isEmpty {
                h.readabilityHandler = nil
                DispatchQueue.main.async {
                    run.errEOF = true
                    self?.finish(run)
                }
                return
            }
            DispatchQueue.main.async { run.noteStderr(data) }
        }
        p.terminationHandler = { [weak self] proc in
            DispatchQueue.main.async {
                run.exited = proc.terminationStatus
                self?.finish(run)
            }
        }
        do {
            try p.run()
            run.startedAt = Date()
            DispatchQueue.main.asyncAfter(deadline: .now() + 60) { [weak self] in
                guard let self = self, run.exited == nil, self.failures > 0 else { return }
                log("the subscription has been up for a minute after \(self.failures) failed run(s)")
                self.failures = 0
                self.backoff = 1
            }
        } catch {
            out.fileHandleForReading.readabilityHandler = nil
            err.fileHandleForReading.readabilityHandler = nil
            run.noteStderr(Data("could not run \(jkb): \(error.localizedDescription)\n".utf8))
            run.exited = -1
            run.eof = true
            run.errEOF = true
            finish(run)
        }
    }

    /// Called on each of a run's end signals — exit, stdout EOF, stderr EOF; acts once, when all have
    /// arrived.
    func finish(_ run: Run) {
        guard let status = run.exited, run.eof, run.errEOF, !run.finishing else { return }
        run.finishing = true
        restartWhenIdle(run, status: status)
    }

    /// Restart once the run's last batch has left the screen.
    private func restartWhenIdle(_ run: Run, status: Int32) {
        if run.consumer.isBusy {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
                self?.restartWhenIdle(run, status: status)
            }
            return
        }
        if let started = run.startedAt, Date().timeIntervalSince(started) > 60 {
            backoff = 1
            failures = 0
        }
        failures += 1
        if failures == 1 {
            let said = String(decoding: run.stderrTail, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            log(
                "the subscription ended (status \(status)); restarting, quietly until it recovers"
                    + (said.isEmpty ? "" : ": \(said)"))
        }
        let delay = backoff
        backoff = min(backoff * 2, 30)
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in self?.start() }
    }
}

func doServe() -> Never {
    if flags["dry-run"] != nil { serveDryRun() }
    guard let jkb = flags["jkb"], !jkb.isEmpty else { fail("serve: --jkb is required", EXIT_USAGE) }
    guard let topic = flags["topic"], !topic.isEmpty else {
        fail("serve: --topic is required", EXIT_USAGE)
    }
    signal(SIGPIPE, SIG_IGN)
    var args: [String] = []
    if let db = flags["db"], !db.isEmpty { args += ["--db", db] }
    args += ["mq", "subscribe", topic, "--group", flags["group"] ?? "macos-notifier"]
    let subscription = Subscription(jkb: jkb, args: args)
    subscription.start()
    // Holds `subscription` for the life of the process; `dispatchMain` never returns.
    withExtendedLifetime(subscription) { dispatchMain() }
}

switch verb {
case "status": doStatus()
case "authorize": doAuthorize()
case "post": doPost()
case "remove": doRemove()
case "list": doList()
case "serve": doServe()
// The fallback's AppleScript, printed so scripts/tests/notify-hook.test.sh can compile it with
// osacompile: its text now arrives from inside the dev container, and a quoting mistake there means
// no notification at all, silently.
case "banner-script":
    print(bannerScript(title: flags["title"] ?? "", subtitle: flags["subtitle"] ?? "", body: flags["body"] ?? ""))
    exit(EXIT_OK)
default: fail("unknown verb: \(verb)", EXIT_USAGE)
}

// Every verb above but `serve` is asynchronous and exits from its completion handler. This is the
// backstop for a notification centre that never answers: a one-shot call must not hang its caller,
// so a silent timeout is the failure mode, never a wait.
let window: TimeInterval = (verb == "authorize") ? 300 : 10
RunLoop.main.run(until: Date().addingTimeInterval(window))
fail("timed out waiting for the notification centre", EXIT_TIMEOUT)
