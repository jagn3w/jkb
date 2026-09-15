# The message queue

jkb's message queue: Kafka-shaped topics, keys and consumer positions, stored in `jkb.db`. Built so
that host-side daemons — the macOS notifier first — can be told things by processes that must not
touch them directly, including processes in the dev container. Design and decision record:
`openspec/changes/jkb-message-queue/design-r3.md` (Q1–Q6, H4). Code: `crates/jkb-core/src/mq.rs`
(the rules), `crates/jkb-api` (the typed operations), `crates/jkb-cli/src/mq_cli.rs` (`jkb mq`).

Read this before touching any of them, and before writing a consumer in another language.

## The model

| term | meaning |
|---|---|
| **topic** | a named stream, e.g. `claude/notify`: the unit of size cap, default TTL and consumer groups. Few and long-lived — never one per entity. |
| **message** | immutable: a queue-assigned `seq`, a `key`, a `kind`, a JSON `payload`, `producer`, `enqueued_at`, optional `expires_at`. |
| **key** | what the message is about (`host/<h>/repo/<r>/session/<s>`). Opaque to the queue, which never filters on it — **pure Kafka**. |
| **kind** | what consumers dispatch on (`notify.post`). |
| **consumer group** | a named reader with **one committed position**: everything with `seq <= position` is consumed by it. Every group is handed every message; a consumer skips what it does not want. |

**Order comes from `seq`, never from a clock.** `seq` is `AUTOINCREMENT`, and SQLite admits one write
transaction at a time and holds its lock until commit, across every process on one kernel — so a
later-committed message always has a higher `seq`, and a cumulative ack is sound. A topic's seqs have
gaps (one sequence serves every topic; reaping leaves holes); they are never reordered.

## The rules

These are the user's, decided 2026-09-13; the code states them in `mq.rs`'s module doc and tests each.

- **Expired messages are still delivered**, flagged `expired: true`. A TTL never skips anything; it
  makes a message *eligible* to be reaped.
- **A message is reaped only when BOTH** it has been consumed by **every** group of its topic (and the
  topic has at least one group) **and** it has either **expired** or the topic is **at its size cap**.
- **Size cap:** 10 MiB or 10,000 messages per topic by default, set at creation. At the cap, `send`
  reaps what the rule allows, oldest first; if that is not enough it is **refused** with `queue_full`.
- **Idle groups** — nobody has polled or acked for 7 days by default — are removed by compaction, so a
  consumer that never returns cannot hold a topic full for ever. A consumer whose group was removed is
  told `no_such_group`, recreates it from now, and accepts the gap.
- **Compaction** runs in the reap service (`jkb task reap --watch`) every pass, and does nothing for a
  topic compacted within its interval (3 days by default). It opens the database on its own, so a
  schema the reap binary does not know stops only the compaction, never the sweep.
- A **producer never creates a topic**; `topic create` is idempotent for an identical spec and refuses
  a different one.
- Payload ≤ 64 KiB and must parse back as JSON; key ≤ 512 bytes, no NUL.
- Queue writes are **not changelogged**: `jkb undo` never replays or erases a delivery.

## The operations (`jkb-api`)

Every client goes through `jkb_api::Backend`. A request is a JSON object tagged by `op`; an unknown op
or an unknown field is **refused**, never ignored, because a container's `jkb` and the host's are
routinely built from different checkouts.

