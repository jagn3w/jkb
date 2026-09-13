// jkb-notifier — post and withdraw macOS notifications by identifier.
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
// Usage:
//   jkb-notifier post --id <id> --title <t> [--subtitle <s>] [--body <b>]
//   jkb-notifier remove --id <id>
//   jkb-notifier list
//   jkb-notifier status
//   jkb-notifier authorize
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

// MARK: - arguments

/// `--flag value` pairs after the verb. Deliberately not a general parser: the only caller is a
/// shell hook whose invocation is pinned by `scripts/tests/notify-hook.test.sh`.
func parseFlags(_ args: ArraySlice<String>) -> [String: String] {
    var out: [String: String] = [:]
    var rest = Array(args)
    while rest.count >= 2, rest[0].hasPrefix("--") {
        out[String(rest[0].dropFirst(2))] = rest[1]
        rest.removeFirst(2)
    }
    return out
}

let argv = CommandLine.arguments
guard argv.count > 1 else {
    fail("usage: jkb-notifier post|remove|list|status|authorize [--flag value ...]", EXIT_USAGE)
}
let verb = argv[1]
let flags = parseFlags(argv.dropFirst(2))
let center = UNUserNotificationCenter.current()

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

func doPost() {
    guard let id = flags["id"], !id.isEmpty else { fail("post: --id is required", EXIT_USAGE) }
    guard let title = flags["title"], !title.isEmpty else {
        fail("post: --title is required", EXIT_USAGE)
    }

    withSettings { s in
        // Refuse rather than post into the void. An unauthorized `add` returns no error and
        // displays nothing, so posting anyway would report success for a notification the user
        // will never see — and the caller could not tell that from a working one. Refusing lets
        // the hook fall back to the plain osascript banner until `authorize` has been run.
        switch s.authorizationStatus {
        case .authorized, .provisional, .ephemeral: break
        default:
            fail(
                "not authorized to post (\(name(s.authorizationStatus))) — run: jkb-notifier authorize",
                EXIT_UNAUTHORIZED)
        }

        let content = UNMutableNotificationContent()
        content.title = title
        if let sub = flags["subtitle"], !sub.isEmpty { content.subtitle = sub }
        if let body = flags["body"], !body.isEmpty { content.body = body }
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

        // Reusing the identifier REPLACES the delivered notification rather than stacking a
        // second one, which is what makes a per-session id the whole of the bookkeeping.
        let request = UNNotificationRequest(identifier: id, content: content, trigger: nil)
        center.add(request) { err in
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

switch verb {
case "status": doStatus()
case "authorize": doAuthorize()
case "post": doPost()
case "remove": doRemove()
case "list": doList()
default: fail("unknown verb: \(verb)", EXIT_USAGE)
}

// Every verb above is asynchronous and exits from its completion handler. This is the backstop
// for a notification centre that never answers: a hook must not hang a Claude Code session, so a
// silent timeout is the failure mode, never a wait.
let window: TimeInterval = (verb == "authorize") ? 300 : 10
RunLoop.main.run(until: Date().addingTimeInterval(window))
fail("timed out waiting for the notification centre", EXIT_TIMEOUT)
