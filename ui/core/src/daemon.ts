//! The `jkb serve` wire protocol, as data: addresses, the op/response/error shapes, and how a
//! reply is decoded. Portable — no Node, no fetch, no filesystem — so every adapter that speaks
//! to the daemon (the desktop app today, a web app later) shares one reading of it (D53.1).
//
// The Rust side is the source of truth: `crates/jkb-daemon/src/server.rs` (routes, auth),
// `crates/jkb-api/src/lib.rs` (`Request`, `Response`, `ApiError`, `ErrorCode`) and
// `crates/jkb-cli/src/remote.rs` (how a client finds the daemon and its token). Each constant
// below names the Rust item it mirrors.

/** `jkb_daemon::DEFAULT_ADDR`: where `jkb serve` binds unless told otherwise. */
export const DEFAULT_DAEMON_ADDR = "127.0.0.1:7117";

/** `jkb_daemon::PROTOCOL_VERSION`: the HTTP protocol this client speaks. */
export const PROTOCOL_VERSION = 1;

/**
 * `jkb_daemon::MAX_BODY_BYTES`: the largest request body `jkb serve` accepts. A larger one is
 * refused before it is sent — the daemon answers an oversized upload by closing the connection,
 * which reaches the client as "cannot reach jkb serve" rather than as the refusal it is.
 */
export const MAX_BODY_BYTES = 1024 * 1024;

/** `jkb_cli::remote::REMOTE_VAR`: the variable naming the daemon's URL, shared with the CLI. */
export const REMOTE_VAR = "JKB_REMOTE";

/** The variable overriding where the daemon's token is read from, shared with the CLI. */
export const TOKEN_FILE_VAR = "JKB_REMOTE_TOKEN_FILE";

/**
 * `jkb_api::ErrorCode`, snake_case as serialized ([`WIRE_ERROR_CODES`]). `unknown` is a code from a
 * newer daemon that this build does not know; treat it like `internal`.
 *
 * Plus one code that is never on the wire: `token_refused`, the client refusing the daemon's token
 * file itself (a link, not a regular file, not a token) — kept apart from `unavailable` because it
 * is a sign of tampering, not of a daemon that is down. A daemon sending it decodes as `unknown`.
 */
export type ErrorCode =
  | "no_such_topic"
  | "topic_conflict"
  | "no_such_group"
  | "queue_full"
  | "too_large"
  | "invalid"
  | "ack_beyond_end"
  | "corrupt_payload"
  | "bad_request"
  | "busy"
  | "unauthorized"
  | "schema_newer"
  | "unavailable"
  | "not_found"
  | "unsupported"
  | "forbidden"
  | "internal"
  | "unknown"
  | "token_refused";

/** Every `jkb_api::ErrorCode` as serialized, in declaration order; pinned to the Rust enum by a test. */
export const WIRE_ERROR_CODES: readonly ErrorCode[] = [
  "no_such_topic",
  "topic_conflict",
  "no_such_group",
  "queue_full",
  "too_large",
  "invalid",
  "ack_beyond_end",
  "corrupt_payload",
  "bad_request",
  "busy",
  "unauthorized",
  "schema_newer",
  "unavailable",
  "not_found",
  "unsupported",
  "forbidden",
  "internal",
  "unknown",
];

const ERROR_CODES: ReadonlySet<string> = new Set<string>(WIRE_ERROR_CODES);

/** `jkb_api::ApiError`: why an op failed. */
export interface ApiError {
  readonly code: ErrorCode;
  readonly message: string;
  /** The message concerned, for `corrupt_payload`. */
  readonly seq?: number;
}

/**
 * One op, `jkb_api::Request` serialized: an `op` tag plus its fields, e.g.
 * `{ op: "session.list", all: false }`. The daemon refuses an unknown op or field, so a typo is
 * a `bad_request`, never a silently different read.
 */
export interface OpRequest {
  readonly op: string;
  readonly [field: string]: unknown;
}

/** `jkb_api::Response` serialized: a `result` tag plus its fields. Fields are only ever added. */
export interface OpResponse {
  readonly result: string;
  readonly [field: string]: unknown;
}

/** `GET /v1/hello`. */
export interface Hello {
  readonly protocol: number;
  readonly schema_version: number;
  readonly supported_schema: number;
  readonly ops: readonly string[];
}

/** A call's outcome. A failure is data, not a throw, so it crosses an IPC boundary intact. */
export type Outcome<T> =
  | { readonly ok: true; readonly value: T }
  | { readonly ok: false; readonly error: ApiError };

/** An [`ApiError`] with this code and message. */
export function apiError(code: ErrorCode, message: string): ApiError {
  return { code, message };
}

/** A failed [`Outcome`]. */
export function failed<T>(code: ErrorCode, message: string): Outcome<T> {
  return { ok: false, error: apiError(code, message) };
}

