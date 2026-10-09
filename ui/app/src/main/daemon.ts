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
import { lstat, open } from "node:fs/promises";
import { dirname, isAbsolute, join, relative, sep } from "node:path";

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
  /**
   * The directory below which no component of `tokenFile` may be a link (`readToken`): the home
   * directory. Unset, or not above `tokenFile`, only the token file itself is checked.
   */
  readonly trustedRoot?: string;
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
 * The first component of `path` strictly below `root` — `root` itself excluded, the leaf included —
 * that is a symbolic link, named relative to `root`; or `undefined` when there is none. A missing
 * component is not a link: the open that follows says it is missing.
 */
async function linkBelow(root: string, path: string): Promise<string | undefined> {
  const parts = relative(root, path).split(sep);
  let at = root;
  for (const part of parts) {
    at = join(at, part);
    try {
      if ((await lstat(at)).isSymbolicLink()) return relative(root, at);
    } catch {
      return undefined;
    }
  }
  return undefined;
}

/** Whether `path` lies strictly below `root`. */
function isBelow(root: string, path: string): boolean {
  const rel = relative(root, path);
  return rel !== "" && rel !== ".." && !rel.startsWith(`..${sep}`) && !isAbsolute(rel);
}

/**
 * Read the daemon's token from `path`, whitespace trimmed.
 *
 * `~/.jkb` is writable from the dev container, so a link planted at **any** component below
 * `trustedRoot` is refused — the `daemon` directory, the `<port>` directory, or the token itself
 * — as is anything but a small regular file holding one printable word. A planted link would have
 * the app read, and send as a header, whatever it points at. `trustedRoot` (the home directory) and
 * its ancestors are trusted: the container cannot replace its bind's own root. When `path` is not
 * below `trustedRoot`, its own directory is the trusted root and only the leaf is checked.
 *
 * Node has no `openat`, so the walk is by path: the chain is checked before the open, the leaf is
 * opened `O_NOFOLLOW`, and after it the chain is checked again and the opened file must be the one
 * the path names now (device and inode). A link swapped in and out between those calls is the
 * residual race; the Rust writer (`jkb_daemon::token::write`), which holds the root token's
 * directory by handle, is the side that closes it. `O_NONBLOCK` makes a FIFO at the path open at
 * once — to be refused as not a regular file — instead of parking a libuv thread until a writer
 * appears.
 *
 * Failures carry no absolute path: the message crosses the bridge, and the renderer is not told
 * where the token lives (`AppInfo`). A component is named relative to the trusted root.
 */
export async function readToken(path: string, trustedRoot?: string): Promise<Outcome<string>> {
  const root = trustedRoot !== undefined && isBelow(trustedRoot, path) ? trustedRoot : dirname(path);
  const refused = (why: string): Outcome<string> => failed("token_refused", `refusing the daemon token: ${why}`);
  const linked = async (): Promise<Outcome<string> | undefined> => {
    const link = await linkBelow(root, path);
    return link === undefined ? undefined : refused(`${link} is a symbolic link`);
  };

  const before = await linked();
  if (before !== undefined) return before;
  const flags = constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0);
  let handle;
  try {
    handle = await open(path, flags);
  } catch (e) {
    const code = (e as { code?: unknown }).code;
    if (code === "ELOOP" || code === "EMLINK") return refused(`${relative(root, path)} is a symbolic link`);
    return failed(
      "unavailable",
      `no daemon token (${typeof code === "string" ? code : "unreadable"}); is jkb serve running on the host?`,
    );
  }
  try {
    const stat = await handle.stat();
    if (!stat.isFile()) return refused("it is not a regular file");
    if (stat.size > MAX_TOKEN_BYTES) return refused(`${stat.size} bytes is not a token`);
    const after = await linked();
    if (after !== undefined) return after;
    const now = await lstat(path).catch(() => undefined);
    if (now === undefined || now.dev !== stat.dev || now.ino !== stat.ino) {
      return refused("the file changed while it was opened");
    }
    const token = (await handle.readFile("utf8")).trim();
    if (token === "") return failed("unavailable", "the daemon token file holds no token");
    if (!/^[\x21-\x7e]+$/.test(token)) return refused("the file does not hold a token (expected one printable word)");
    return { ok: true, value: token };
  } catch (e) {
    const code = (e as { code?: unknown }).code;
    return failed("unavailable", `reading the daemon token: ${typeof code === "string" ? code : describe(e)}`);
  } finally {
    await handle.close();
  }
}

/** A client of one `jkb serve`. Holds the token; never hands it out. */
export class DaemonClient {
  readonly url: string;
  readonly #tokenFile: string;
  readonly #trustedRoot: string | undefined;
  readonly #timeoutMs: number;
  readonly #fetch: typeof fetch;
  #token: string | undefined;

  constructor(options: DaemonClientOptions) {
    this.url = options.url;
    this.#tokenFile = options.tokenFile;
    this.#trustedRoot = options.trustedRoot;
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
    const read = await readToken(this.#tokenFile, this.#trustedRoot);
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