| op | fields | result |
|---|---|---|
| `mq.topic_create` | `topic`, `spec?` {`max_bytes`, `max_messages`, `default_ttl_ms`, `group_idle_ms`, `compact_every_ms`} | `created` {`created`} |
| `mq.send` | `topic`, `key`, `kind`, `payload`, `ttl_ms?`, `producer` | `sent` {`seq`} |
| `mq.group_create` | `topic`, `group`, `from_start?` | `created` {`created`} |
| `mq.poll` | `topic`, `group`, `max`, `after?` | `messages` {`messages`} |
| `mq.ack` | `topic`, `group`, `seq` | `position` {`position`} |
| `mq.compact` | `force?` | `compacted` {…counts} |
| `mq.inspect` | — | `topics` {`topics`} |
| `mq.tail` | `topic`, `limit` | `messages` {`messages`} |
| `notify.event` | `session`, `event` (`needed`\|`tool_finished`\|`user_acted`\|`turn_ended`\|`session_ended`), `tool?`, `message?`, `cwd?`, `owner?`, `instance?` | `notified` {`state`, `moved`, `effects`, `refusal?`, `sent`} |
| `notify.open_sessions` | — | `sessions` {`sessions`: [{`session`, `tool`, `owner`, `instance`, `updated_at`}]} |
| `notify.gone` | `session`, `owner`, `instance` (as `notify.open_sessions` reported them) | `notified` {…} |
| `kb.ambient` | `cwd`, `home?` | `ambient` {`namespace`} |
| `kb.query` | `dsl`, `default_scope?`, `limit?`, `count?`, `order?` (`id`\|`updated_desc`) | `items` {`items`}, or with `count` `count` {`count`} |
| `kb.ls` | `path?`, `all?`, `recursive?` | `listing` {`rows`: [{`parent`, `child`}]} |
| `kb.tree` | `path?`, `all?`, `depth?` (≤ 48) | `tree` {`nodes`: [{`child`, `children`}]} |
| `kb.cat` | `uid` | `content` {`content`} |
| `kb.grep` | `pattern` (non-empty), `scope?`, `ignore_case?`, `mode?` (`lines`\|`names`\|`count`) | `grep_hits` {`hits`: [{`uid`, `kind`, `lines`: [{`line`, `text`}]}], `count`, `truncated`} |
| `kb.search` | `dsl`, `default_scope?`, `route` (`vector`\|`fts`\|`hybrid`), `limit` (≤ 1000), `context?` (≤ 50) | `search_hits` {`hits`} |
| `task.ready` | `dsl`, `default_scope?`, `limit?` | `items` {`items`} |
| `task.show` | `uid` (a uid or bare slug) | `task` {`task`: {`item`, `transitions` (the last 5), `subtasks`}} |
| `task.subtasks` | `uid`, `all?` | `children` {`children`} |

The `notify.*` ops are the permission-notification machine, which runs in the daemon and sends its
effects on `claude/notify` as `notify.post` (payload `id`, `session`, `title`, `subtitle`, `body`; TTL
12 h) and `notify.withdraw` (`id`, `session`; no TTL), keyed `session/<id>` — and only while the topic
has a consumer group. [notifications.md](notifications.md) is their record.

**The agent read set** (`kb.*`, `task.ready`/`show`/`subtasks`; tasks S6.1) is in
`crates/jkb-api/src/kb.rs`, and each read has **one implementation there**: the host CLI serves `jkb
query`, `find`, `recent`, `search`, `ls`, `tree`, `grep`, `cat` and `jkb task next`/`show`/`subtasks`
through a `LocalBackend` too (`crates/jkb-cli/src/read_cli.rs`), so the daemon cannot answer one of
them differently from the host — pinned byte-for-byte by `tests/cli.rs`
`the_read_set_answers_through_the_daemon_exactly_as_on_the_host`. The CLI only renders, and its
`--json` shapes are unchanged (the UI parses them, D31). The decisions in it:

- **An unscoped read's scope comes from the client's directory, re-rooted.** `kb.ambient` takes the
  client's `cwd` and `$HOME`; a `cwd` under that home is looked up under the serving process's home,
  which is what makes the container's `/home/vscode/repos/jkb` find the mount the host recorded as
  `/Users/<u>/repos/jkb` (the container binds `~/repos` at `~/repos`). In one process the homes are
  equal and nothing changes. The path is compared against the mounts table and never opened.
  Residual: a container directory under its home that is *not* bound from the host re-roots onto a
  host path that may be a mount, and scopes to that namespace — wrong scoping, no content moved.
  Verified by disabling the re-rooting: the daemon's answers then differ from the host's.
- **The daemon serves only the FTS search route.** Vector and hybrid embed the query text, which would
  have the host call a model for the container, so a backend with no embedder — what `jkb serve`
  builds — refuses them with `unsupported`, and remote mode's `jkb search` defaults to `--route fts`
  (the host keeps `hybrid`).
