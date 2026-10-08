//! The `jkb serve` HTTP client, in the main process (D53.1).
//
// The app's only way to read or write jkb: `GET /v1/hello` and `POST /v1/op` with the daemon's
// bearer token. It mirrors the CLI's `RemoteBackend` (`crates/jkb-daemon/src/client.rs`): the
// token is read from the file `jkb serve` writes and cached; it rotates each time the daemon
// starts, so a call the daemon refuses as `unauthorized` is retried once with the token read
// afresh; and a body past the daemon's limit is refused here rather than sent.
//
// No Electron import: this is plain Node, so it is tested against a real HTTP server without
// launching Electron. The wire format itself (URLs, reply decoding) is `@jkb/core`'s.

import { constants } from "node:fs";
import { open } from "node:fs/promises";

import {
  MAX_BODY_BYTES,
  decodeHelloReply,
  decodeOpReply,
  failed,
  helloUrl,
  opUrl,
  type Hello,
  type OpRequest,
  type OpResponse,
  type Outcome,
} from "@jkb/core";

/** A token file larger than this is not a token. `jkb serve` writes 64 hex characters. */
const MAX_TOKEN_BYTES = 4096;

/** How long a call may take, past any long-poll wait it asked for. */
const DEFAULT_TIMEOUT_MS = 30_000;

export interface DaemonClientOptions {
  /** The daemon's base URL (`@jkb/core`'s `daemonUrl`). */
  readonly url: string;
  /** Where the daemon writes its token (`@jkb/core`'s `tokenPath`, or `JKB_REMOTE_TOKEN_FILE`). */
  readonly tokenFile: string;
  /** How long a call may take, past any long-poll wait. */
  readonly timeoutMs?: number;
  /** The fetch to use; the global one by default. */
  readonly fetch?: typeof fetch;
}

/** Options for one op. */
export interface OpOptions {
  /** `mq.poll` only: hold an empty answer up to this long for a message to arrive. */
  readonly waitMs?: number;
}

function describe(e: unknown): string {
  if (e instanceof Error) {
    const cause = (e as { cause?: unknown }).cause;
    return cause instanceof Error ? `${e.message}: ${cause.message}` : e.message;
  }
  return String(e);
}

/**
 * Read the daemon's token from `path`, whitespace trimmed.
 *
 * `~/.jkb` is writable from the dev container, so the file is opened without following a link
 * (a planted link would have the app read, and send as a header, whatever it points at) and must
 * be a small regular file holding one printable word.
 */
export async function readToken(path: string): Promise<Outcome<string>> {
  const flags = constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0);
  let handle;
  try {
    handle = await open(path, flags);
  } catch (e) {
    const code = (e as { code?: unknown }).code;
    if (code === "ELOOP" || code === "EMLINK") {
      return failed("unavailable", `refusing the daemon token at ${path}: it is a symbolic link`);
    }
    return failed(
      "unavailable",
      `no daemon token at ${path} (${describe(e)}); is jkb serve running on the host?`,
    );
  }
  try {
    const stat = await handle.stat();
    if (!stat.isFile()) {
      return failed("unavailable", `refusing the daemon token at ${path}: not a regular file`);
    }
    if (stat.size > MAX_TOKEN_BYTES) {
      return failed("unavailable", `refusing the daemon token at ${path}: ${stat.size} bytes is not a token`);
    }
    const token = (await handle.readFile("utf8")).trim();
    if (token === "") return failed("unavailable", `${path} holds no token`);
    if (!/^[\x21-\x7e]+$/.test(token)) {
      return failed("unavailable", `${path} does not hold a token (expected one printable word)`);
    }
    return { ok: true, value: token };
  } catch (e) {
    return failed("unavailable", `reading the daemon token at ${path}: ${describe(e)}`);
  } finally {
    await handle.close();
  }
}

/** A client of one `jkb serve`. Holds the token; never hands it out. */
export class DaemonClient {
  readonly url: string;
  readonly #tokenFile: string;
  readonly #timeoutMs: number;
  readonly #fetch: typeof fetch;
  #token: string | undefined;

  constructor(options: DaemonClientOptions) {
    this.url = options.url;
    this.#tokenFile = options.tokenFile;
    this.#timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.#fetch = options.fetch ?? globalThis.fetch;
  }

  /** `GET /v1/hello`. */
  hello(): Promise<Outcome<Hello>> {
    return this.#authenticated((token) =>
      this.#send(helloUrl(this.url), { method: "GET", token }, this.#timeoutMs, decodeHelloReply),
    );
  }

  /** `POST /v1/op`. */
  op(request: OpRequest, options: OpOptions = {}): Promise<Outcome<OpResponse>> {
    const body = JSON.stringify(request);
    const size = Buffer.byteLength(body, "utf8");
    if (size > MAX_BODY_BYTES) {
      return Promise.resolve(
        failed("too_large", `request refused before sending: ${size} bytes is over jkb serve's ${MAX_BODY_BYTES}`),
      );
    }
    const { waitMs } = options;
    const timeout = this.#timeoutMs + (waitMs ?? 0);
    return this.#authenticated((token) =>
      this.#send(opUrl(this.url, waitMs), { method: "POST", token, body }, timeout, decodeOpReply),
    );
  }

  /**
   * Run `call` with the cached token, and once more with a freshly read one when the daemon
   * itself answered `unauthorized` — it has restarted and rotated the token. Any other 401, or a
   * second refusal, is the answer.
   */
  async #authenticated<T>(call: (token: string) => Promise<Outcome<T>>): Promise<Outcome<T>> {
    const cached = await this.#currentToken(false);
    if (!cached.ok) return cached;
    const first = await call(cached.value);
    if (first.ok || first.error.code !== "unauthorized") return first;
    const fresh = await this.#currentToken(true);
    if (!fresh.ok) return fresh;
    return call(fresh.value);
  }

  async #currentToken(fresh: boolean): Promise<Outcome<string>> {
    if (!fresh && this.#token !== undefined) return { ok: true, value: this.#token };
    const read = await readToken(this.#tokenFile);
    this.#token = read.ok ? read.value : undefined;
    return read;
  }

  async #send<T>(
    url: string,
    req: { method: "GET" | "POST"; token: string; body?: string },
    timeoutMs: number,
    decode: (status: number, body: string) => Outcome<T>,
  ): Promise<Outcome<T>> {
    const headers: Record<string, string> = { authorization: `Bearer ${req.token}` };
    if (req.body !== undefined) headers["content-type"] = "application/json";
    let response: Response;
    let text: string;
    try {
      response = await this.#fetch(url, {
        method: req.method,
        headers,
        ...(req.body !== undefined ? { body: req.body } : {}),
        signal: AbortSignal.timeout(timeoutMs),
      });
      text = await response.text();
    } catch (e) {
      if (e instanceof Error && e.name === "TimeoutError") {
        return failed("unavailable", `jkb serve at ${this.url} did not answer within ${Math.round(timeoutMs / 1000)}s`);
      }
      return failed("unavailable", `cannot reach jkb serve at ${this.url}: ${describe(e)}`);
    }
    return decode(response.status, text);
  }
}