/**
 * The daemon's base URL, from the value of [`REMOTE_VAR`] (`jkb_cli::remote::daemon_url`): that
 * value, trimmed, with `http://` added when it names no scheme; else the default address.
 */
export function daemonUrl(remote: string | undefined): string {
  const v = (remote ?? "").trim();
  if (v === "") return `http://${DEFAULT_DAEMON_ADDR}`;
  return v.includes("://") ? v : `http://${v}`;
}

/**
 * The port in `url` (`jkb_cli::remote::port_of`): explicit, else the scheme's default. An IPv6
 * literal's colons are inside its brackets and are not mistaken for one.
 */
export function portOf(url: string): number {
  const sep = url.indexOf("://");
  const scheme = sep >= 0 ? url.slice(0, sep) : "http";
  const rest = sep >= 0 ? url.slice(sep + 3) : url;
  const authority = rest.split("/")[0] ?? "";
  const close = authority.lastIndexOf("]");
  const afterHost = close >= 0 ? authority.slice(close + 1) : authority;
  const colon = afterHost.lastIndexOf(":");
  if (colon >= 0) {
    const digits = afterHost.slice(colon + 1);
    const port = Number(digits);
    if (/^\d+$/.test(digits) && port <= 65535) return port;
  }
  return scheme.toLowerCase() === "https" ? 443 : 80;
}

/**
 * Where a `jkb serve` on `port` writes its token under `home` (`jkb_cli::service::serve_token_path`):
 * `~/.jkb/daemon/<port>/token`, whichever database it serves. The token rotates each start.
 */
export function tokenPath(home: string, port: number): string {
  return `${home.replace(/\/+$/, "")}/.jkb/daemon/${port}/token`;
}

/** The URL `POST /v1/op` is served at, with the long-poll wait when one is asked for. */
export function opUrl(base: string, waitMs?: number): string {
  const root = base.replace(/\/+$/, "");
  return waitMs === undefined ? `${root}/v1/op` : `${root}/v1/op?wait_ms=${Math.max(0, Math.floor(waitMs))}`;
}

/** The URL `GET /v1/hello` is served at. */
export function helloUrl(base: string): string {
  return `${base.replace(/\/+$/, "")}/v1/hello`;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** The error a non-2xx body carries, or `unavailable` when the body is not a jkb `ApiError`. */
function errorFrom(status: number, parsed: unknown, raw: string): ApiError {
  if (isObject(parsed) && typeof parsed["code"] === "string" && typeof parsed["message"] === "string") {
    const code = ERROR_CODES.has(parsed["code"]) ? (parsed["code"] as ErrorCode) : "unknown";
    const seq = parsed["seq"];
    return typeof seq === "number" ? { code, message: parsed["message"], seq } : { code, message: parsed["message"] };
  }
  // Not jkb serve's answer — a proxy, or another service on its port — so not a refusal of the
  // request: the daemon is out of reach, as `jkb_daemon::client::decode` reads it.
  const snippet = raw.length > 200 ? `${raw.slice(0, 200)}…` : raw;
  return apiError("unavailable", `HTTP ${status} from something other than jkb serve (a proxy?): ${snippet}`);
}

function parse(body: string): unknown {
  try {
    return JSON.parse(body) as unknown;
  } catch {
    return undefined;
  }
}

/**
 * Decode a `POST /v1/op` reply: a 2xx carries a `Response` (a `result` tag), anything else an
 * `ApiError`. A 2xx whose body is not a response is an `internal` failure, never a success.
 */
export function decodeOpReply(status: number, body: string): Outcome<OpResponse> {
  const parsed = parse(body);
  if (status >= 200 && status < 300) {
    if (isObject(parsed) && typeof parsed["result"] === "string") {
      return { ok: true, value: parsed as OpResponse };
    }
    return failed("internal", "jkb serve answered 2xx with a body that is not a response");
  }
  return { ok: false, error: errorFrom(status, parsed, body) };
}

/** Decode a `GET /v1/hello` reply, refusing a daemon that speaks another protocol. */
export function decodeHelloReply(status: number, body: string): Outcome<Hello> {
  const parsed = parse(body);
  if (status < 200 || status >= 300) return { ok: false, error: errorFrom(status, parsed, body) };
  if (
    !isObject(parsed) ||
    typeof parsed["protocol"] !== "number" ||
    typeof parsed["schema_version"] !== "number" ||
    typeof parsed["supported_schema"] !== "number" ||
    !Array.isArray(parsed["ops"]) ||
    !parsed["ops"].every((o) => typeof o === "string")
  ) {
    return failed("internal", "jkb serve answered /v1/hello with a body that is not a hello");
  }
  if (parsed["protocol"] !== PROTOCOL_VERSION) {
    return failed(
      "unsupported",
      `jkb serve speaks protocol ${parsed["protocol"]}; this app speaks ${PROTOCOL_VERSION}`,
    );
  }
  return { ok: true, value: parsed as unknown as Hello };
}