- **The daemon serves reads on a second, `query_only` connection** (`Db::reader`,
  `LocalBackend::with_reader`). `Db` runs every call on one thread, so a container's long read — a
  wide grep, a deep tree — held up every write behind it, the notification hook's 1 s round trip
  included (pinned by `a_long_read_on_the_reader_does_not_hold_up_a_write`: a 1.5 s read, and the
  write beside it under 0.7 s). The reader cannot write, so no read op can either. The host CLI keeps
  one connection: a process there serves one command.
- **Every read is bounded in what a hand-written request can make the daemon hold**, because the CLI
  is not the only client: `kb.search` takes at most 1000 hits (the answer's size, and the hybrid
  route's fusion from twice the limit — which only the host, with an embedder, runs) and 50 chunks of
  context either side; `kb.grep` refuses an empty pattern, reads items one at a time
  (`item::grep_each`), and stops collecting at 8 MiB of lines with `truncated` set while it goes on
  counting — `-c` and `-l` ask for no lines at all; `jkb recent` orders and limits on the server
  (`order: updated_desc`) rather than fetching the scope; `kb.tree` descends at most 48 levels (each is
  two levels of JSON, and serde_json refuses past 128), not into a node whose reference is one of its
  ancestors' (a reference is looked up as a namespace before an item, so data can make nodes list each
  other), and into nothing once 10,000 nodes are listed. Each guard was checked by removing it.

`after` is the consumer's **fetch position**, separate from its committed one as in Kafka: a consumer
that has handed messages on but not yet acked them polls with `after` set to the last seq it handed
on. Without it, a batch of unacked messages comes back from every poll and nothing past it is read.

Errors carry a stable `code`: `no_such_topic`, `topic_conflict`, `no_such_group`, `queue_full`,
`too_large`, `invalid`, `not_found` (an item a read names does not exist), `unsupported` (a search
route this backend does not serve), `ack_beyond_end`, `corrupt_payload` (with `seq`, so a consumer can ack past
it), `bad_request`, `busy` (transient — retry: another writer held the database lock past the busy
timeout, or, over HTTP, the daemon is at a concurrency limit or the group already has a long-poll in
progress), `internal`, and `schema_newer` — a newer `jkb` migrated the database, so this build must
not write to it. That one means different things by where it comes from: **in-process**, this process
is too old and only a newer binary helps (exit, and let a supervisor start one); **from `jkb serve`**,
the daemon is too old and `setup.sh` restarts it on the newer binary, so waiting helps. Over HTTP two
more: `unauthorized` (a missing or stale token) and `unavailable` (the daemon cannot be reached,
something other than the daemon answered, or the daemon cannot open its database). A code a client
does not know decodes as `unknown` and is treated like `internal`, so a newer host can add codes.
Under `--json`, every `jkb mq` verb except `subscribe` (whose stdout is its event stream) prints any
failure to stdout as `{"error":{"code":…,"message":…}}` as well as exiting 1 — a refusal with its own
code, invalid input as `bad_request`, a local database that will not open as `schema_newer` or
`unavailable`, anything else as `internal`.

An idle `mq.poll` — nothing past the fetch position, and its `last_poll_at` refreshed within the hour
(or a quarter of the topic's idle period, whichever is shorter) — is answered by a read and takes no
write lock, so an idle subscriber does not contend with every other writer several times a second.

`mq.tail` shows a message whose stored payload does not parse with its raw text and
`"unreadable": true`, rather than stopping at it.

## `jkb mq subscribe` — the consumer stream

For a daemon in any language: run it as a child process and speak NDJSON.

```
jkb mq subscribe <topic> --group <name> [--from-start] [--at-most-once] [--batch N] [--interval-ms N]
```

**stdout, one event per line:**

- `{"event":"message","message":{"seq":…,"key":…,"kind":…,"payload":…,"producer":…,"enqueued_at":…,"expires_at":…,"expired":…}}`
- `{"event":"unreadable","seq":…,"reason":…}` — a stored payload that does not parse; reported once.
  Ack its `seq` to move past it.
- `{"event":"caught_up","seq":…}` — the last poll returned less than a full batch: everything
  available up to `seq` has been handed over. Emitted once when the stream first has nothing more,
  and again after each burst. A consumer that folds messages (the notifier folds post/withdraw pairs
  per notification) acts at this boundary rather than guessing with a timeout.
- `{"event":"error","code":…,"reason":…,"fatal":bool}` — `fatal: true` is followed by exit status 1.

**Consumers must ignore event types and fields they do not know.** Events and fields are only ever
added; a strict decoder breaks on the first new one.

**stdin, one command per line:** `{"ack":<seq>}` commits the group's position through `seq`. Anything
else — including a line that is not UTF-8 — is answered with a non-fatal `bad_request` error event. A
failure to READ stdin is fatal (exit 1), never treated as EOF, since acks after it would silently
never apply.

**Semantics.**

- The group is created if missing: from now, or from the start with `--from-start`. An existing group
  keeps its position.
- Each message is emitted **once per run**. What was not acked when the run ends is delivered again
  by the next run — **at-least-once**, so a consumer must be idempotent.
- `--at-most-once` acks each message before emitting it: a crash loses it instead.
- **EOF on stdin ends the subscription**, after processing the acks that preceded it — waiting up to
  10 s for an outage that holds one; an ack still not applied then ends the run with a fatal error
  event naming its seq (those messages come again next run), never a silent exit 0. Keep stdin open
  for as long as you want messages.
- A closed stdout ends it with status 0.
- **Transient refusals are waited out, never fatal** — at startup, and for every poll, ack and group
  recreation alike: `busy` (a locked database, a busy daemon) silently; `unavailable` (the daemon is
  down or restarting), and in remote mode `schema_newer` (`setup.sh` has not yet restarted the
  daemon — tens of seconds on every upgrade), with **one non-fatal error event per outage**. An outage
  ends when a call succeeds — unless it began with an ack that is still held, which only that ack
  succeeding ends (a poll, a read, can pass while the ack, a write, is refused). The position is in the database, so the stream resumes where it stopped. An ack sent
  during an outage is held and applied when the backend answers — including one sent before the group
  could be created, which is never sent against a group that does not exist yet (that would
  "recreate" it from now and lose `--from-start`); under `--at-most-once` a message whose ack could
  not be applied yet is not emitted, and comes back on the next poll.
- **In local mode `schema_newer` is fatal**, on a poll or an ack alike: the subscriber itself is
  older than the database, and exiting is what lets its supervisor start the newer binary.
- **Any refusal not handled where it arises is fatal.** Each call names the refusals it handles — an
  ack: `no_such_group` (recreate) and the refusals about that ack alone (`ack_beyond_end`, `invalid`,
  `bad_request`, reported as non-fatal events); a poll: `no_such_group` and `corrupt_payload` — and
  everything else that is not transient ends the stream with a fatal event. A code added later is
  fatal until someone decides otherwise, never silently taken for a harmless one.
- The event that gives up on a held ack at EOF carries the code of the refusal that held it. Stdin
  closing while the group cannot be created yet still creates it and applies a held ack, both within
  one 10 s wait. A group recreation the backend is too busy for is retried after the poll interval,
  never straight away.
- **Startup failures with no events:** a non-zero exit with nothing on stdout means the subscription
  never started — bad arguments (clap, exit 2), or in local mode a database that could not be opened
  (exit 1) — and stderr says which. A refusal of the subscription itself (`no_such_topic`, say) is a
  fatal error event.
- A group removed while subscribed (it idled out) is recreated from now, with a non-fatal
  `no_such_group` error event that says messages in between were not delivered.

Pinned end to end through a real binary by `tests/cli.rs`
`mq_subscribe_speaks_ndjson_over_pipes_and_resumes_after_the_ack` (delivery, ack, EOF, resume), and in
`crates/jkb-cli/src/mq_cli.rs`'s tests for `caught_up`, `unreadable` (once, with its seq, moving on
after an ack), a group recreated mid-stream, a closed stdout, a stdin read failure, bad commands,
`--at-most-once`, and each transient path against a scripted backend: two outages, startup, a held
ack (applied after, and at EOF within one wait or given up loudly with its code), `--at-most-once`
during an outage, a regroup during one (and its interval), an ack read before the group exists, an
outage whose op is abandoned, an in-process `schema_newer`, and an unhandled refusal on an ack.

## Over HTTP: `jkb serve` and remote mode

A process that must not open `jkb.db` — the dev container's `jkb`, whose kernel corrupts the host's
database across the bind mount — reaches the same operations through the host daemon
(`crates/jkb-daemon`, design H2/H3).

**The daemon.** `jkb serve [--addr 127.0.0.1:7117] [--token-file PATH]`, normally the `com.jkb.serve`
launchd/systemd unit that `jkb service install` writes and `setup.sh` (re)starts. It:

- refuses an unspecified address (`0.0.0.0`, `::`);
- mints a 256-bit bearer token each start and writes it, owner-only, to
  `~/.jkb/daemon/<port>/token` whichever database it serves — keyed by what a client knows: the
  notification hook and the dev container (through the `~/.jkb` bind) know the address, never the
  database, and one path per home let a second daemon overwrite the first's live token. The default
  path is refused inside the dev container (`JKB_NS_MARKER`) and on a filesystem shared with another
  kernel, so a `jkb serve` run inside cannot replace the host daemon's token, and for port 0, whose
  port no client can know in advance; `--token-file` overrides all three — after the
  port is bound, so a fresh token means a listening daemon. That directory is writable from the dev
  container, so the write is made relative to a directory handle opened without following links,
  through an `O_EXCL` temp file with a random name, renamed into place: a link planted there cannot
  redirect it, and a symlinked `daemon/` is refused;
- refuses every operation, with `schema_newer`, while the database is at a schema this build does not
  know — a newer `jkb` migrated it; restart the daemon from that build. "Newer" is read from
  refinery's history table, which each migration writes in its own transaction (`PRAGMA user_version`
  is stamped only after all of them). Every write refuses it **inside its own IMMEDIATE transaction**
  (`jkb_core::Db::write_txn`, for every long-lived writer, not only the daemon), so no migration can
  commit between the check and the write; the daemon also asks before each read and each re-poll of a
  long-poll;
- does not exit when it cannot open the database at start (a supervisor would restart-loop it): it
  binds, writes its token, answers every request with the reason — `schema_newer`, or `unavailable`
  for any other open failure — and tries the open again on a request at most every 5 s, so a failure
  that passes (a lock held past the busy timeout during a long write) needs no restart. Not every
  start-up failure is retried: an unwritable token directory still stops it, and a database file that
  does not exist yet is created, as every `jkb` command does;
- holds a 1 MiB body limit, and separate concurrency budgets for operations and long-polls, answering
  `busy` when one is exhausted. A request past authentication holds its permit from before its body is
  read;
- bounds the unauthenticated side too: at most 256 connections (one more is closed on accept — and
  fewer if the descriptor limit, raised toward 4096 at start, leaves less room beside the database's
  own files; launchd's default soft limit is 256), request headers — and idle keep-alive — within
  10 s, a body within 10 s, `Connection: close` on every refusal before authentication, and behind it a
  second guard: a connection that has not presented the token within 10 s is closed whatever it is
  doing (a client that pipelines and never reads stops hyper's header timer; measured, either guard
  alone frees the slot, and an authenticated long-poll outlives the deadline), and a pause after an accept that failed for want of
  descriptors or memory — only then, so a peer resetting its connection does not slow anyone else.

**The wire.** Both endpoints need `Authorization: Bearer <token>`. The body is always JSON; clients
branch on its `code`, the HTTP status is for people and proxies.

```
GET  /v1/hello            → {"protocol":1,"schema_version":…,"supported_schema":…,"ops":[…]}
POST /v1/op[?wait_ms=N]   → a response object ({"result":…}), or an error object ({"code","message"})
```

`wait_ms` (capped at 30 s) turns an empty `mq.poll` into a long-poll: it returns as soon as a message
arrives — at once for a send the daemon served, within 250 ms for one another host process wrote. Any
other op ignores it. **One long-poll per group at a time**: a second, while the first is held, is
answered `busy` (design H3's budget rule, so a burst of subscribes to one group cannot occupy the
poll budget).

**The token keeps out other local processes, not the container's agent**, which can read the file.
What bounds the agent is the operation set: nothing in it touches a file, a URL or a process on the
host.

**Remote mode.** With `JKB_REMOTE=http://<host>:<port>` set (and `JKB_REMOTE_TOKEN_FILE`, default
`~/.jkb/daemon/<port>/token` for that URL's port), `jkb`:

- runs `jkb mq …` and the agent read set (above) through the daemon;
- runs the commands that need no database (`notify`, `guide`, `commands`) as usual;
- **refuses everything else before it does anything** — with a reason: host-only commands (`sync`,
  `mount`, `ingest`, `service`, `serve`) never go through the daemon, the rest are not ported yet;
- refuses `--db`, and a non-empty `JKB_DB` — a process configured with both names a database two ways
  at once, and silently obeying one hides the other;
- treats an error body that is not the daemon's (a proxy's `502`, say) as `unavailable`, and re-reads
  the token only when the daemon itself answered `unauthorized`.

The table is an exhaustive `match` (`crates/jkb-cli/src/remote.rs`), so a new subcommand does not
compile until it says which it is. A daemon that cannot be reached is remembered for 5 seconds
(`~/.cache/jkb/remote-unreachable-<port>`), so a burst of short-lived `jkb` processes pays one connect
timeout, not one each.

`setup.sh` activates every unit `jkb service units` lists (label, installed path, role) — restarting
each, so none keeps running an old binary. When the daemon's unit starts, it waits up to 10 s for a
fresh token at `jkb service token-path` as proof the daemon is listening, then **asks the daemon
itself** (remote mode, at `jkb service serve-url`, as the container would) and judges by its answer:
success is `up`; `schema_newer` is `refusing` — and the watcher, the same binary, is marked failed
too, since it cannot open the database either; any other failure is `undecided`, with the answer in
the warning. An open by the setup shell would measure a different process. Each unit's failure is reported under its own role — the daemon's on its own
`jkb serve` summary line, the watcher units' on the watcher line
(`scripts/lib.sh` `activate_services`, pinned by `scripts/tests/services.test.sh` against stub
service managers and by `tests/cli.rs`
`service_units_and_token_path_name_what_install_and_serve_actually_write` against the real binary).
`jkb service install` prints the same restart form as its activation advice.

Pinned by `crates/jkb-daemon/tests/loopback.rs` (the server and client over real TCP: round trip,
long-poll wake-ups, token rotation, unspecified-address refusal, body limit, unknown fields, schema
refusal before and during a long-poll, a database that will not open and then does, one long-poll per
group and its release when the client goes away, `wait_ms` on a non-poll, the connection cap, read
timeouts, a pipelining client that never authenticates and an authenticated long-poll that outlives
that deadline, a proxy's error, the unreachable cache),
`crates/jkb-core/src/store.rs` (a write after a newer migration, and one that waited on that
migration's lock),
`crates/jkb-daemon/src/token.rs` (planted links), and `tests/cli.rs`
`remote_mode_reaches_the_daemon_and_refuses_everything_else` and
`serve_answers_schema_newer_rather_than_exiting_on_a_newer_database` (real binaries on both sides).

## Not yet

- `JKB_REMOTE` set in the container — at the cutover (tasks S6), not before: remote mode refuses
  `JKB_DB` and every unported command, and the container's agents still need both. The network path
  it will take already exists; see `.container/README.md`, "The one opening to the host".
- Porting the task-mutate command set to operations, and client-side ingest — stages S6.2/S6.3. The
  read set is done (S6.1); `stat`, `item show`, `related`, `inv`, `view`, `ns`, `tag`, `history` and
  `blob` are still refused remotely.
- The MCP server's read tools (`jkb-mcp/src/logic.rs`) still read the database directly rather than
  through `jkb-api` (design H4 says they should become its callers).
- `work` (competing consumers) and `compacted` (newest per key) queue types — design Q9.
- A native, non-subprocess client (Swift) — it would speak the HTTP protocol above. `jkb-notifier
  serve`, the first consumer, runs `jkb mq subscribe` as a child for now (design N2).
