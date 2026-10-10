<!-- generated from jkb design design:daemon-and-messaging-18dcdf2a2d45d6f884c253, edit there (version 159.ngGE1J7o0dOTCwGFvsnWyuvJDAGGqJ7W_pWzCQGIoufM5rfTCgGI2LH6w6vJAgGLzITutqDlCAGN3vaWl6OcCQGP8reJgdCwDQGQ6N7RiM3zCAGS6JPyh53sAgGUoJSTyLi_DgGW3ITWkL_ZBQGXvpP-ha-RBQGY7ICV-PWdAQGaqKnX7YWJBAGborff9vTKCQGe0o-5-LurCAGfjJ7LqOWLAgGg5LK1x-_-BgGhuNjwgcluAZ_g_Y2r9roHAaPu2cDny8QDAaPKmZTN7pYEAaW84Z3OkqQEAaaY-rv9mFQBp-i--8iXrgcBp8LY3aegjwgBqaC6sf3xpQcBqabF84LyWwGetM-4t6H9DwGj6M7VnYm_AQGt8MLNoKLlDAGvgLyph8ZBAa-yqeWUqJQJAbHC1-W6ub0GAbH2qbSdnfgFAbbi_oLm-dkDAbfg5sLdvMYBAbrm9Mqu9LUOAbuq1PuNpeoGAbzewJHE6ewBAbzKntKX394LAbyw-P_is9IBAbuuyMiGo8wPAcDw2vOytoMLAcL6gK2ClvkNAcaGiJqCndEJAcbA1t3O1_gEAciawd_k5-8MAcjSoLuKuYUIAcaMmIaN0L4PAcqA27eH8J8CAc3W-cCvmasDAc6ov--e_8kOAdDwrc7mkSUB0cb42fOQuQMB0qbxvvW4sAsB06zrvaiwGgHV2JS6g_25BQHYiruKrMCJAQHYmNOHjJPzDwHa4J-FzLVG-cYI2vTZ89Sb0AkB3ObV3o3w0wgB3Oqdn4_jKwHe_JfJuPypCAHfjNbSsfeSDQHg6NeLrcyjCQHhvK_Z2u-tDgHgzoK8s9u8BgHfzqToh_urBwHo1Pb08fzYCAHovPuw3OLlAgHq0oXT1MW3AQHq3sLC8rX-AgHv7MPKkOLxBwHwsNeyzujHAgHylqD9-ZqDAgHz2PDKmvWzCQH0qv3g2evsAgHyquP20cSjA0z6us3fxb6fCgH7sKCauvOADQH6_Kemz7PHCwGFrZulp-ZsAYXrm8XNnfMHAYfJp5e18MYLAYnb5bT0qC8Bicul5dWkkAYBi7e0kOT15A8Bj9-0lYuc6wMBkPWcjd7ZuQIBmNevt_fduA4BnOPOlbGn7AoBncuemvWPkQkBnvGkyYqXowEBnu3xormPqQ0Bo6npr9e0sA4Bo6ubjffrtQkBo7fZyeiU5gYBpe2d6I_XmwYBo6WSxoD75wwBo93GwMjo9wMBqdvdxIbD0w8Bo4Xr9LPLuw8Br8HUhJfx4Q0BsJHmr4bd5woBr638rcaA4wgBr-_G3JmH-QUBto_tk-CtOwG258DcyIf0BAG46c_Z0tHECwG99avG_sTaAQHChcec-IqODQHDndPo0efVDwHDxbi-rY-TDgHF_aP9u4_VDAHFwd-I74jUAgHHybL_yNv1AwHF9bXe_a7UBQHH26ObrKbHAQHLpZX3uf4iAcvLna-0nuUDAc2pvN78j2MB0fWNisGpiwoB05fd3MXdmgwB1_eo5YTxgQgB2JX2-dHpmwIB2uXuw6PrqwMB29vb6M35sAMB3deQgMj_-QcB3a3D4N6M8goB3pm5zNDhtAIB4I3uupicqAIB4Jnnr-rHqQYB49Ojksu2kwkB5P_MwsyR-gsB5bmQ6LGAjAEB5oeHlIuSxgkB6NnB2JOs_AsB6f334MD4zgwB6oftzr2a-A4B64ft6eXJxgUB69P95Nej1A4B6_fr9pPqwgkB64eGlYS-kw0B6PGeo7GW-QMB8JWanqqx7AgB9pOApsv1lgoB9rGTrfXn8wQB-MO_lovJ6QgB98_8mMCo_QsB-v3T-s3mvg0B-q3qspid_QEB_PWbh4LP9wEB_O-FvsmT7QoB98fKxtup1AUB___bz7TN5QgB, blake3 e4f83384d980d900bc37897f3eaa65d63fbba628e5c0dd059e772de1bc6b1b7f) -->
# Daemon and messaging

This design records how processes that must not open `jkb.db` still reach the knowledge base, and
what jkb built on top of that path. In order: who may open the database at all; `jkb serve`, the
host daemon; the typed operations it serves, which are its security boundary; remote mode, the
CLI's way of speaking to it; the session verbs, split into client-side git and database ops; the
message queue, stored in `jkb.db`; the registry of Claude Code sessions; and sticky permission
notifications, the queue's first consumer.

Read it before touching `crates/jkb-core/src/{mq,notify,claude_session}.rs`, `crates/jkb-api`,
`crates/jkb-daemon` (`jkb serve`), `crates/jkb-cli/src/{mq_cli,remote,notify,ops_cli,task_cli}.rs`,
`.claude/hooks/notify-sticky.sh`, `macos/notifier/` or `scripts/build-notifier.sh`, and before
writing a queue consumer in another language.

Two facts run under all of it. A process on the dev container's kernel corrupts the host's
`jkb.db` if it opens it, so the host owns the database and the container sends requests. And
every request is a typed operation whose effect is database work only, so what the container can
make the host do is the closed list of operations, never a command line. Who may present which
credential to the daemon (the operator's root token, role grants, the container credential,
harness tickets) is the agents-and-roles design; the container itself is the
sandbox-and-container design.

## Who may open `jkb.db`

The plan for this whole subsystem changed on one measurement, taken on 2026-09-13 while a message
queue was being designed to live in loose files. Everything below follows from it.

### SQLite cannot be shared across the container's bind mount

`.container/sqlite-share-probe.py`, one process on each kernel (virtiofs, Docker Desktop):

| probe | one kernel | container ↔ macOS |
|---|---|---|
| fcntl write lock held; other side `F_GETLK`/`F_SETLK` | seen, refused | **unlocked, acquired** |
| `MAP_SHARED` counter, 2,514 writes | all seen | **2 distinct values** |
| 4 writers + checkpointer per side, jkb's pragmas | 100k+ rows, integrity ok | **malformed after 38 commits** |

No SQLite journal mode survives that, and a *reader* is unsafe too: jkb opens read-write, and
closing what looks like the last WAL connection checkpoints and truncates. Inside the container,
`statfs` on the `~/.jkb` and `~/repos` binds reports `FUSE_SUPER_MAGIC` (`0x65735546`,
"fuseblk"); `/tmp` is overlayfs and the cargo volume ext4.

So **the host owns `jkb.db`**. A process on the container's kernel never opens it, reads
included. The container reaches the knowledge base through a host daemon over one TCP port,
sending typed database operations, never whole CLI commands, which would run gates and git on the
host, outside the container's sandbox. And because only one kernel touches `jkb.db`, the message
queue can be tables in it.

### `Db::open` refuses a database on a shared or network filesystem

*A rule every call site must remember is the defect*, so the rule lives in `Db::open`, not in the
container's environment and not in the CLI. It `statfs`es the database's **directory** (a fresh
`--db` path has no file yet, and `-wal`/`-shm` land beside it, so the file is the wrong thing to
ask; the nearest existing ancestor for a fresh path) and refuses an `f_type` in {`FUSE_SUPER_MAGIC`
0x65735546, measured on the container's binds; `V9FS_MAGIC` 0x01021997, colima/podman 9p;
`NFS_SUPER_MAGIC` 0x6969; `SMB2` 0xFE534D42; `CIFS` 0xFF534D42}. The error names the measurement
and the remedy (`JKB_REMOTE`). SQLite's own guidance rules these filesystems out for WAL; this
makes it structural. The predicate is a pure function over `f_type`, unit-tested; the live half is
asserted by `.container/verify.sh`.

It is Linux-only, and that is the argument, not a gap: the side that must never open the database
is always the Linux container, and the macOS host's `~/.jkb` is APFS. The choke point is real on
the Rust side: `jkb-core`'s `db` module is private, only `store.rs` calls `db::open`, and a grep
test fails the suite on a new `Connection::open(` outside `db.rs` (in-memory test opens excepted).
The guard's first review found and closed a bypass live: the bundled SQLite is built with
`-DSQLITE_USE_URI`, so a `file:` URI reached the database around the check; its second found
`Db::backup` writing unguarded. A `-shm` that SQLite deletes between the listing and the `statfs`
is skipped, while its directory is still judged (a race a parallel test run exposed).

Rejected: a process with `JKB_DB` unset silently falling back to `~/.jkb/jkb.db`. It still falls
back, and the open now refuses.

### Scripts get the same rule

`scripts/swarm-status.sh` ran `sqlite3` on `${JKB_DB:-~/.jkb/jkb.db}`, the sanctioned direct read,
and a `sqlite3` shell opens read-write and checkpoints on close, which is exactly the unsafe read
above. A critique of the plan caught that the scripts bypassed the `Db::open` choke point.
`scripts/lib.sh` has `refuse_shared_db <path>` (`stat -f -c %T` of the directory against the same
set), called before every `sqlite3`, and a shell test greps `scripts/` for a `sqlite3` not
preceded by it.

### Host processes on one kernel open the database directly

The CLI, `jkb sync --watch`, `jkb task reap --watch` and `jkb mcp` on the host keep opening
`jkb.db` themselves. SQLite locking is sound within one kernel, and forcing every host command
through the daemon buys no safety. The user decided it (2026-09-13): host-side processes may access
the database directly; only the container goes through the daemon. The consequence the rest of
this design carries: a host process can write the database without the daemon knowing, so nothing
the daemon does may assume it saw every write.

### A write refuses a newer schema inside its own transaction

A newer `jkb` migrates the database on its first open, and an older process still running must
not write to a schema it does not know. "Newer" is read from refinery's history table, which each
migration writes in its own transaction (`PRAGMA user_version` is stamped only after all of
them). Every write refuses it **inside its own IMMEDIATE transaction** (`jkb_core::Db::write_txn`,
for every long-lived writer, not only the daemon), so no migration can commit between the check
and the write; the refusal is the `schema_newer` error. Pinned in `crates/jkb-core/src/store.rs` (a
write after a newer migration, and one that waited on that migration's lock).

`schema_newer` means different things by where it comes from. **In-process**, this process is too
old and only a newer binary helps: exit, and let a supervisor start one. **From `jkb serve`**, the
daemon is too old, and `setup.sh` restarts it on the newer binary, so waiting helps.

## The daemon: `jkb serve`

`crates/jkb-daemon`. One long-running host process, normally the `com.jkb.serve` launchd agent
(macOS) or systemd user unit (Linux) that `jkb service install` writes beside the sync and reap
services, with the same pure-generator pattern (`service.rs`).

### One host process holds the writer and runs the migrations

`jkb serve [--addr 127.0.0.1:7117] [--token-file PATH]` holds the `Db` (the writer-actor) and runs
migrations. **The container never migrates**, which also removes the failure where a newer
container binary locked the host out of its own database.

### HTTP/1.1 and JSON, because the sandbox proxy speaks HTTP

The transport is HTTP/1.1 with JSON bodies, on `hyper` and `tokio`, both already locked when the
choice was made (crates.io is blocked from the sandbox, so no dependency that was not already
cached could be added). Measured inside the dev container: Claude Code's sandbox routes Bash egress
through an HTTP proxy (`HTTP_PROXY=localhost:3128`, posture `strictAllowlist: true`), so a raw-TCP
protocol from a sandboxed command cannot traverse it, and `host.docker.internal` does not resolve
inside that sandbox (`getent hosts` empty). Through the proxy, a sandboxed `jkb` reaches the daemon
as an ordinary proxied request to an allowlisted host.

### It binds loopback only

The daemon refuses an unspecified address (`0.0.0.0`, `::`) and binds `127.0.0.1`. Measured on
the Mac on 2026-09-14 (Docker Desktop 4.87.0, context `desktop-linux`, daemon on `127.0.0.1:7117`):
a plain `curlimages/curl` container reaches it by `host.docker.internal` (`401`, with `-4` too), so
the runtime forwards to the host's loopback and the port is never exposed on the LAN;
`--add-host=host.docker.internal:host-gateway` changes nothing there (`401`) and is pinned anyway
for a Linux engine (CI); the alias resolves to `fdc4:f303:9324::254` first and `192.168.65.254`
over v4, identically in a plain container and in `jkb-dev`. From the Claude Bash sandbox in the
rebuilt container, through its proxy, `/v1/hello` answered `401` without the token and the hello
JSON with it, and a closed port answered `502`. colima forwards differently and is unmeasured; if
it needs a gateway bind, bind that address only, never `0.0.0.0`.

### The token is keyed by port, minted per start, and written without following links

The daemon mints a 256-bit bearer token each start and writes it, owner-only, to
`~/.jkb/daemon/<port>/token`, whichever database it serves, and compares presented tokens in
constant time. The path is keyed by what a client knows: the notification hook and the dev
container (through the `~/.jkb` bind) know the address, never the database. Beside the database, a
host set up with `--db` wrote it where no client looked; one path per home let a second daemon on
another port overwrite the first's live token.

The default path is refused inside the dev container (its image sets `JKB_NS_MARKER`) and on a
filesystem shared with another kernel, so a `jkb serve` run inside cannot replace the host
daemon's token through the bind; the filesystem check alone missed a native-Linux engine, whose
bind is plain ext4. Port 0 has no default path, since no client could derive it. `--token-file`
overrides all three. The token is written after the port is bound, so a fresh token means a
listening daemon.

That directory is writable from the dev container, so the write is made relative to a directory
handle opened without following links, through an `O_EXCL` temp file with a random name, renamed
into place: a link planted there cannot redirect it, and a symlinked `daemon/` is refused. The
first review of the daemon found the token write following links planted in the container-writable
`~/.jkb/daemon`; `crates/jkb-daemon/src/token.rs` pins the planted-link cases. Residual: a
different container, with no marker, on a native-Linux bind.

### The token keeps out other processes, not the container's agent

Stated plainly: the agent in the container can read the token through the bind. The token keeps
out other local processes and other containers that do not mount `~/.jkb`. What bounds the agent
is the operation set: nothing in it touches a file, a URL or a process on the host. Anything the
operation set allows, the agent can do from the container, which by design equals its earlier
direct database access minus reach into the host's filesystem.

### The daemon refuses a newer schema and survives a database that will not open

It refuses every operation with `schema_newer` while the database is at a schema this build does
not know, and asks before each read and each re-poll of a long-poll as well as inside every write.
It does not exit when it cannot open the database at start, because a supervisor would
restart-loop it: it binds, writes its token, answers every request with the reason
(`schema_newer`, or `unavailable` for any other open failure), and tries the open again on a
request at most every 5 s, so a failure that passes (a lock held past the busy timeout during a
long write) needs no restart. Not every start-up failure is retried: an unwritable token directory
still stops it, and a database file that does not exist yet is created, as every `jkb` command
does. `jkb service` dispatches before any database opens, so `setup.sh` can start a daemon over a
newer-migrated database and then ask it.

### Concurrency is a set of budgets, not one cap

Separate permit budgets for operations, long-polls, the agent read set (`max_reads`, 16) and
`ingest.text` (`max_ingests`, 1), answering `busy` when one is exhausted, so a burst of subscribes
or a container's greps cannot starve a hook's 1 s budget. A request past authentication holds its
permit from before its body is read.

### The unauthenticated side is bounded too

At most 256 connections (one more is closed on accept), and fewer if the descriptor limit, raised
toward 4096 at start, leaves less room beside the database's own files; launchd's default soft
limit is 256. Request headers, and idle keep-alive, within 10 s; a body within 10 s;
`Connection: close` on every refusal before authentication. Behind that, a second guard: a
connection that has not presented the token within 10 s is closed whatever it is doing. A client
that pipelines and never reads stops hyper's header timer; measured, either guard alone frees the
slot, and an authenticated long-poll outlives the deadline. After an accept that failed for want of
descriptors or memory, and only then, the daemon pauses, so a peer resetting its connection does
not slow anyone else.

### An oversized body is drained before the refusal

The body limit is 1 MiB (`jkb_daemon::MAX_BODY_BYTES`). An oversized body is read to its end, up to
eight times the limit, and discarded before the 413 goes out. Answering first closed the socket with
the upload unread, and the reset that sends let the client's kernel drop the 413 unread: flaky on
macOS under the land gate's load. Pinned by `an_oversized_body_is_drained_before_the_refusal`.
`RemoteBackend` checks the cap before sending any op, because a body many times the cap was
otherwise cut off mid-upload and reported as a daemon that could not be reached.

### The wire is two endpoints

Both need `Authorization: Bearer <token>`. The body is always JSON; clients branch on its `code`,
and the HTTP status is for people and proxies.

```
GET  /v1/hello            → {"protocol":1,"schema_version":…,"supported_schema":…,"ops":[…]}
POST /v1/op[?wait_ms=N]   → a response object ({"result":…}), or an error object ({"code","message"})
```

`hello` returns the protocol version, the schema versions and the op list, so a client can refuse
early with "rebuild the host `jkb`" instead of failing mid-command. `wait_ms` (capped at 30 s)
turns an empty `mq.poll` into a long-poll; any other op ignores it. **One long-poll per group at a
time**: a second, while the first is held, is answered `busy`, so a burst of subscribes to one
group cannot occupy the poll budget, and the slot is released when the client goes away.

### Long-poll wake is driven by the database, not by the daemon's own sends

A per-topic condition variable notified by the daemon's own sends is only the fast path: a host
process can insert without the daemon knowing. The floor is the daemon checking `PRAGMA
data_version` (no lock) every ~250 ms and waking waiters when it moves. A long-poll returns at once
for a send the daemon served and within 250 ms for one another host process wrote, never at the
`wait` bound. Subscribers are woken only by a response that actually sent something
(`Request::may_send` covers the notification ops too). A critique of the plan found the first
version keeping the wake path in the daemon while host processes could write around it.

### Reads are served on a second, read-only connection

`Db` runs every call on one thread, so a container's long read (a wide grep, a deep tree) held up
every write behind it, the notification hook's 1 s round trip included. The daemon serves reads on a
second, `query_only` connection (`Db::reader`, `LocalBackend::with_reader`), pinned by
`a_long_read_on_the_reader_does_not_hold_up_a_write` (a 1.5 s read, and the write beside it under
0.7 s).

Which ops go there is said once, by `Request::is_agent_read`: the agent read set (`kb.*`,
`task.ready`/`show`/`subtasks`/`why`, `task.facts`/`by_branch`/`review_findings`, `repo.gate`), not
every op that does not write. The reader serves one call at a time behind a client's greps, so the
queue's and the hook's own short reads (`mq.inspect`, `mq.tail`, `notify.open_sessions`,
`session.list`, whose `SessionStart` sweep has 1 s) stay on the writer. Classing by "does not
write" put that sweep behind a container's grep, and a third review caught it. The backend picks
the connection from the class, and no dispatch arm chooses. Pinned by
`every_op_is_served_on_the_connection_its_class_names`, which checks each op's class against the
test's own list (with the writer held every read answers, with the reader held every other op
does): two mutants first survived because the test filtered on the very predicate it tested.

### A permit is held until its answer is written

A permit is released only when its call has returned on its blocking thread and hyper has written
or dropped the answer. Released with the request's future, a client that asked and hung up grew the
reader's queue while the budget read empty. The answer is handed to hyper in 64 KiB frames, because
given one frame hyper copied the whole answer into its buffer and dropped the body, and the permit,
before sending a byte (measured: a loopback client that never read got its permit straight back).
And a write that makes no progress for `write_stall` (10 s) closes its connection, since hyper has
no write timeout: without it a client that stopped reading kept its permit until the daemon
restarted. Pinned by `a_permit_outlives_a_cancelled_request_until_its_call_returns`,
`an_unread_answer_holds_its_permit_until_the_write_deadline` and
`reads_have_their_own_permits_and_a_bounded_answer`.

### Every read that lists is bounded by one byte budget

`kb::Budget` is charged row by row with what each row serializes to, so the answer is a prefix of
the full one and says `truncated: true` (the field is omitted otherwise). The daemon gives each read
16 MiB (`read_budget_bytes`); the host CLI's is unlimited, and the CLI says on stderr when an answer
was cut. It replaced per-op caps, each of which a second review found measured in the wrong unit
(chunks of context, while a document hit's context is its whole body; bytes of line text, while each
line carries its own JSON) or missing (`kb.query` with no limit). Not bounded, stated: `kb.cat` and
`task.show`'s own body are the one item asked for, and a namespace's children are gathered before
they are sorted and charged. Pinned by
`every_listing_read_stays_within_its_budget_and_says_when_it_was_cut` across all ten listing reads;
each guard was checked by removing it.

### Reads that do work are bounded by the work

`kb.search` takes at most 1000 hits and 50 chunks of context either side. A query evaluates at most
64 `tag:`/`-tag:` terms (`query::MAX_TAG_TERMS`): each is a subquery, and ~1000 of them from an
8 KiB request exceeded SQLite's expression depth as an internal error. `kb.grep` refuses an empty
pattern and reads items one at a time (`item::grep_each`), counting past the budget, and `-c`/`-l`
ask for no lines. `jkb recent` orders and limits on the server (`order: updated_desc`). `kb.tree`
descends at most 48 levels (each is two levels of JSON, and serde_json refuses past 128), not into a
node whose reference is one of its ancestors' (an item uid equal to its namespace's path made it
recurse forever), and stops after 10,000 nodes, a cut the answer names apart (`at_node_cap`), since
unlike the budget no host lifts it. The frontier (`task.ready`) is ordered and limited over ids, and
a task's subtasks are streamed, so at most one body is held at a time (`task::ready_ids`,
`task::subtasks_each`). Every list in `jkb-core` a client can lengthen (ids, uris, and a query's
`kind:` values) is bound as one JSON parameter (`sql::json_ids`, `json_strings`), since a
placeholder per element failed past SQLite's 32,766 variables.

### `setup.sh` restarts every unit and asks the daemon itself

`post-merge` runs `setup.sh`, which runs `cargo install`, and the new CLI migrates on its first open
under a daemon built from the old checkout. So `setup.sh` activates every unit `jkb service units`
lists (label, installed path, role), restarting each, so none keeps running an old binary. When the
daemon's unit starts, it waits up to 10 s for a fresh token at `jkb service token-path` as proof the
daemon is listening, then **asks the daemon itself** (remote mode, at `jkb service serve-url`, as the
container would) and judges by its answer: success is `up`; `schema_newer` is `refusing`, and the
watcher, the same binary, is marked failed too, since it cannot open the database either; any other
failure is `undecided`, with the answer in the warning. An open by the setup shell would measure a
different process. Each unit's failure is reported under its own role (`scripts/lib.sh`
`activate_services`, pinned by `scripts/tests/services.test.sh` against stub service managers and by
`tests/cli.rs` `service_units_and_token_path_name_what_install_and_serve_actually_write`). `jkb
service install` prints the same restart form as its activation advice.

### VS Code must not auto-forward the daemon port

Measured 2026-09-14: after something in the container listened on 7117, VS Code ("Code Helper")
held the Mac's `127.0.0.1:7117`, so `com.jkb.serve` crash-looped on `EADDRINUSE` and connections to
the port hung. Mostly an outage rather than a token leak, since the token is already readable in the
container, but a container process holding the host's port can also impersonate the daemon to a
host client. The fix is `onAutoForward: ignore` for the port in the `devcontainer.metadata` label
in `container.json`'s `runArgs`, pinned by `check-config.sh`; the user confirmed on the Mac that
attaching honours it. And `jkb serve` refuses with `ServeError::AddrInUse`, naming `lsof -nP
-iTCP:<port> -sTCP:LISTEN` and this cause, in the callee rather than in `setup.sh`, so `serve.log`
says it too.

### The daemon's own tests run over real TCP

`crates/jkb-daemon/tests/loopback.rs` drives server and client over real TCP on Linux CI: round
trip, long-poll wake-ups, token rotation, unspecified-address refusal, body limit, unknown fields,
schema refusal before and during a long-poll, a database that will not open and then does, one
long-poll per group and its release when the client goes away, `wait_ms` on a non-poll, the
connection cap, read timeouts, a pipelining client that never authenticates and an authenticated
long-poll that outlives that deadline, a proxy's error, and the unreachable cache. `tests/cli.rs`
`remote_mode_reaches_the_daemon_and_refuses_everything_else` and
`serve_answers_schema_newer_rather_than_exiting_on_a_newer_database` run real binaries on both
sides.

### The daemon is a single point of failure, stated

Down, every remote command refuses, setup and doctor report it, and notifications degrade to
nothing (the hook stays silent). The refused-filesystem set also refuses legitimate FUSE and
network filesystems, which is correct for SQLite.

## Typed operations

New crate `jkb-api`: `enum Request` / `enum Response` (serde, tagged by `op`), and one trait every
client goes through.

### One dispatch, three backends

```rust
pub trait Backend {
    fn call(&self, req: Request) -> Result<Response, ApiError>;
}
pub struct LocalBackend { db: Db, /* embedder, … */ }   // host: dispatch in-process
pub struct RemoteBackend { base: Url, token: Secret }   // container: reqwest::blocking
```

The daemon is `LocalBackend` behind HTTP; the host CLI uses `LocalBackend` directly; the
container's CLI uses `RemoteBackend`. One dispatch, so the host and the container cannot answer one
op two ways. The queue's producer and consumer traits are implemented once over `Backend`, so the
same consumer code runs in-process on the host or over HTTP. `jkb-mcp/src/logic.rs` already
expressed a subset of jkb as plain sync functions over `&Db` returning JSON, and became a caller of
`jkb-api` rather than a second copy.

### An op is admissible only if it is pure database work

What would be new from the container is anything that makes the *host* touch the host's filesystem
or run a process. So an op is admissible only if it is pure database work. Excluded, with the reason
each is an escalation:

| excluded | why |
|---|---|
| `mount create` / `task bind` with a `file://` URI | the host's `jkb sync --watch` would then read and write that host directory: `file:///Users/<u>/.ssh` imports keys into the KB, where the agent reads them |
| `ingest <path>` as a path, **or as bytes** | a path makes the host read a host path; bytes make the host run `pdf-extract`/`scraper` on attacker-chosen input, parser attack surface moved onto the host, and most PDFs exceed the body cap |
| `ingest <url>` | the host would fetch outside the container's egress firewall |
| writing `namespaces.metadata` under `repos/*` (incl. `undo` of such a write) | the stored land gate is read from `metadata.gate` and run with `sh -c` at `task land`/`task gate`: a row would choose the command the host user runs |
| content/status writes to an item bound under a mount whose directory is outside `~/repos` | the host's sync watcher reconciles that binding and writes the host file, so an admitted item write becomes a host file write in, say, `~/Documents` |
| `sync` | writes host files |
| `task land` / `work` / `gate` / `staging` as whole commands | run git and the stored gate command; the container runs the git, and only their database reads and writes become ops |
| `doctor --backup` to a caller-chosen path, `service *`, `blob cat` to a path | host filesystem writes |

`mount create` is refused even under `~/repos`; an explicit prefix rule can admit it later. A
critique of the plan forced the ingest row: the first draft had an `ingest.bytes` op, which moved
parsing of untrusted input onto the host.

### The baseline was no database access, not shared access

"The agent already had database access" was an accident, not a design: the container shared
`jkb.db` because nobody had measured that sharing corrupts it. So the exclusion table is judged
against *no* database access from the container, and every row in it is a place where a database
row or a request body would otherwise become host execution, parsing or file writes.

### The op enum is the allowlist

An op that is not a variant cannot be requested. Adding a variant is a code change reviewed against
the exclusion table. Porting was staged by command group, and unported commands refused in remote
mode. Scale at the start, measured: `jkb-cli` had 118 `db.read`/`write_txn` call sites across an
8,651-line `main.rs`, and its commands were closures over `&Connection` that cannot be sent as-is.

### Version skew is the normal state

The container's `jkb` and the host's are routinely built from different checkouts. So an unknown
op or an unknown request field is **refused**, never ignored; unknown response fields are ignored;
ops are added and never changed in meaning, and a changed meaning is a new op name. An error code a
client does not know decodes as `unknown` and is treated like `internal`, so a newer host can add
codes. A new op a host daemon has not been rebuilt for is answered `bad_request` ("unknown
variant").

### Errors carry a stable code

`no_such_topic`, `topic_conflict`, `no_such_group`, `queue_full`, `too_large`, `invalid`,
`not_found` (an item a read names does not exist), `unsupported` (a search route this backend does
not serve), `forbidden` (a write whose host-side effect this client may not cause),
`ack_beyond_end`, `corrupt_payload` (with `seq`, so a consumer can ack past it), `bad_request`,
`busy` (transient, retry: another writer held the database lock past the busy timeout, or, over
HTTP, the daemon is at a concurrency limit or the group already has a long-poll in progress),
`stale` (HTTP 409: a `task.edit` replace whose `expected` base no longer holds; nothing was written,
re-read and retry), `internal`, and `schema_newer`. Over HTTP two more: `unauthorized` (a missing or
stale token) and `unavailable` (the daemon cannot be reached, something other than the daemon
answered, or the daemon cannot open its database).

Under `--json`, every `jkb mq` verb except `subscribe` (whose stdout is its event stream) prints any
failure to stdout as `{"error":{"code":…,"message":…}}` as well as exiting 1: a refusal with its own
code, invalid input as `bad_request`, a local database that will not open as `schema_newer` or
`unavailable`, anything else as `internal`.

### The changelog names where a write came from

Every backend names the actor the changelog records its writes under: `cli` for the host's command
line, `serve` for the daemon's clients, `reap` for the reap service's compaction, so the audit trail
says where a change came from.

### The operations

The full set, as the wire carries it. A request is a JSON object tagged by `op`; the result is
tagged by `result`.

| op | fields | result |
|---|---|---|
| `mq.topic_create` | `topic`, `spec?` {`max_bytes`, `max_messages`, `default_ttl_ms`, `group_idle_ms`, `compact_every_ms`} | `created` {`created`} |
| `mq.send` | `topic`, `key`, `kind`, `payload`, `ttl_ms?`, `producer` | `sent` {`seq`} |
| `mq.group_create` | `topic`, `group`, `from_start?` | `created` {`created`} |
| `mq.poll` | `topic`, `group`, `max`, `after?` | `messages` {`messages`} |
| `mq.ack` | `topic`, `group`, `seq` | `position` {`position`} |
| `mq.group_delete` | `topic`, `group` | `group_deleted` {`deleted`}, `false` when there was no such group |
| `mq.compact` | `force?` | `compacted` {…counts} |
| `mq.inspect` | — | `topics` {`topics`} |
| `mq.tail` | `topic`, `limit` | `messages` {`messages`} |
| `notify.event` | `session`, `event` (`needed`\|`tool_finished`\|`user_acted`\|`turn_ended`\|`session_ended`), `tool?`, `message?`, `cwd?`, `owner?`, `instance?` (an `owner` is refused without it) | `notified` {`state`, `moved`, `effects`, `refusal?`, `sent`} |
| `notify.open_sessions` | — | `sessions` {`sessions`: [{`session`, `tool`, `owner`, `instance`, `updated_at`, `state`}]}; `state` is `awaiting_user` or `awaiting_tool`, derived by the daemon (empty from an older one), and a client reads it rather than deciding from `tool` |
| `notify.gone` | `session`, `owner`, `instance` (as `notify.open_sessions` reported them) | `notified` {…} |
| `session.started` | `session`, `source`, `pid?`, `instance?` (a `pid` is refused without it), `cwd?` | `session_start` {`was`: `unknown`\|`live`\|`ended`} |
| `session.ended` | `session`, `reason`, `pid?`, `instance?`; ends that process's hold only | `session_end` {`outcome`: `recorded`\|`already_ended`} |
| `session.gone` | `session`, `pid`, `instance` (as `session.list` reported them) | `session_gone` {`ended`} |
| `session.list` | `all?`, `after?` (a page's `next`, sent back as it came) | `claude_sessions` {`sessions`: [{`session`, `pid`, `instance`, `cwd`, `started_at?`, `start_source?`, `seen_at`, `ended_at?`, `end_reason?`}], `next?`} |
| `session.state` | `session` | `session_is` {`state`: `live`\|`ended`\|`unknown`} |
| `kb.ambient` | `cwd`, `home?` | `ambient` {`namespace`} |
| `kb.query` | `dsl`, `default_scope?`, `limit?`, `count?`, `order?` (`id`\|`updated_desc`) | `items` {`items`}, or with `count` `count` {`count`} |
| `kb.ls` | `path?`, `all?`, `recursive?` | `listing` {`rows`: [{`parent`, `child`}]} |
| `kb.tree` | `path?`, `all?`, `depth?` (≤ 48) | `tree` {`nodes`: [{`child`, `children`}]} |
| `kb.cat` | `uid` | `content` {`content`} |
| `kb.grep` | `pattern` (non-empty), `scope?`, `ignore_case?`, `mode?` (`lines`\|`names`\|`count`) | `grep_hits` {`hits`: [{`uid`, `kind`, `lines`: [{`line`, `text`}]}], `count`, `truncated`} |
| `kb.search` | `dsl`, `default_scope?`, `route` (`vector`\|`fts`\|`hybrid`), `limit` (≤ 1000), `context?` (≤ 50) | `search_hits` {`hits`} |
| `kb.context` | `item` (an id), `n` (≤ 50) | `context` {`chunks` [{`item`, `position`, `is_hit`, `content`}], `truncated`}; no text is embedded |
| `kb.related` | `uid`, `edges?`, `depth` (≤ 16), `direction?` (`out`\|`in`\|`both`) | `related` {`rows` [{`uid`, `kind`, `status?`, `resolution?`, `depth`, `via`, `direction`, `snippet?`}], `truncated`, `at_node_cap`}; the walk stops at 1000 items |
| `kb.blobs` | `contains?` (non-empty), `limit` (≤ 10 000) | `blobs` {`blobs` [{`hash`, `size`, `mime?`, `created_at`}], `truncated`} |
| `kb.blob` | `prefix` (4–64 hex digits, unique) | `blob` {`hash`, `text`}; a blob that is not UTF-8, or over 8 MiB, is refused, and `jkb blob cat` on the host reads any blob in-process |
| `kb.history` | `path` (absolute, the client's), `home?` (re-rooted as `kb.ambient` does) | `versions` {`uri`, `versions` [{`ts`, `blob`, `status`}], `truncated`} |
| `kb.health` | — | `health` {`schema_version`, `fts_ok`, `flagged` (≤ 200 {`uri`, `status`, `detail?`}), `flagged_count`, `vector_tables`, `stale_vectors`}; run on the writer, since FTS5's integrity check is an `INSERT` |
| `task.ready` | `dsl`, `default_scope?`, `limit?` | `items` {`items`} |
| `task.show` | `uid` (a uid or bare slug) | `task` {`task`: {`item`, `transitions` (the last 5), `subtasks`}} |
| `task.subtasks` | `uid`, `all?` | `children` {`children`} |
| `task.why` | `uid` | `history` {`entries`} (budgeted) |
| `task.add` | `text`, `home?`, `under?`, `backlog?`, `global_backlog?`, `sync?`, `managed?`, `cwd?`, `client_home?`, `literal?` (the text is the title, no modifiers read), `priority?`, `due?`, `also?` (a reference placement) | `added` {`id`, `uid`, `home`, `binding`}, or `needs_global_backlog_assent` |
| `task.set` | `uid`, `status?`, `priority?`, `due?` | `applied` |
| `task.edit` | `uid` (any item's), `text`, `append?`, `expected?` | `edited` {`file_backed`}; the result is capped at 256 KiB for a task, and for any item a client of `jkb serve` edits; `expected` is a replace's base, refused as `stale` when the content is no longer exactly that, and refused (`invalid`) together with `append` |
| `task.tag` | `uid`, `facet_value`, `mode` (`add`\|`set`\|`rm`) | `applied` |
| `task.depend` / `task.undepend` | `uid`, `dep` | `applied` |
| `task.place` | `uid`, `ns`, `home?` | `applied` |
| `task.move` | `uid`, `under` | `task_moved` {`moved`, `from?`, `under`} |
| `task.unplace` | `uid`, `ns` | `unplaced` {`removed`} |
| `task.bind` | `uid`, `sync?` (`managed:` when absent) | `applied` |
| `task.claim` | `uid`, `owner` (≤ 512 bytes) | `claimed` {`acquired`, `refusal`} |
| `task.release` | `uid`, `owner` (≤ 512 bytes) | `released` {`released`} |
| `task.facts` | `uid` | `task_state` {`uid`, `status`, `tags` (facet → values), `claim?`, `land_target?`, `start_refusal?` (for a finished task only), `terminal`, `open_subtasks`, `writable` (`false` for a task filed outside the daemon's file roots, asked before git work)} |
| `task.by_branch` | `repo` | `branch_tasks` {`tasks`: branch → [{`uid`, `status`, `onto?`}], every task on the branch, in id order} |
| `task.start` | `uid`, exactly one of `take` {`owner`, `displace?`} and `keep` (the claim kept), `place` {`branch`, `repo`, `onto?`} | `taken` {`taken`}; `false`, nothing written, when the claim is not the one judged |
| `task.take` | `uid`, `take` {`owner`, `displace?`}, `place` {`branch`, `repo`, `onto`}; the place is judged (written for trial and rolled back), not recorded | `taken` {`taken`} |
| `task.locate` | `uid`, `owner`, `place`; recorded only while `owner` holds the claim | `taken` {`taken`} |
| `task.abandon` | `uid`, `observed?` (the claim read before the git work) | `abandoned` {`released`, `reopened`, `status`}; `released` is false only when someone else holds the task |
| `task.land` | `uid`, `landed` {`branch`, `onto`, `head?`} | `landing` {`moved`, `refusal?`, `status`}; the facts the caller established (graft, green gate, disposal) are stated |
| `task.landed` | `uid`, `landed` | `landing`; `observed_landed`: a guard's refusal is still recorded, an event the task's state does not define is not |
| `task.review_findings` | `namespaces` (≤ 64; a client asks in pieces) | `review_findings` {`total`, `open_count`, `open_must_fix` (≤ 100 {`uid`, `title` (≤ 200 chars)})}; refused past 10 000 tasks examined |
| `task.review_file` | `run` {`reviewers`, `returned`, `error?`} (refused unless no error and every reviewer came back), `ns` (must hold nothing; no `tasks` mount may cover it), `findings` (≤ 1000 [{`severity` (`must-fix`\|`concern`\|`nit`), `summary` (≤ 2 KiB), `file?`, `line?`, `scenario?`, `fix?` (≤ 32 KiB each)}], ≤ 768 KiB serialized) | `review_filed` {`ns`, `uids`, `clean`} |
| `task.review_record` | `repo`, `branch`, `sha?` (letters and digits, ≤ 64), `findings` (at least one item) | `review_recorded` {`recorded` [{`uid`, `moved_to_review`}], `skipped_unlanded`, `unusable`, `unwritable`}; one transaction |
| `task.claims` | `after?` (a page's `next`) | `claims` {`claims` [{`uid`, `owner`}] (≤ 1000 a page, task order), `next?`} |
| `task.reclaim` | `dead` (≤ 1000 owners the client proved gone) | `reclaimed` {`cleared`, `refused` [{`owner`, `reason`}], `unwritable`} |
| `task.staging` | `repo`, `all?` | `staging_tasks` {`tasks` [{`uid`, `title`, `status`, `tags`, `land_target`, `open_subtasks`}], `truncated`}; `staging ls` asks git the rest |
| `task.pr_facts` | `uid` | `pr_facts` {`uid`, `pr?`, `branch?`, `live_landing`, `superseded?`, `resumed_at?`, `writable`} |
| `task.open_in_repo` | `repo` (≤ 255 bytes) | `uids` {`uids`, `truncated`}; the repo's unfinished tasks |
| `task.pr_record` | `uid`, `number` (> 0) | `applied`; a `note` in the task's history |
| `task.close_merged` | `uid`, `merged` (`yes`\|`no`\|`unknown`, the client's `gh` answer), `observed` {`live_landing`, `resumed_at?`, `pr?`}, `pr?`, `dry_run?` | `closed` {`refusal?`}; `observed_landed`, judged in the op's transaction, held if the history no longer matches `observed` |
| `item.show` | `uid`, `preview?` (characters; by kind when absent, ≤ 1 000 000) | `item` {`item` {`uid`, `kind`, `status?`, `resolution?`, `priority?`, `due?`, `mime?`, `binding?`, `namespace?`, `content_chars`, `content_hash?`, `created_at`, `updated_at`, `tags`, `preview`, `preview_truncated`}} |
| `item.rm` | `uid` (full), `force?` | `item_removed` {`uid`, `kind`, `placements`, `edges`, `tags`} |
| `inv.read` | `read`: `ls` \| `type` {`ns`} \| `frontier` {`ns`, `all?`, `limit?`} \| `core` {`ns`} \| `tombstones` {`ns`} \| `retread` {`uid`, `depth` (≤ 16)} \| `evidence` {`uid`} \| `digest` {`ns`} | `inv` {`answer`}; budgeted; `retread` and `evidence` stop at 1000 (`at_node_cap`); units carry a 100-character snippet, never their body |
| `inv.write` | `write`: `new` \| `digest` \| `rollup` \| `do` \| `add` \| `link` \| `promise` \| `resolve` \| `reopen` \| `stale` (at most 64 edges and 64 tags) | `inv` {`answer`} |
| `ns.list` | `scope?` | `namespaces` {`paths`, `truncated`} |
| `ns.mv` | `from`, `to` | `moved` {`count`}; for a client, refused for a reserved root or anything under `_sys`, past 1000 items, 1000 namespaces or 64 filed tasks unless every mount and item involved is inside the file roots, and when a task's line would not come back |
| `view.list` | — | `views` {`views` [{`name`, `query`}], `truncated`} |
| `view.run` | `name`, `limit?` | `items` {`items`, `truncated`} |
| `repo.gate` | `repo` | `gate` {`gate?`}; read-only: no op stores a gate |
| `removal.add` | `removal` {`worktree`, `repo_root`, `branch`, `uid`, `delete_branch`, `accept_dirty`, `recorded_at`, `head?`, `archive?`, `archived_at?`} | `removal_added` {`id`} |
| `removal.list` | `after?` | `removals` {`records`, `next?`}; 64 a page, oldest first, each with `id` and `written_via` |
| `removal.archived` | `id`, `archive`, `at` | `changed` {`changed`}; only a pending record moves |
| `removal.cancel` | `ids` (≤ 256) | `removals_cancelled` {`cancelled`, `sweep_holder?`} |
| `removal.drop` | `id` | `changed` {`changed`} |
| `lease.get` | `name` (`removal-sweep`, or `land:<repo>`) | `lease` {`lease?` {`holder`, `taken_at`}} |
| `lease.take` | `name`, `holder` (`<owner> <nonce>`), `displace?` | `changed` {`changed`}; free, or still held by exactly `displace` |
| `lease.release` | `name`, `holder` | `changed` {`changed`}; only the holder's own |
| `lease.break` | `name` | `lease_broken` {`holder?`}; refused to a client of `jkb serve` |
| `ingest.text` | `text`, `mime` (≤ 255 bytes), `namespace` | `ingested` {`document`, `namespace`, `chunk_count`, `embedded`, `already_ingested`, `warnings`} |

Every listing answer (`items`, `listing`, `tree`, `children`, `search_hits`, `task`, `history`) and
`grep_hits` carries `truncated: true` when the read was cut. `jkb task show --json` gained a
`subtasks` array (`uid`, `title`, `status`) with this op, so a cut it reports refers to something
in the document.

### The agent read set has one implementation, served identically

`crates/jkb-api/src/kb.rs` holds each read once: the host CLI serves `jkb query`, `find`, `recent`,
`search`, `ls`, `tree`, `grep`, `cat` and `jkb task next`/`show`/`subtasks` through a
`LocalBackend` too (`crates/jkb-cli/src/ops_cli.rs`), so the daemon cannot answer one of them
differently from the host. Pinned byte for byte by `tests/cli.rs`
`the_read_set_answers_through_the_daemon_exactly_as_on_the_host` (17 reads). The CLI only renders,
and its `--json` shapes are unchanged, because the explorer parses them (the code-factory design:
the UI is a CLI client). Self-review of this port found serde_json parsing floats one ulp off
(59,200 of 400,000 random values), fixed by the workspace `float_roundtrip` feature and a test.

### An unscoped read's scope comes from the client's directory, re-rooted

`kb.ambient` takes the client's `cwd` and `$HOME`; a `cwd` under that home is looked up under the
serving process's home. That is what makes the container's `/home/vscode/repos/jkb` find the mount
the host recorded as `/Users/<u>/repos/jkb` (the container binds `~/repos` at `~/repos`). In one
process the homes are equal and nothing changes. The path is compared against the mounts table and
never opened. Verified by disabling the re-rooting: the daemon's answers then differ from the
host's. Residual: a container directory under its home that is *not* bound from the host re-roots
onto a host path that may be a mount, and scopes to that namespace: wrong scoping, no content moved.

### The daemon serves only the keyword search route

Vector and hybrid search embed the query text, which would have the host call a model for the
container. So a backend with no embedder, which is what `jkb serve` builds, refuses them with
`unsupported`, and remote mode's `jkb search` and the MCP server's `search` tool default to the FTS
route (the host keeps `hybrid`). Embedding a query where the daemon runs is a follow-up.

### The task-mutate set runs through the same ops on the host

`task.add`/`set`/`edit`/`tag`/`depend`/`undepend`/`place`/`unplace`/`bind`/`claim`/`release` and
the read `task.why` are in `crates/jkb-api/src/tasks.rs`, one implementation each, which the host
CLI runs through a `LocalBackend` too (`crates/jkb-cli/src/task_cli.rs`). Only what cannot happen on
the serving side stays in the client: reading stdin (`task edit --stdin`), asking the terminal, and
choosing the claim owner (this process's `host:pid` or agent id).

The terminal question is the op's to raise: `task add --backlog` outside any repo answers
`needs_global_backlog_assent` only once everything else about the request has validated (it runs
the whole create and rolls it back), and the client asks and sends it again with `global_backlog`.
The rule for what counts as an explicit placement is not copied into the client. Whether a task is
file-backed is decided by its binding, never by how the caller spelled its uid. The branch and repo
facet writers moved into `jkb-core` (`location.rs`) for this, with the ref-name check the CLI's git
calls share, and so did the lookup of the `tasks.md` covering a home (`mount::tasks_file_for`),
which `jkb-sync` now calls too.

### A write that would have the host's sync write outside the container's view is refused

A task bound to a file is written back to it by the host's `jkb sync --watch`. So a backend given
`tasks::FileRoots` refuses, with `forbidden`, every write to a task whose binding, or whose uid, is
a file outside the roots; creating a task filed in one (`--managed` is served); and binding a task
to a file at all. The uid matters because a task taken out of its file is rebound `managed:` but
keeps its `file://` uid, and sync re-attaches it by that uid when the line comes back. Only a file
binding makes sync write: it gathers a file's items by binding, so a managed task placed under a
tasks mount's namespace is not written into its file.

`jkb serve` gives its clients `$HOME/repos` (`jkb_daemon::CLIENT_FILE_ROOT`, which
`.container/check-config.sh` holds to the container's `${localEnv:HOME}/repos` bind), because a
sync write there is one the container could make itself; the host CLI's backend has no roots. A path
is judged by its components without touching the filesystem (`..` or `.` is outside; only a
trailing `#<id>` is a fragment). That judges the path's *spelling*, which cannot see a symlink, and
the container can plant one inside `~/repos` at a bound `tasks.md` or a directory above it. A second
review caught it, and the fix is in sync itself, which no longer follows a link on any bound file's
path (the file-sync design). An earlier version said links were no risk because bindings come only
from host-made mounts: true of the binding, false of the directory it names.

The refusal test is driven from a one-sample-per-op list, so a task write added later without the
guard fails it. Pinned by `a_rooted_backend_refuses_every_write_to_a_task_filed_outside_its_roots`,
`every_task_write_a_client_can_send_is_refused_for_a_task_filed_outside_the_roots` and, through a
real daemon, `the_task_writes_go_through_the_daemon_and_stop_at_the_container_s_view`.

### What a request can make the writer do is bounded

A namespace path is at most 4096 bytes and 128 segments (`ns::MAX_PATH_BYTES`/`MAX_DEPTH`, in
`normalize`, so every entry point has it): `ensure` writes a row per ancestor, and a megabyte
`a/a/…` in a request body was half a million rows of rising length in one transaction on the
writer. It is checked again after NFC, which can lengthen a path, so a stored path stays nameable. A
quick-add line carries at most 64 `+ns`/`#tag`/`^dep` modifiers, a task body at most 256 KiB after
an edit or append, a tag or due date at most 1024 bytes, a claim owner at most 512 (it is stored on
every transition). Tags and due dates are bounded in the core writers every path shares
(`tag::MAX_TAG_BYTES`, `task::MAX_DUE_BYTES`), so `tag rm` can always remove one, and `ns mv` checks
every path it would write. `task.why` is charged to the read budget like every listing.

### Every task write holds the task's `tasks.md` line to the file's round trip

`jkb_sync::filed_task_problem`, run by `LocalBackend` after the op, inside its transaction,
assembles the line as an export would (real local id, text, status, priority, due date, tags,
out-of-file placements, in-file dependencies), renders it alone and parses it back. A write that
makes a readable line come back different is refused, naming the first field that fails on its own.
Checking only the text let `task set --due "2026-07-15 17:00"`, a tag value or a namespace with a
space, and `task bind --sync …#Fix_Login` through, and the next import from the file cleared the
field and rewrote the title.

A line that was already unreadable does not block later writes: writers outside the typed
operations do not ask the file (`jkb ns mv` on the host, `jkb tag rename`), and refusing every write
after one of them left the task unable to be released. The exception is a write that moves the task
to another line (`task.bind`), which is judged as a new line: excused by the old line's problem, a
bind from an unreadable line onto another task's `#id` put two tasks on one line, and the next
export dropped one. Whether a task is in a tasks file is decided by the serializer owning its
binding (`binding::serializer_for`: a `#<local id>` binding goes to its mount, since only a
multi-item serializer makes one; a whole-file uri to the sync journal row the engine wrote for it),
never by a `#` in its uri.

`task edit` and `jkb item edit` share one edit rule (`item::edit_content`), which refuses a text the
serializer would not read back as written (`jkb_sync::task_content_problem`): a blank or
whitespace-only line, a checkbox line in the body, trailing `^id`/`@due`/`#tag`/`+ns`/`!p` tokens,
or what the parser normalizes (quotes, runs of spaces or tabs, spaces at the title's ends and a body
line's start); the refusal names which. The size cap is checked before that probe, which parses
inside the writer's transaction, and the parse is linear in duplicate lines (measured before the
fix: 16k duplicate lines took 10.7 s in release). The line check also exposed that the parser
rejected the Unicode ids `mint_id` makes, so each sync stamped another `^id`; the id alphabet and
its round trip are the file-sync design's.

### Ingest: the client parses, the host chunks

`jkb ingest <file|url>` reads and parses its source where it runs (`jkb_ingest::read_source`: a
file by its extension, a URL rendered in a headless browser) and sends only the extracted text as
`ingest.text` (`crates/jkb-api/src/ingest.rs`). In the dev container that means the container's
file, a page fetched through the container's egress firewall, and PDF/HTML parsers running on the
container's kernel. The host chunks the text (character windows, not a format parser), so the
chunking strategy stays in the ingestion's idempotency key and the body is half the size of sending
chunks, and captures it through the same `Pipeline::ingest` as ever. `namespace` in the answer is
where the document is: for one ingested before, where that ingest put it.

### An ingested text is addressed by what the host saw

From the host's own process the raw bytes travel with the request (`IngestAsk::raw`,
`#[serde(skip)]`, so never on the wire): the document is `b3:<hash of the bytes>` and the bytes are
its blob, exactly as a host ingest always was, so re-ingesting a file ingested before this op is
still a no-op. Through the daemon there are no bytes, so the document is addressed by
`jkb_ingest::text_address`, blake3 of the text in key-derivation mode under its own context, which
no plain hash of any bytes equals, and no blob is stored.

Not by a hash the client names, which would let it choose the uid its text is filed under. Not by
the text's plain hash (review 1): a UTF-8 file's text is its bytes, so a container sending a host
file's bytes as text took the uid and ingestion row the host's own ingest of that file resumes into,
leaving the host's document unparsed, under the container's namespace and mime, and "already
ingested" for good. Not by a prefix-and-text hash (review 2): a file whose bytes began with the
prefix matched it. The same file ingested from the container and from the host is two documents.

### No model call for a client, and ingests run one at a time

`jkb serve` has no embedder, so a container's ingest is captured, keyword-searchable at once, and
answered `embedded: false` with a warning; it gains vectors when the host runs `jkb index
--pending`. A repeat of the same text answers `already_ingested`; once the vectors exist (written by
`index --pending`, which keys no ingestion) a repeat marks the ingestion complete and answers
`embedded: true`. Nothing runs `index --pending` on its own yet.

A capture at the 1 MiB body cap holds the single writer for about half a second (measured by the
ingest port's second review: 457 ms release, Linux; two in flight left a hook write 841 ms of its
1 s). So ingests run one at a time under a budget of their own (`max_ingests`, 1) in place of an op
permit, refused `busy` past it, rather than queue ahead of the notification hook's writes.

### A client never stores a gate

`jkb task land` runs a check (tests, lint) on the merged result before recording the task done, and
resets the merge if it fails. The command is stored per repo (`namespaces.metadata.gate`, e.g.
`./scripts/check.sh`) so every later `land` of that repo runs the same one, and `land` stores it the
first time, from `--gate` or autodetected. The host runs that string with `sh -c`, as the user. If a
container could store it, a process in the container would choose a shell command the host later
runs outside the container: an escape through a database row. So a container reads the gate
(`repo.gate`) and runs it **in the container**; a `--gate` or detected gate given remotely is run
and never stored (`GateSource::*Unstored`). Storing stays a host command.

Residual: `task land` on the host still runs a gate stored during the shared-database era. For new
runs, `task land` prints a stored gate that differs from the autodetected one and requires `--gate`
to run it.

### Review findings are filed through an op, never through a mount

`jkb task review file --findings <ns> --from <json>` takes the reviewer workflow's own result
(`{findings:[{severity, file, line, summary, scenario, fix}], …}`), and `task.review_file` creates
one `managed:` task per finding under `<ns>/must-fix|concern|nit` (priority 1/2/3), or one `done`
"No findings — clean review" item. Refused when `<ns>` already holds items or a `tasks` mount covers
it, and, in the op, from the result's `reviewers`/`returned`/`error`, unless every reviewer the
workflow launched came back: a partly read change must not read as reviewed, and an empty result
from a review that did not run, filed as clean, would let `task land` pass. `jkb task review file`
trims the longest texts to fit one request.

`/jkb-review-log` used to write a `tasks.md`, `mount create` its folder and `jkb sync` it; from the
container that has the host read and write files at a path the container chose, the attack the
removal-records port spent five review rounds closing for one directory. It stops writing
`tasks.md`, `mount create` and `sync` in **both** modes, so the host and the container cannot file
findings differently. Cost: a findings file can no longer be ticked in an editor; status moves
through `jkb task set` and the explorer. Rejected: a container-created mount the host's watcher
syncs. The `.claude` command changes are a patch the user applies, since the agent cannot write
`.claude/`; `.claude/commands/review-log.md` still describes the mount until it is applied.

`task review record` is an op too (`task.review_record`): the client resolves the branch and SHA
with git, and the op does the rest (findings non-empty check, crediting, tagging, `in_progress` →
`needs_review`) in one transaction. Tasks the client may not write are skipped and named.

### Liveness is judged where the owner lives; the daemon only compares

`task reclaim` and `doctor` list the claims (`task.claims`), probe each owner where they run
(`owner::is_alive`), and send the owners they proved gone (`task.reclaim`). The op frees, in one
transaction and through `observed_owner_gone`, claims still held by exactly one of those strings.
The owner string is the compare-and-set: a resumed session or a new process claims under a different
string, so a probe made before the transaction cannot free a claim taken after it. A client is
refused an owner it cannot have proved: a process of the daemon's own host, a session checkout
outside `~/repos`, and (from anyone) an `agent:` or unreadable owner. A claim held by a live or
unestablished owner is refused by `task.claim` from anywhere. This adds no power a client lacked:
`task.release` already frees any claim whose owner string it names.

**But the daemon does not judge liveness.** A client that names an owner can drop it, and so can a
session op's `displace`. A misbehaving container can therefore free any claim, and an honest one
never does, because it can prove gone only what its own machine can see. Residual: a `host:pid`
owner carries no run, so a pid probed dead and reused by a new process that claims under the same
`host:pid` before the reclaim commits (milliseconds) is freed; the host's in-transaction re-probe
this replaced closed that window too. `/task-swarm` runs `jkb task reclaim`, not `doctor --fix`.

### `doctor` reads through an op; its fixes stay on the host

`kb.health` answers the database-side checks (FTS integrity, schema version, sync journal, stale
derived rows). The un-embedded count stays host-only, because which vector table it counts against
is the host embedder's. Claims are probed by the client as above. The old removal store and the
cloud-folder check are host facts, printed only on the host. `--fix` and `--backup` stay on the
host.

### `jkb mcp` is built over `Backend`

Every MCP tool is an op (`kb.context`, `view.list`, `view.run` and `task.add`'s `literal`/
`priority`/`due`/`also` were added for it), served in both modes. `ingest_path`/`ingest_url` read
the source where the server runs and send its text, as `jkb ingest` does, rather than being
refused. `search` defaults to FTS through the daemon. In remote mode `jkb mcp` uses only an
operator-configured role token (the agents-and-roles design).

### What stays on the host

`undo` (it reverts any transaction, the host's own included; limiting it to a client's own writes
is a separate design), `index` (it calls the host's model), `task mirror` (a sweep over every task),
`task reap`, storing a gate, breaking a lease (`task reap --break-lock`, `task land --break-lock`),
`doctor --fix`/`--backup`, `mount`, `sync`, `service` and `serve`. Still refused remotely but not
host-only by design: `view`, `tag`, and `ns mk`/`rm`/`type`.

## Remote mode

A process that must not open `jkb.db` (the dev container's `jkb`) reaches the same operations
through the daemon. The table of what runs remotely is `crates/jkb-cli/src/remote.rs`.

### `JKB_REMOTE` puts the CLI in remote mode, and it never builds a local `Db`

With `JKB_REMOTE=http://<host>:<port>`, or bare `<host>:<port>` read as `http://` (which is how the
dev container sets it), and `JKB_REMOTE_TOKEN_FILE` (default `~/.jkb/daemon/<port>/token` for that
URL's port), `jkb` runs `jkb mq …`, the agent read set, the task-mutate set, `jkb ingest`, the
session verbs, the review and reclaim verbs, `jkb doctor`'s report and `jkb mcp` through the
daemon, and the commands that need no database (`notify`, `guide`, `commands`) as usual.

### Everything else is refused at dispatch, before any side effect

A command whose database access has not been ported **refuses**, with a reason: host-only commands
(`sync`, `mount`, `service`, `serve`, `doctor --fix`) never go through the daemon, and the rest are
not ported yet. Never a silent local fallback: refusal is safe by construction, and a fallback is
the corruption this exists to prevent. The refusal happens before the command runs, because `task
work` runs `git worktree add` before its first database write, so discovering unported-ness at the
first `db.read` would refuse after git acted. The table is an exhaustive `match`, so a new
subcommand does not compile until it says which it is.

### `--db` and `JKB_DB` are refused in remote mode

A process configured with both names a database two ways at once, and silently obeying one hides the
other. So `--db` is refused, not ignored, and so is a non-empty `JKB_DB`.

### A daemon that cannot be reached is remembered for 5 seconds

`~/.cache/jkb/remote-unreachable-<port>`, keyed by port, so a burst of short-lived `jkb` processes
pays one connect timeout, not one each. An error body that is not the daemon's (a proxy's `502`,
say) is `unavailable`, and the token is re-read only when the daemon itself answered
`unauthorized`.

### The firewall opens the daemon's port and nothing else on the host

One `iptables` ACCEPT for the host gateway address on the daemon port only (`RULE_DAEMON` over its
own `jkb-daemon` set), not a domain in `allowedDomains`, whose `hash:net` ipset opens every port on
that address; the raise keeps any daemon address out of `allowed`, by address and by name. The
sandbox posture names the host alias in `network.allowedDomains` so the proxy tunnels to it.
`egress-status.sh` reports `daemon=port|unresolved|absent|wide`; `verify.sh` asserts the kernel
answer and `/v1/hello` with the bind's token; `check-config.sh` holds the port to the daemon's
default address. The container's firewall, its verdict and the mutation suites are the
sandbox-and-container design.

### The dev container is in remote mode

`containerEnv` sets `JKB_REMOTE` to `host.docker.internal:7117`, the firewall's one opening, and
names no database; the token default resolves through the `~/.jkb` bind, so no
`JKB_REMOTE_TOKEN_FILE` is set. The container-local knowledge base it had until then (`JKB_DB` on
the `jkb-kb-local` volume) is gone, and tasks created in it did not survive unless exported first.
Decided with the user (2026-09-15) to wait until nothing the container's agents use is refused,
the session verbs above all, since `jkb task work` makes the worktrees agents run in. What is still
refused and named by an agent command is `mount`/`sync`: `/jkb-review-log` files findings with
`task review file`, and `/next-task` and `/design-pass` leave a file-backed task's edit to the host's
sync watcher. Those command changes live under `.claude/`, which an agent cannot write, so they are
a patch the user applies, and the cutover was not to merge before it did (`jkb commands install`
embeds the same files, so a binary built without it hands out the refused workflow too).
`scripts/swarm-status.sh`'s file view reads the database directly and refuses in the container.

## The session verbs through ops

`task start`, `work`, `abandon`, `land`, `landed`, `gate` (show) and `sessions` run git and the
filesystem as well as the database, so they could not be sent whole. The port was approved by the
user on 2026-09-16 and built in four stages, each with its own review loop. The lifecycle rules
these verbs carry out (claims, worktrees, the land gate) are the task-lifecycle design; this section
is only how they cross the daemon.

### The client does the git; each database step is an op

Every session verb already did **git and filesystem work first and the lifecycle transition last**,
with the claim taken early and compensated. So the client, host CLI or container, does all git,
filesystem and process work itself, and each database step becomes an op (`task.start`, `take`,
`locate`, `abandon`, `land`, `landed`, `review_findings`, `facts`, `by_branch`, `repo.gate`,
`session.state`, `removal.*`, `lease.*`). The logic moved out of `jkb-cli` closures (`swap_claim`,
`claim_session`, `current_claim`, `judge_existing_claim`, `stored_gate`, `findings_in`,
`tasks_by_branch`, `resolve_task_uid`) into `jkb_api::sessions` and `jkb_api::removals`, one
implementation that the host runs over `LocalBackend`. The order is preserved:

- `work`: claim (CAS) → removal-record sweep → worktree add, compensated by `task.release` with that
  owner.
- `land`: lock → facts + git preflight → review gate → graft → gate locally (red → reset, nothing
  written) → waive if `--no-review` → dispose → `task.land` last → unlock.
- `abandon`: dispose → `task.abandon` (CAS on the observed owner).

`task.facts` reports `writable`, and `land`/`abandon` refuse before any git work when it is false.
`task.review_findings` is the land gate's one query, asked in chunks of 64 namespaces (the first
land review found it failing past 64 reviews). `task.by_branch` lists every task on a branch.

### A session owner names its worktree relative to the home, and carries its opener

The claim model keeps its rule (the agents-and-roles design): the *work* is alive while its worktree
is, whether or not the agent that opened it still runs, and there is no TTL. What changed is what
names it. A session owner is `session:<pid>[@<claude session>]:~/repos/…/.jkb/work/<slug>`. It names
a path, but relative to the home, and each side resolves it against its own home: the dev container
and the host share `~/repos` through the bind, and `kb.ambient` already maps a client's home onto
the server's the same way. Only `~/repos` is home-relative (round 3), because it is the shared
directory; `~/src` on each side is a different directory, where a `~/` owner would read as proven
gone. Absolute owners written before still parse and are judged as before. `host:<pid>` stays for a
plain process with no `CLAUDE_CODE_SESSION_ID` (a person at a terminal).

The opener rides in the owner (`<pid>@<claude session>`), not in a separate record, because the
claim is the one place every reader already looks. A resume keeps the opener unless the resumer is
itself a live registered session; otherwise a terminal or subagent resume cleared the protection.
`/clear` gives a new id and nothing links it to the old one: the old id is ended, so the new session
resumes the task like any other session.

### Another session is refused only when both are live and different

`jkb task work` from another session refuses only when both the opener and the asking session are
`live` in the session registry and are different sessions: two top-level sessions in one checkout.
`unknown` (no row, or no daemon), `ended`, a process with no session, and a subagent (its own id,
unknown to the registry) behave as before. Measured: `CLAUDE_CODE_CHILD_SESSION` is set in a
top-level session's shell too, so it cannot identify a subagent. The owners port's review found that
the first version refused subagents resuming their parent's work. The error names `jkb task release`
as the way out.

### `task work` judges its location with the claim and records it after the worktree

`task.take` carries the place and trial-writes it (savepoint, `tasks.md` line check, rollback), so
a refusal comes before any git work. `task.locate` records it afterwards, compare-and-set on the
run's owner, and notes the branch and land target. Round 1 found that a location refused after the
worktree stranded the claim. Round 2 found that writing it with the claim left a failed run pointing
the task at a branch nobody made. The take's history entry is unlabelled. The daemon does not judge
a `displace`: a client can already drop any claim with `task.release`.

A consequence: the session ops' writes pass the `tasks.md` round-trip check, so for a file-backed
task in a repo whose directory name contains a space, `task start`/`work` are now refused, because
the `repo=` value would break its line. They used to write it and break the line.

### Worktree-removal records live in the database and name only the shared directory

`archive::dispose` wrote `<db dir>/worktree-removals/*.json` naming full paths, and the host's reap
service renames, deletes and runs `git branch -D` on what they name. The danger is a record choosing
host paths. The records moved into `worktree_removals` (`V020`), and the sweep's lock into the
`leases` table. Every path a client writes, or names by id, must be `~/`-relative, of plain
segments, and under `~/repos` once resolved against the daemon's home
(`FileRoots::admits_home_path`). A client therefore points the sweep only at directories it could
change itself, and the reader's existing rules (`Record::parse`: a worktree under
`<repo>/.jkb/work`, an archive under `<repo>/.jkb/archive`; the identity check) still decide what
the sweep does. The host's own records stay absolute where they are outside `~/repos`. Ops:
`removal.add/list/archived/cancel/drop`, `lease.get/take/release/break`; lease names are a closed
set (`removal-sweep`, `land:<repo>`), and `lease.break` is refused to a client.

### The reader confines a row at the moment it acts

The path check is spelling only, and the container can plant links under `~/repos`, so the
**reader** confines too: the removals port's first review found a linked `.jkb/archive` having the
host sweep delete `~/Documents`. A row is **confined** unless the host's own CLI or reap service
wrote it *and* it names no `~/` path (a client can re-point a host row under `~/repos` with
`removal.archived`, so who wrote it proves nothing there). A confined row is acted on only if its
repo root, resolved at the moment of acting (`archive::beneath`), lies under `~/repos`; one that
resolves elsewhere is reported REFUSED and never touched. The sweep's rename and removal walk that
resolved path with `O_NOFOLLOW` (`jkb_core::nofollow::rename_into`, `remove_tree`), so a link
anywhere below the root is refused rather than followed. What remains is git: the sweep's `git
worktree remove`/`branch -D` run with the path as spelled, and a branch is deleted only when its tip
is the commit the record names. Clients read every record, the host's absolute paths included. A
disposal whose record the daemon refused says nothing will finish it.

### `task work` cancels a record by op, never by taking the sweep's lease

`task work` cancels a pending record with `removal.cancel`, one write that a sweep in flight
refuses. A container `task work` killed while holding the sweep's lease would have left a holder the
host cannot probe, and the host's reap service would skip every pass until someone broke it. Only
the host's legacy file records are cancelled under the lease. `removal.cancel` skips and names a
record the client may not name rather than refusing the batch, and `task work` then refuses to hand
the checkout back, because that record is still owed; it sends its ids in batches under the
daemon's cap, whose count the container could otherwise inflate. Residual: a client can take
`removal-sweep` and hold it, which stops the host's sweep **and every `task work`** (a cancel is
refused while the lease is held, even with nothing to cancel) and acts on nothing; `jkb task reap
--break-lock` on the host ends it.

### A worktree is unregistered by its own path, never with `git worktree prune`

Prune drops every registration whose directory this side cannot see, which across the bind is every
session opened on the other side. `gitrepo::forget_worktree` runs `git worktree remove` on a missing
directory, and `worktree_remove` no longer prunes after removing (both measured on git 2.51.1).

### `jkb task reap` opens the database per pass

It deliberately did not open it before, because the records were files. Now a newer-schema database
fails a pass, reported once while unchanged, not the service, and the one-shot form fails with the
reason.

### The land lock is a database lease

`.jkb/land.lock` held a pid, which the other kernel cannot probe, so each side would steal the
other's live lock. It is now the `land:<repo key>` lease, taken as a compare-and-set with holder
`<owner> <nonce> <Claude Code session or ->`. A holder is stale only when proven gone, asked in this
order: a process this host can probe decides (dead is stale, alive is not, whatever became of its
session: a land left running by a Claude Code that died is still grafting, which the first land
review found an ended session overriding); only a process this host cannot probe falls back to the
session it names, stale once that has ended in the registry. Anything else is respected until `jkb
task land --break-lock` on the host. Never by age. The repo key is a directory's basename, so two
repos of one name share a lease: they land one at a time, which costs only waiting. A client
holding a `land:<repo>` lease stops only landings in repos of that name.

## The message queue

Kafka-shaped topics, keys and consumer positions, stored in `jkb.db`. Built so that host-side
daemons (the macOS notifier first) can be told things by processes that must not touch them
directly, including processes in the dev container. Code: `crates/jkb-core/src/mq.rs` (the rules;
its module doc states each and tests it), `crates/jkb-api` (the ops), `crates/jkb-cli/src/mq_cli.rs`
(`jkb mq`). The user's direction for it was fixed from the start: a generic producer/consumer
interface, Kafka-style consumption (a position per consumer, never delete-on-read), a generic
subscribe API for future daemons, a log queue type first, and compaction in the existing reap
service.

### The model: topics, keys, kinds, groups

| term | meaning |
|---|---|
| **topic** | a named stream, e.g. `claude/notify`: the unit of size cap, default TTL and consumer groups. Few and long-lived, never one per entity. |
| **message** | immutable: a queue-assigned `seq`, a `key`, a `kind`, a JSON `payload`, `producer`, `enqueued_at`, optional `expires_at`. |
| **key** | what the message is about (`host/<h>/repo/<r>/session/<s>`): an opaque non-empty string ≤ 512 bytes, no NUL. The queue never filters on it. |
| **kind** | what consumers dispatch on (`notify.post`). |
| **consumer group** | a named reader of one topic with **one committed position**: everything with `seq <= position` is consumed by it. Every group is handed every message; a consumer skips what it does not want. |

*Topic = what kind of stream and how long it is kept; key = what a message is about.* The key is
pure Kafka (user, 2026-09-13): metadata for consumers and for a future compacted queue type, never a
server-side filter. Rejected: a topic hierarchy (`claude/notify/jkb/<session>` as its own topic),
thousands of short-lived topics each with its own spec and group state, where "everything about
this repo" is a multi-topic read with no order between the parts. The queue type is a closed enum,
`Log` only, so adding `Work` (competing consumers) or `Compacted` (newest per key) forces every
`match` to decide what it does with them.

### Order comes from `seq`, never from a clock

`seq` is `INTEGER PRIMARY KEY AUTOINCREMENT`, and SQLite admits one write transaction at a time and
holds its lock until commit, across every process on one kernel (the daemon, the host CLI,
`sync --watch`, `reap --watch`, `mcp`). So a later-committed message always has a higher `seq`, and
a cumulative ack is sound. Exercised by a **two-process** test. A topic's seqs have gaps (one
sequence serves every topic; reaping leaves holes; a rolled-back insert's seq is reused, measured on
SQLite 3.45.1), and are never reordered. `enqueued_at` is metadata stamped by the daemon's clock,
one clock, the host's, used only for TTL and reaping, so a container clock that drifts after a host
sleep touches nothing.

### Delivery is a committed position and a fetch position

`poll(group)` returns messages with `seq > position` in `seq` order. `ack(group, seq)` sets
`position = max(position, seq)` and refuses a `seq` above the topic's `MAX(seq)` (`ack_beyond_end`:
no pre-consuming the future). Delivery is at-least-once by default; consumers are idempotent. A
group created from now starts at `position = MAX(seq)`, from the start at 0.

`after` on `mq.poll` is the consumer's **fetch position**, separate from its committed one as in
Kafka: a consumer that has handed messages on but not yet acked them polls with `after` set to the
last seq it handed on. Without it, a batch of unacked messages comes back from every poll and
nothing past it is read.

### Expired messages are still delivered

A message past its TTL is delivered, flagged `expired: true`. A TTL never skips anything; it makes a
message *eligible* to be reaped. What to do with a stale message is the consumer's call (the
notifier shows it marked stale).

### A message is reaped only when it is consumed and either expired or over the cap

**consumed(m)** ⇔ the topic has at least one group, and every group's position ≥ `m.seq`; a topic
with no groups has nothing consumed. **reapable(m)** ⇔ consumed(m) **and** (m is expired **or** the
topic is at its size cap). A consumed, unexpired message stays while there is room. These are the
user's rules, decided 2026-09-13.

The size cap is 10 MiB or 10,000 messages per topic by default, set at creation. On `send`, if the
new message would exceed either, reapable messages are deleted oldest-`seq` first until it fits; if
it still does not, the send is **refused** with `queue_full`, because everything left is unread by
some group. Consequence, stated: a topic with no groups fills and then refuses. Proptests pin that
reaping never deletes an unconsumed message, that `queue_full` comes exactly when nothing is
reapable, and that ack is monotonic.

### Idle groups are removed after 7 days, deliberately by age

A group nobody has polled or acked for `group_idle_days` (7 by default, the user's choice), measured
from `max(last_poll_at, last_ack_at, created_at)`, is removed by compaction, so a consumer that
never returns cannot hold a topic full for ever. A consumer whose group was removed is told
`no_such_group`, recreates it from now, and accepts the gap. A group created from the start later
un-consumes every retained message and can push the topic to `queue_full`, intended and stated. This
is liveness by age, and deliberately so: dropping a group loses only unread messages, whereas
reclaiming a task claim by age (forbidden in the claim model) buries live work.

### A consumer that knows it is leaving removes its own group

`mq.group_delete`, rather than waiting out the idle period. `jkb mq group rm <topic> <group>` is the
same from the shell: it prints `removed group <g> from <t>` or `no such group <g> on <t>` (neither
is an error), and under `--json` the op's own result. It is how an operator clears a group a
consumer left behind, such as a crashed Code Factory's `code-factory` group on `claude/notify`.

### Compaction runs in the reap service

`jkb task reap --watch` calls compaction every pass, and compaction does nothing for a topic
compacted within its interval (`compact_every`, 3 days by default), so "every few days" is
compaction's own rule; `--force` ignores it. It deletes consumed ∧ expired messages and removes idle
groups. It opens the database on its own, so a schema the reap binary does not know stops only the
compaction, never the sweep. Its writes are recorded under the `reap` actor.

### A producer never creates a topic

`topic create` is idempotent for an identical spec and refuses a different one (`topic_conflict`),
so the first producer never races a creator over the spec. Payload ≤ 64 KiB and must parse back as
JSON; `send` refuses larger (`too_large`).

### Queue writes are not changelogged

Like `blobs` and `task_transitions`: the queue is transport, not knowledge, and `jkb undo` reaching
into it would replay or erase deliveries.

### The schema, and why running totals are computed per send

One migration:

```sql
CREATE TABLE mq_topics (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
  type TEXT NOT NULL CHECK (type IN ('log')),
  max_bytes INTEGER NOT NULL, max_messages INTEGER NOT NULL,
  default_ttl_secs INTEGER, group_idle_days INTEGER NOT NULL,
  created_at TEXT NOT NULL, compacted_at TEXT);
CREATE TABLE mq_messages (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  topic_id INTEGER NOT NULL REFERENCES mq_topics(id) ON DELETE CASCADE,
  key TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL,
  size INTEGER NOT NULL, producer TEXT NOT NULL,
  enqueued_at TEXT NOT NULL, expires_at TEXT);
CREATE INDEX mq_messages_topic_seq ON mq_messages(topic_id, seq, size);  -- covering for the cap SUM
CREATE TABLE mq_groups (
  topic_id INTEGER NOT NULL REFERENCES mq_topics(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  position INTEGER NOT NULL, created_at TEXT NOT NULL,
  last_poll_at TEXT, last_ack_at TEXT,
  PRIMARY KEY (topic_id, name));
```

Running totals for the cap are computed per send (`SUM(size)`, `COUNT(*)` over the covering index),
with no counter table, which would be a second copy of a fact. Measured (release build, WAL file
database in the container, 300 samples each): a send to a topic at a full 10,000-message cap whose
consumer keeps up (so each send reaps one) takes p50 0.44 ms / p99 0.64 ms, against 0.07 / 0.20 ms
for a near-empty topic; debug builds 1.3 / 2.0 ms. Negligible beside a hook's process spawn and HTTP
round trip, so the counter table stays out.

### An idle poll takes no write lock

An `mq.poll` with nothing past the fetch position, and its `last_poll_at` refreshed within the hour
(or a quarter of the topic's idle period, whichever is shorter), is answered by a read, so an idle
subscriber does not contend with every other writer several times a second.

### The queue's reads are bounded by batch size, refused rather than clamped

The queue's reads stay on the writer connection, so they are bounded by `mq::MAX_BATCH` (256): a
poll or tail asking for more is refused, and `jkb mq subscribe --batch` is held to 1–256 when
parsed, so neither reads more than about 16 MiB of payload however large a topic's creator, a
container included, let it grow. Refused, not clamped: the subscribe stream reads a batch shorter
than it asked for as caught up, and a clamp announced `caught_up` after every capped poll of a
backlog (a fifth review caught it). `mq.tail` shows a message whose stored payload does not parse
with its raw text and `"unreadable": true`, rather than stopping at it.

### `jkb mq subscribe` is the consumer stream

For a daemon in any language: run it as a child process and speak NDJSON.

```
jkb mq subscribe <topic> --group <name> [--from-start] [--at-most-once] [--batch N (1-256)] [--interval-ms N]
```

stdout, one event per line:

- `{"event":"message","message":{"seq":…,"key":…,"kind":…,"payload":…,"producer":…,"enqueued_at":…,"expires_at":…,"expired":…}}`
- `{"event":"unreadable","seq":…,"reason":…}`: a stored payload that does not parse, reported once.
  Ack its `seq` to move past it.
- `{"event":"caught_up","seq":…}`: the last poll returned less than a full batch, so everything
  available up to `seq` has been handed over. Emitted once when the stream first has nothing more,
  and again after each burst. A consumer that folds messages acts at this boundary rather than
  guessing with a timeout.
- `{"event":"error","code":…,"reason":…,"fatal":bool}`: `fatal: true` is followed by exit status 1,
  so a supervisor can tell a crash from a clean end.

**Consumers must ignore event types and fields they do not know.** Events and fields are only ever
added; a strict decoder breaks on the first new one.

stdin, one command per line: `{"ack":<seq>}` commits the group's position through `seq`. Anything
else, including a line that is not UTF-8, is answered with a non-fatal `bad_request` error event. A
failure to READ stdin is fatal (exit 1), never treated as EOF, since acks after it would silently
never apply.

### The stream's delivery semantics

The group is created if missing: from now, or from the start with `--from-start`; an existing group
keeps its position. Each message is emitted **once per run**; what was not acked when the run ends
is delivered again by the next run, so delivery is at-least-once and a consumer must be idempotent.
`--at-most-once` acks each message before emitting it, so a crash loses it instead.

**EOF on stdin ends the subscription**, after processing the acks that preceded it, waiting up to
10 s for an outage that holds one; an ack still not applied then ends the run with a fatal error
event naming its seq and the code of the refusal that held it (those messages come again next run),
never a silent exit 0. Keep stdin open for as long as you want messages. A closed stdout ends it
with status 0. A group removed while subscribed (it idled out) is recreated from now, with a
non-fatal `no_such_group` event saying messages in between were not delivered.

### Transient refusals are waited out; anything unhandled is fatal

At startup, and for every poll, ack and group recreation alike: `busy` (a locked database, a busy
daemon) is waited out silently; `unavailable` (the daemon is down or restarting), and in remote mode
`schema_newer` (`setup.sh` has not yet restarted the daemon, tens of seconds on every upgrade), with
**one non-fatal error event per outage**. An outage ends when a call succeeds, unless it began with
an ack that is still held, which only that ack succeeding ends (a poll, a read, can pass while the
ack, a write, is refused). The position is in the database, so the stream resumes where it stopped.
An ack sent during an outage is held and applied when the backend answers, including one sent before
the group could be created, which is never sent against a group that does not exist yet (that would
"recreate" it from now and lose `--from-start`). Under `--at-most-once` a message whose ack could not
be applied yet is not emitted. A group recreation the backend is too busy for is retried after the
poll interval, never straight away.

**In local mode `schema_newer` is fatal**, on a poll or an ack alike: the subscriber itself is older
than the database, and exiting is what lets its supervisor start the newer binary (the third daemon
review found a local subscriber waiting it out forever, the fourth the same rule missed on the ack
path).

So the rule became structural: **each call names the refusals it handles**, and everything else that
is not transient ends the stream with a fatal event. An ack handles `no_such_group` (recreate) and
the refusals about that ack alone (`ack_beyond_end`, `invalid`, `bad_request`, reported as
non-fatal); a poll handles `no_such_group` and `corrupt_payload`. A code added later is fatal until
someone decides otherwise, never silently taken for a harmless one. A non-zero exit with nothing on
stdout means the subscription never started (bad arguments, exit 2; or in local mode a database that
could not be opened, exit 1), and stderr says which; a refusal of the subscription itself
(`no_such_topic`) is a fatal event.

Pinned end to end through a real binary by `tests/cli.rs`
`mq_subscribe_speaks_ndjson_over_pipes_and_resumes_after_the_ack`, and in `mq_cli.rs`'s tests for
`caught_up`, `unreadable`, a group recreated mid-stream, a closed stdout, a stdin read failure, bad
commands, `--at-most-once`, and each transient path against a scripted backend.

### The queue's second API is the wire protocol, not a file format

The first design promised a documented on-disk format that native libraries could implement. The
storage is now private SQLite, so the second API is the HTTP protocol above: a native client (Swift
`URLSession`, say) speaks `POST /v1/op` with the queue ops and `wait_ms` long-polls. Same intent: a
daemon in any language, without shelling out. None is built; the notifier runs `jkb mq subscribe` as
a child.

## The session registry

The notification hook feeds a registry of Claude Code sessions (`claude_sessions`, `V019`;
`jkb_core::claude_session`), so that the session verbs can ask whether the session holding a claim
or a lock has ended. The hook sends `session.started` on `SessionStart` and `session.ended` on
`SessionEnd`, after that event's `notify.event`. `jkb notify sessions [--all]` prints it.

### What Claude Code does was measured, not read

On 2026-09-16, in the dev container, a logging hook on `SessionStart`/`SessionEnd` recorded
`$CLAUDE_CODE_SESSION_ID` and each payload (`~/.jkb/logs/session-events.log`) while the user ended
sessions by hand:

| action | events | session id |
|---|---|---|
| launch, no prompt sent | `SessionStart` `startup` | new |
| `/exit` | `SessionEnd` `prompt_input_exit` | — |
| `/clear` | `SessionEnd` `clear`, then `SessionStart` `clear`, same second | **new** |
| `claude --resume` (new process) | `SessionStart` `resume` | kept |
| `/resume` in a session | `SessionEnd` `resume` for the one left, `SessionStart` `resume` for the one entered | kept |
| compaction | `SessionStart` `compact`, no end | kept |
| closing the terminal tab | `SessionEnd` `other` | — |
| `kill -9` of `claude` | nothing | — |
| `docker restart` | nothing | — |

Also measured: `$CLAUDE_CODE_SESSION_ID` equalled the payload's `session_id` in every event,
including right after `/clear`; hooks edited into settings took effect in a running session; hooks
in the project's `.claude/settings.json` did not run for a session started outside the repository
(which first looked like a killed session that had never started). A first attempt to log the
`claude` pid from `sh -c '…$PPID…'` recorded a different pid for each event: that `$PPID` is the
shell Claude Code wraps a hook command in. The shim's single-command form, `bash <script>`, is what
makes its `$PPID` `claude`. Not measured: whether `other` also covers Ctrl+C/Ctrl+D exits, and
whether Claude Code lets two processes hold one session id at once.

So **a `SessionEnd` proves an end; its absence proves nothing**, and an ended session can come back
(`resume`). The session id is stable across compaction and resume, and a Bash command inside the
session can read it, which it could not when jkb's owners were designed around pids.

### A row is a process holding a session

Rows are keyed by (session, pid, instance). A session is **live** while any of its rows is live,
**ended** once every row has ended (a `SessionEnd`, or a sweep proved it gone), and **unknown** with
no rows (a session started outside the repo, before the hook shipped, or pruned). Only an ended
session is evidence: a killed process stays live until the next `SessionStart` in its instance
sweeps it, and one on a rebuilt container never can be. Unknown licenses nothing. Ended is not
final: a `resume`, or any later event from the process, makes its row live again, which is why every
takeover built on it must be a compare-and-set.

### One process's end ends only its own row

`claude --resume` can run an id in a second process while the first still runs it. The first
version kept one row per session, and its review showed that the later process's exit then recorded
as ended a session the earlier one was still running. `session.gone` also ends only a live row that
still names exactly the pid and instance that were probed, `notify.gone`'s rule for the same race.

A start does not end its process's other sessions, because one process can host several (the Agent
SDK), and a running session would then be recorded as ended. Round 2 of the review proposed exactly
that rule, and it was rejected on this ground; pinned by
`a_start_never_ends_another_session_of_the_same_process`. The cost, deliberate: a lost end after
`/clear` or `/resume` leaves the old session live until its process is gone, which means a `--force`
in that case.

### A pid needs its instance, and a pid-less row is never evidence

A pid is refused without the instance it belongs to, by the check the notification ops share. The
hook, with no instance to send, sends no pid; when only the registry refused the pair, inside
`notify.event`'s transaction, the notification was rolled back with it (round 3; pinned by
`a_hook_with_no_instance_sends_no_pid`). A pid-less process's own end is recorded, but every process
the hook could not name on one instance shares that one row, so a session with a pid-less row reads
**unknown** once nothing is live, never ended (pinned by
`a_pid_less_process_never_proves_a_session_ended`).

### A lost start is repaired by the next event

If `session.started` is lost (the daemon is busy or restarting), a resumed session would stay
recorded as ended while it ran. So every `notify.event` from a process makes its row live, except
the end's own, which must not revive what `session.ended` is about to end. A live row's `seen_at` is
rewritten at most hourly, so a tool call on a known session writes nothing. Pinned by
`a_notify_event_marks_its_process_running_except_at_the_end` and
`any_event_from_a_process_makes_it_live`.

### The starting session is never judged by its own sweep

Suppose it was killed and is now being resumed: its earlier process is provably dead, but ending
that row would record a running session as ended if this start's `session.started` had been lost. A
later sweep, from another session, ends the row. Pinned by
`the_sweep_never_judges_the_session_that_is_starting`.

### What is only shown is normalised; identity is refused when malformed

An unusual start source or end reason is recorded as `unknown`, and a long working directory is
cut, because a refused start is exactly the lost write above. A working directory is stored with
its control characters replaced, since each row is printed on one line. Identity (session, pid,
instance) is still refused when malformed, with the notification ops' own checks, shared rather
than copied (`identity_is_refused_and_the_rest_normalised`).

### A session can be judged only from its own instance

Host sessions are judged by the host's `SessionStart` sweep, container sessions by the container's;
the daemon probes nothing itself. A daemon-to-container request was considered: the only thing in
the container that could answer is a running session's hook, and if no session is running there,
nobody is waiting on the answer either, because the next session to start runs the sweep before it
could want the task. So the host judges host rows, the container judges its own, and neither
accepts the other's word.

### The `SessionEnd` budget is spent in order

`SessionEnd` hooks get 1.5 s (Claude Code's documentation), and the end sends two requests. The
second starts only within 300 ms of the hook's first line (`SESSION_END_SECOND_REQUEST`), because it
may itself take the full 1 s request deadline. That leaves 200 ms for the shim's and the binary's
start-up, a margin **assumed, not measured**. What is not sent is logged, whereas a hook killed at
the budget would log nothing; for a process that then exits, a later sweep proves the same end from
its pid. `SessionStart` starts nothing 1 s after the hook began, including inside the sweep's loops
(a page, a verdict), so a start plus both sweeps is bounded by about 2 s. Pinned by
`a_slow_first_request_leaves_the_rest_unsent` and `a_slow_sweep_step_stops_the_sweep_there`.

### Whole sessions are pruned after 90 days unseen

A session starting deletes every session none of whose rows has been seen for 90 days, except its
own. Whole sessions only: deleting one stale live row beside a recent ended one would turn a live
session into an ended one (round 2; pinned by
`a_start_prunes_whole_sessions_not_seen_within_the_prune_age`). Deleting a session makes it unknown,
which licenses nothing, the safe direction for a record nobody can prove anything about any more,
such as a rebuilt container's sessions. The registry is **not changelogged**, like
`notify_sessions`: it is observation, and `jkb undo` must not revive or end a session.

### The listing is paged

`session.list` returns at most 1000 rows (`claude_session::LIST_CAP`) and an opaque `next` cursor,
which the sweep follows within its time limit. Rows it can never judge (another container's, the
host's seen from a container, pid-less ones) would otherwise fill the single page and hide one it
could judge. The keyset is (`seen_at`, session, pid, instance); a row written between two pages
moves, so in the live order it may be returned twice, harmless because verdicts are compare-and-set,
and in the `--all` order it may be skipped, so that listing is not a snapshot. `jkb notify sessions`
keeps one copy of a repeated row. A page of 1000 is bounded at about 9 MiB (a working directory may
take 4 KiB, and JSON escaping can double that). Pinned by
`the_sweep_pages_past_rows_it_cannot_judge` and `the_listing_is_paged`.

### Ended is a hint, and a takeover confirms against what it can see

The daemon cannot tell a host client from a container client, so a misbehaving container could
report a host session's process ended; an honest one never does, because its verdict about a host
row is `Unknown`. That grants nothing a container could not already do with `task.release` and a
claim's owner string. Two containers given the same `--hostname` read as each other's earlier boot,
and each one's sweep ends the other's live rows until their next hook event revives them. So a
takeover confirms "ended" against something the taker can observe, the worktree above all, and is a
compare-and-set on the owner it read.

## Sticky permission notifications: the queue's first consumer

Claude Code's "needs your permission" notification, made sticky and self-clearing. The pieces, in
the order an event passes through them:

1. the hook shim `.claude/hooks/notify-sticky.sh`, which hands the payload to
2. `jkb notify hook` (`crates/jkb-cli/src/notify.rs`), a client of `jkb serve` that never opens a
   database, which sends what it observed (a `notify.event` for each lifecycle event, and the
   `session.*` requests of the registry) to
3. the daemon, where the lifecycle table (`crates/jkb-core/src/notify.rs`) runs against its record
   of the session (`notify_sessions`, `V018`) and sends posts and withdrawals on the `claude/notify`
   topic, consumed by
4. `jkb-notifier serve` (`macos/notifier/`, built and installed as the launchd agent
   `com.jkb.notifier` by `scripts/build-notifier.sh`), which displays them.

### A permission notification is sticky and owned by its session

A banner that hides after a few seconds is exactly wrong for "Claude needs your permission": the
session sits blocked until you happen to look. The table posts on `Notification` under an id derived
from `session_id`, and withdraws that id on `PostToolUse`/`UserPromptSubmit`/`Stop`/`SessionEnd`.
**The id comes from the session, so dismissing owns no pid and no window handle**, and parallel
worktree sessions cannot clear each other's. `PostToolUse` is the closest observable "permission was
given" (`PreToolUse` runs *before* the prompt), so granting a slow command clears on completion, not
on the click.

### The lifecycle is a table `jkb-fsm` can check, because four fix rounds made it worse

The feature worked; the way it was arrived at did not. Four review rounds of the shell
implementation produced **4 → 8 → 11 → 13** findings, and each round's fixes generated the next
round's findings: one owner for the install path, and the *other* copy of the same fact omitted
`.claude/`; behavioural tests that ran the hook with `sh`, where ubuntu's dash killed it and four
assertions passed vacuously; a marker consumed only on a successful withdraw, so an unwritable
marker meant *no* withdraw ever; two dismiss assertions that passed regardless of the rule they
named. The worst defect was found by neither reviewer nor author: a test fixture escaped its temp
directory, rewrote tracked sources, and ran the real `setup.sh`, surfaced only because the user saw
an unexpected npm prompt.

The common cause was that the rules lived as conditionals spread across a shell script, with no
artefact anyone could walk: "can this notification always come down?", "which events can move
it?", "is there a state nothing exits?" were each answered by re-reading the script. So the
lifecycle is a `jkb-fsm` machine (the task-lifecycle design owns the crate), and a test asserts
`machine.check()` finds no defects. Without that test the table would be just another shape of the
same conditionals.

### The object is one session's notification, not the notifier

Its whole life is: something needs your attention → it is on screen → you dealt with it → it is
gone. Deliberately out of scope: the notifier's readiness pipeline (built → signed → registered →
authorized → styled), a second, genuinely different lifecycle; folding it in would produce one
machine answering two unrelated questions, the shape that made the shell script unreadable.

### Whether a tool was named is a state, not a guard

| state | settled | meaning |
|---|---|---|
| `Absent` | yes (initial) | nothing withdrawable of ours is on screen |
| `AwaitingTool` | no | posted, and the prompt named the tool it is about |
| `AwaitingUser` | no | posted, and no tool is known: the idle prompt, or a message that could not be parsed |

"Did we parse a tool name?" decides *which events are meaningful*, not whether an event may proceed.
A tool finishing tells you nothing about an untooled notification, so there is no row for it. That
is the crate's own division: states determine what may happen, guards whether it may happen now.
Expressed as a guard it became `Fact::Unknown → deny`, which reads as "we could not tell" when we
know perfectly well the question does not apply. State is read from the record, never passed
alongside it (`Stateful::state`).

### `SessionGone` is the one reconciled event, and it must carry evidence

| event | kind | source |
|---|---|---|
| `Needed` | Applied | `Notification` hook |
| `ToolFinished` | Applied | `PostToolUse` |
| `UserActed` | Applied | `UserPromptSubmit` |
| `TurnEnded` | Applied | `Stop` |
| `SessionEnded` | Applied | `SessionEnd` |
| `SessionGone` | Reconciled | nobody asked; the owning session was found provably gone |

The crate refuses a reconciled event without a guard, since an unguarded reconciliation is a state
change with no evidence. That is the rule this feature got wrong by hand: a *killed* session left an
Alerts-style notification on screen that no later event could ever reach, because every other event
is scoped to a session id that no longer occurs.

### Facts are three-valued, and `Unknown` never collapses to a boolean

`tool_matches`: is the tool that just finished the one the prompt named? `No` on a concurrent call
was a shipped defect; `Unknown` cannot arise in `AwaitingTool` (both names are known) and is a
refusal if it ever does. `session_alive`: `No` only on positive evidence that the session is gone,
**never age**, the claim model's rule, because a paused-but-alive session must keep its
notification; `Unknown` refuses, so an orphan survives until death can be proved rather than being
withdrawn on suspicion. Do not close that gap with a TTL.

### The transition table

`Needed` from any state is one row whose destination is stated by the machine (`Dest::Stated`:
`AwaitingTool` when a tool is named, else `AwaitingUser`), with plan `[Post, Remember]`; three rows
would be `Defect::Nondeterministic`, and the shell really had three competing branches with nothing
able to say so.

```
Absent        --ToolFinished------------------->   Absent         (absorbed, no plan)
Absent        --UserActed---------------------->   Absent         (absorbed, no plan)
Absent        --TurnEnded----------------------->  Absent         plan: Withdraw
Absent        --SessionEnded------------------->   Absent         plan: Withdraw
AwaitingTool  --ToolFinished[tool_matches=Yes]-->  Absent         plan: Withdraw, Forget
AwaitingTool  --UserActed / TurnEnded / SessionEnded --> Absent   plan: Withdraw, Forget
AwaitingTool  --SessionGone[session_alive=No]-->   Absent         plan: Withdraw, Forget
AwaitingUser  --UserActed / TurnEnded / SessionEnded --> Absent   plan: Withdraw, Forget
AwaitingUser  --SessionGone[session_alive=No]-->   Absent         plan: Withdraw, Forget
```

`check()` failed on first run with `Defect::Unrepeatable` for `ToolFinished` and `UserActed` from
`Absent`: withdraw once and the next `PostToolUse` had no row. They are the two most frequent events
in the system, the hand-written design omitted both, and they are now bare self-loops the
destination absorbs at no cost. `ToolFinished` in `AwaitingUser` has no row, which is
`Outcome::Undefined`, a named refusal rather than a silent no-op. The other checks name past
findings too: `Wedged` (a state with no way back to rest, the killed-session orphan),
`Unrepeatable` (two `Stop`s in a row, a re-delivered hook), `UnreachableRemedy` (a refusal's advice
must name an event that applies).

### Effects are data, and the record and the sends are one transaction

`Post`, `Withdraw`, `Remember { tool }`, `Forget` are produced *with* the move, as one `Vec`, and
performed by the caller. A plan cannot be half-applied by forgetting a line, and half-applying was a
shipped defect (clearing the marker without a successful withdraw, later withdrawing without
clearing). Inside one `write_txn` in the daemon, a plan applies whole or not at all: a send the queue
refuses leaves the record as it was. The old ordering rule (screen effects before record effects,
stop at the first that cannot be carried out) existed because a subprocess and a marker file could
half-apply a plan; it is kept, and still walked by `plans_change_the_screen_before_the_record`,
because it costs nothing.

### `Needed` always posts; whether it can be displayed is the consumer's fact

Whether a post can be *displayed* (the bundle installed, authorized, alert style not `none`) is a
fact about the Mac, which a producer in a container cannot ask. So `Needed` always plans `[Post,
Remember]`, and `jkb-notifier serve` decides per batch: through the notification centre by id when
usable, else a plain `osascript` banner, on the host where `osascript` runs. **Behaviour change,
stated:** the banner fallback used to need nothing installed; it now needs the notifier agent
running, because the agent is what reads the queue. With no agent, nothing is shown.

### The dismiss events are not interchangeable

One assistant message routinely batches several tool calls, so a slow allowlisted one finishes while
another's permission prompt is still on screen, and a `PostToolUse` that withdrew on *any* tool left
the session blocked with nothing on screen, the state the hook exists to prevent. So the events are
split by how far each can be trusted: a **tool** event withdraws only when the finished tool is the
one the prompt named (read out of the notification message, `"…permission to use Bash"`, since the
payload names no tool and there is no contract behind that wording; a parse failure yields
`AwaitingUser`, the safe direction: late, never absent); a **user** event withdraws unconditionally;
and `Stop`/`SessionEnd` **sweep without consulting the record at all**. Residual: two calls to the
*same* tool, one allowlisted and one prompting, are indistinguishable, so the first to finish
withdraws; the sweep bounds it to the turn. Re-verified end to end against the real notifier: a
different tool finishing leaves a real notification up.

### The blind turn-end sweeps stay

`Absent --TurnEnded--> Absent` with a `Withdraw` looks like withdrawing something believed not to be
there. It is the one honest place to admit that the record can be wrong about the screen. The record
can no longer disagree with what was *sent*, but the screen is a consumer's, across a queue: a
consumer whose withdraw hit a notification centre that never answered has already acked it (after
10 s). An Alerts-style notification waits forever by design, so one withdrawal per turn bounds that
failure to the turn. The crate makes this expressible: a destination absorbs an event only when the
row has nothing to do, so a row with a plan is run rather than swallowed. The volume (a message every
turn of every session) is trivial.

### A topic nobody reads is not written to

A message no group consumes is never reapable, so a topic with no groups fills to its 10,000-message
cap and then refuses every hook call; on a machine with no notifier that is the topic's whole life.
So the machine still moves the record but sends only while `claude/notify` has a group. There are
two consumers: the notifier's `macos-notifier` group, created from now by its `jkb mq subscribe`, and
the Code Factory app's `code-factory` group, which feeds its needs-input dot while the app is open
(the code-factory design). The app takes its group off the topic when it stops reading (its last
window closing, and quit) with `mq.group_delete`: left there, its group held every later message
unreapable, and once the topic reached its cap `notify.event` was refused `queue_full`, so the
notifier showed nothing after an ordinary quit. When it still stays (a crash, a daemon that does not
answer in time, or one older than the op), `jkb mq group rm claude/notify code-factory` removes it.

Posts are `notify.post` (payload `id`, `session`, `title`, `subtitle`, `body`; TTL 12 h, since an
11-hour-old sticky prompt is still true) and withdrawals `notify.withdraw` (`id`, `session`; no TTL,
reaped under cap pressure once consumed), keyed `session/<id>`.

### The hook never opens a database, and has no local fallback

It runs after every tool call. Measured per tool call when the logic first moved into Rust: the bash
hook took 11 ms, `jkb` with no database 5.8 ms, `jkb` with a database read 103 ms (~110 ms later).
Worse, a database a newer migration locked this binary out of would stop every withdrawal.
`tests/cli.rs` `notify_needs_no_database` pins it, now with the daemon unreachable. So when `jkb
serve` is down, notifications stop and the hook stays silent. The hook runs even where remote mode
would refuse (`JKB_REMOTE` beside `JKB_DB`), since it opens no database.

### The hook is bounded and silent

200 ms to connect, 1 s per request (`RemoteBackend::with_deadlines`, pinned against a daemon that
accepts and never answers), nothing on stdout, and every failure appended to
`~/.jkb/logs/notify-hook.log`, moved aside to `.log.1` at 256 KiB so a daemon that stays down cannot
fill the disk. A hook must never block Claude or print to the transcript. The address is remote
mode's `JKB_REMOTE` if set (`.container/check-config.sh` reads that variable's name out of
`remote.rs` and holds the value to the firewall's opening), else `jkb serve`'s default loopback; the
token is `~/.jkb/daemon/<port>/token`. **Only a failed connect marks the daemon down** for the 5 s
other clients skip it: a request that connected and then outran the hook's 1 s reached a daemon
busy on a write lock, and marking that down made the next permission prompt give up untried.
Measured on 2026-09-15 from the Claude Bash sandbox in the container, through its proxy: a
`notify.open_sessions` round trip took 6 ms cold, then 2.1–2.4 ms (curl, 5 runs). Not measured: the
hook process's own start-up in the container.

### The sweep is the producer's, because only the producer can probe a pid

The daemon cannot probe a container's pid. At `SessionStart` the hook asks `notify.open_sessions`,
decides for each record whether its session is provably gone, and sends `notify.gone` for those;
then it does the same over the registry's live rows (`session.list`, `session.gone`), the same
function asked of each row's `pid` and `instance`. A record is gone when (a) it was written from
**this instance**, or both it and this process are the bare macOS host, and its owner pid is dead by
`kill(pid, 0)` (`EPERM` counts as alive), or (b) it was written from **another boot of this same
container**. The sweep starts no request once 1 s has passed. It was kept on the producer because
the instance rule alone covered only a container restart, not the common case of `claude` killed
with no `SessionEnd` while the container keeps running (a critique of the plan found the first draft
dropping it).

### An instance is host, boot and pid namespace

The instance is `host[#boot][/pidns]`: the hostname; in the container, the boot, the pid namespace
the entrypoint recorded in `JKB_NS_MARKER`; and the pid namespace the writing process is actually
in. A container keeps its hostname across `docker stop`/`start` but writes a new marker, and runs
one boot at a time, so a different boot on the same host is gone. **The process's own namespace is
what makes rule (a) sound:** a nested sandbox shares the hostname and the marker but not the pid
namespace (measured in the Bash sandbox: `pid:[4026532823]` against the marker's
`pid:[4026532556]`), so without it a `claude` started in a sandbox would have probed the outer
sessions' pids where they do not exist and withdrawn every live prompt in the container. Everything
else (the host seen from a container, another container, a nested sandbox of this boot, a record
with no owner) is `Unknown`, and `Unknown` never withdraws.

The macOS host's instance is the hostname alone, and macOS renames the host on a network change. Two
*bare* instances (no boot, no namespace) are therefore taken to be the same machine, and the pid is
probed. The assumption, stated: every bare instance is the machine `jkb serve` runs on, because its
clients are that host and its containers, and a container always records a boot and a namespace. A
second bare machine reaching the daemon would break it. An *empty* instance is not bare: it names
nothing, and is refused with a pid. Pinned in
`only_a_dead_pid_here_or_an_earlier_boot_of_this_container_is_gone`.

### `notify.gone` withdraws only while the record still names what was judged

It carries the owner and instance it judged, and the daemon withdraws only if the record still names
both. `claude --resume` keeps the session id and runs a new process, so a session resumed between
the sweep's read and its withdrawal would otherwise lose a live prompt. The owner alone was not
enough: after a container restart a resumed session can draw the same pid in the new boot. Pinned
by `the_sweep_spares_a_session_resumed_after_it_looked` and
`gone_withdraws_only_the_owner_that_was_probed`. Liveness evidence is the hook's parent pid, which is
Claude Code itself; the transcript survives the session that wrote it, so its presence proved
nothing.

### The shim hands over the payload and never `exec`s

`.claude/hooks/notify-sticky.sh` pipes its stdin to `jkb notify hook` and keeps the fail-open,
silent-stdout contract. It must not `exec`: that makes `jkb`'s exit status the hook's, and a
`PostToolUse` hook exiting non-zero **blocks the tool call**, so an older `jkb` without the
subcommand, which clap rejects with status 2, stopped a live session dead (observed, not
hypothesised). A stale installed `jkb` silently disables the feature, since the shim delegates to
the first `jkb` on `PATH`: `setup.sh` reinstalls it as step 1, and the post-merge trigger fires on
`crates/`, so a pull repairs it. The shim and `.claude/settings.json` are edited outside the
sandbox, which has `.claude/` read-only.

### The consumer folds each burst to its net effect

`jkb-notifier serve` is long-running under the macOS-only launchd agent `com.jkb.notifier`
(`KeepAlive`), pointing at the **bundle's** binary so `UNUserNotificationCenter` finds the bundle id,
and not inheriting the one-shot run loop's 10-second window. It spawns `jkb mq subscribe
claude/notify --group macos-notifier` and respawns it. It folds each burst to its net effect per
notification id before touching the notification centre, so a consumer that was down does not flash
every post/withdraw pair, and acks only after display, so a crash redelivers rather than loses.
Because the queue orders a post before the withdraw the machine emitted later, a post can no longer
be delivered after its withdrawal, and no tombstone table is needed.

A burst ends at `caught_up` **or at `unreadable`**: `jkb mq subscribe` reports an unreadable row once
and then sends nothing more until that seq is acked, so waiting for `caught_up` there deadlocked on
one corrupt row. The 10 s notification-centre deadline is armed before the first call, so a centre
that never answers even the settings read cannot wedge the consumer.

### A subscription run is over only when it has exited and its output has ended

A run is over only when its process has exited **and** its stdout and stderr have reached EOF, and
the next starts only once that run's batch is off the screen: keyed on the exit alone, a dead run's
last post could land after the next run withdrew it. Each run restarts once however many of its end
signals arrive. A failing subscription is retried with backoff (1 s doubling to 30 s, reset only
after a run that really started lasted a minute), and only the first failure (with what the child
wrote to stderr, or why it could not start) and the recovery are logged, so `notifier.log` does not
grow a line per retry for as long as a failure lasts.

### An expired post that survives the fold is shown, marked stale

Decided by the user (2026-09-13). A post that a later event for its session in the same batch
supersedes is dropped by the fold; one that survives is shown with its age ("14h ago"), and the
`SessionStart` sweep clears it if its session is dead.

### Payload text from the container reaches `osascript` on the host

So the Swift consumer's fallback banner uses the same escaping the Rust hook's `banner_script` had,
pinned through `osacompile`. Any process that can reach the port and read the token can post *and
withdraw* notifications, including hiding a live permission prompt; it can already write the
knowledge base, so this adds no reach, but the withdraw half is worth naming.

### The notifier is ours, and the withdraw half is what forced that

Nothing shipping on macOS both stays up and takes itself down: `osascript` cannot withdraw what it
posted, and `terminal-notifier` can but was last released in **2017** on `NSUserNotification`,
deprecated since macOS 11. So ~200 lines of Swift (`macos/notifier/`) call Apple's current
`UserNotifications` framework directly, for `removeDeliveredNotifications(withIdentifiers:)`, and
remain the macOS API surface (`post`/`remove`/`list`/`status`/`authorize`/`serve`). **Swift, not
Rust, to avoid a dependency rather than add one**: the Rust route is `objc2`, where every message
send is `unsafe`, and this workspace's one `unsafe_code` carve-out is spoken for. It links nothing
but system frameworks. Rejected: the `user-notify` crate (one maintainer, wraps the same `unsafe`,
and its README asserts an Apple developer account is required, which we measured to be false).

### Three facts about the bundle were measured, and each one silently breaks it

`UNUserNotificationCenter.current()` refuses a process whose bundle has no identifier, so the binary
must live in an `.app`. The bundle must be **ad-hoc signed** (`codesign -s -`; no Apple developer
account, which only costs distribution) and **registered with Launch Services**, or the framework
answers "Notifications are not allowed for this application" however it is signed. And the
authorization prompt **dies with the process that raised it**, so `authorize` waits 5 minutes: a
prompt that vanishes is recorded as a *denial* only System Settings can undo.
`scripts/build-notifier.sh` does all four steps; `scripts/setup.sh` runs it.

### Two things no installer can do are reported instead

The one-time Allow, and the sticky **Alerts** style (a per-app System Settings choice; `banner`
hides itself). `jkb-notifier status` reads both back, which terminal-notifier could not, so setup
says which is missing. `post` **refuses** when unauthorized rather than succeeding invisibly (macOS
accepts it, displays nothing and returns no error); `serve` asks the same question per batch and
falls back to the `osascript` banner. `NSUserNotificationAlertStyle` in `Info.plist` is
deliberately absent: it is the legacy key, ignored by the modern framework, also measured.

### Do not try to set the alert style in code

This was attempted and abandoned on purpose. The style lives in `com.apple.ncprefs`, which on macOS
26 is TCC-protected (unreadable without Full Disk Access) and stores it as an undocumented bitfield
in a file shared by **every** app's notification settings, so a wrong write breaks notifications
system-wide to save one click. The supported path is the System Settings pane, which `open
"x-apple.systempreferences:com.apple.Notifications-Settings.extension"` goes straight to.

### Focus suppresses these notifications, and that is not fixable here

`.timeSensitive` is requested, but `status` reports `time-sensitive=not-supported`: the capability
comes from `com.apple.developer.usernotifications.time-sensitive`, an ad-hoc bundle carries no
entitlements, and the system **downgrades silently rather than erroring**. Embedding the entitlement
anyway was measured and is strictly worse: an ad-hoc signature carrying a restricted entitlement is
rejected and the binary is **SIGKILLed on launch**. Honouring it needs a real signing identity. The
request is kept (it costs nothing and starts working the day this is signed), and the claim lives in
`status` where it is read back, never in a comment asserting a capability we do not have.

### Stickiness cannot be asserted, only the style can

`jkb-notifier list` reports *delivered* notifications, which includes one whose banner has hidden
and is resting in Notification Center, so no test can tell "still visible" from "already hidden";
no API exposes it. The live test asserts the round trip and reads the style back; that it stays on
screen was confirmed by looking. Worth knowing before writing a test that appears to prove more than
it does.

### Setup reports only what it checked

`scripts/setup.sh` creates the `claude/notify` topic on every platform, because a producer never
creates one. On macOS it reports (`report_notifier` in `scripts/lib.sh`) that the agent has a running
process (a PID from `launchctl list`, not merely "loaded", which a crash-looping agent also is) and
that the topic has the notifier's own group, `macos-notifier` (main.swift's default, which the
installed agent does not override). Any group used to do, and then a `code-factory` group left by
the app made a notifier whose subscription never joined read as `subscribed`
(`services.test.sh` pins the case). That is still deliberately not "notifications are shown": a
group outlives its consumer by 7 idle days, and a running notifier can have a failing subscription
(its `notifier.log` says). What it rules out is the two states setup once reported as healthy: an
authorized bundle with no agent, and an agent loaded but not running. A group list that cannot be
read is `undecided`, never "no group". `--no-service` leaves the agent off.

### The hooks are code, and `cargo test` never reaches them

`scripts/tests/notify-hook.test.sh` is their suite, run first by `check.sh` and by CI (bash and `jq`
only, so it passes on the Linux runner). It also checks the launchd agent `build-notifier.sh
--print-agent` writes, and on macOS, where `swiftc` exists, compiles `macos/notifier/main.swift` and
drives `serve --dry-run` (the fold, stale marking and acks) and the fallback banner's AppleScript
through `osacompile`. **Portable means portable:** every macOS-only tool it touches is `command
-v`-guarded, and the plist check goes through `scripts/build-notifier.sh --check`, which answers
*before* its own Darwin gate and reads the plist with `awk`, so the assertion is live on the Linux
runner rather than skipped. It once called `/usr/libexec/PlistBuddy` directly, whose `|| echo
MISSING` fallback turned *cannot read* into a wrong **value** and reddened CI on every push. The
machine's behavioural cases are Rust tests over the table; the shell suite keeps only what is
genuinely shell.

The live post-then-withdraw round trip, through the hook, the running `jkb serve` and the running
`com.jkb.notifier` agent to the real notification centre, is opt-in behind `JKB_HOOK_LIVE_TEST=1`,
for the same reason the ollama and Chrome smokes are `#[ignore]`d: it puts a real notification on
screen, and a gate that runs before every commit must not flash one. No stub can show that
withdrawing works; only the notification centre can.

### Notifications left by a rebuilt container cannot be proved gone

A *rebuilt* container gets a new hostname, so notifications its sessions left cannot be proved gone
from anywhere and stay until dismissed by hand. Two containers given the same `--hostname` read as
each other's earlier boot. `session_alive` is rarely provable in general: an orphan survives until
evidence exists, which is better than never being withdrawn and worse than a guarantee.

## Deferred and not built

Decided directions with nothing built yet. Each is tracked in this design's plan.

### A database service instead of typed operations: deferred, not rejected

Raised by the user after approval (2026-09-13): *if these were containerized services, both sides
would just connect to a database over TCP; is engineering typed operations overkill?* Decision: keep
the typed-operation model to start, **because it constrains what the container can run**, and
investigate the service approaches later.

What the question gets right: as a *security* argument the op allowlist protects less than it
appears to. The container shared `jkb.db` until this design, so it could already read and write
every row, and the concrete escalations found in review are host processes *acting on rows* (the
sync watcher following a `file://` mount anywhere, `task land` running a stored gate, synced writes
to bound files outside `~/repos`), which need host-side guards under **any** transport. Why typed
ops are still the safer start: they constrain *what can run*, not only which rows can be written;
arbitrary SQL from the container is a wider surface (`ATTACH`/`VACUUM INTO` write host files,
extension loading, lock-holding transactions), and closing it relies on getting an authorizer or a
server's configuration exactly right.

The alternatives, measured against this codebase on 2026-09-13 (45 files / 296 functions take a
`rusqlite::Connection`, 482 statement sites, FTS5 30 refs, `sqlite-vec` 52, `json_extract` 16,
`RETURNING` 8, `ON CONFLICT` 18, 16 refinery migrations, 201 in-memory test databases; and inside
Claude Code's sandbox there is no direct TCP or DNS, only the HTTP proxy, so a database wire
protocol cannot be spoken without tunnelling through `CONNECT`):

| approach | for | against |
|---|---|---|
| MySQL server | the familiar containerized-service shape | no `RETURNING`, different upsert, no FTS5-like ranking, weak vectors; a full storage/search/migration rewrite; raw TCP against the proxy; jkb stops being a local file |
| Postgres server | `RETURNING`, `ON CONFLICT`, recursive CTEs, JSONB, tsvector, pgvector, `LISTEN/NOTIFY` (native push for the queue) | still a rewrite of `jkb-core`/`jkb-index`/`jkb-search`, migrations and test infra; a host service; raw TCP needs a `CONNECT` tunnel, unmeasured through the proxy |
| sqld (libSQL server) | SQLite over **HTTP** (passes the proxy), interactive transactions, same dialect, FTS5 | `rusqlite` → async `libsql` at every call site; `sqlite-vec` → libSQL vectors or a server-loaded extension; new crates fetched on the host; must run natively on macOS (in a container on a bind it is the corruption measured above); unverified that it blocks `ATTACH`/`VACUUM INTO`/extension loading |
| rqlite, ws4sqlite, SQLite-over-HTTP | off the shelf | batch-only transactions; jkb's read-decide-write closures cannot be expressed |
| forward SQL through `jkb serve` | keeps `rusqlite`, `sqlite-vec`, refinery, no new dependencies; every command works from the container at once | 296 signatures move to a trait; a round trip per statement; a remote transaction holds the host write lock (needs a lease); security rests on SQLite's authorizer + `SQLITE_DBCONFIG_DEFENSIVE` + the host-side guards |

What would settle it: sqld running natively on macOS; an interactive transaction through the
sandbox proxy; FTS5 and a workable vector path; `ATTACH`, `VACUUM INTO` and extension loading
refused. If sqld passes, it is the least code to own; if it fails on vectors or the proxy, SQL
forwarding is the fallback. Either would retire most of remote mode and the per-command porting,
and neither removes the host-side row guards.

### More queue types and a native client

`Work` (competing consumers: claim, ack, dead-letter after N attempts, claims reclaimed by owner
existence, never age) and `Compacted` (newest message per key retained, so readers see current
state; the natural home for "what should be on screen"). A native Swift client speaking the HTTP
protocol in place of the `jkb mq subscribe` child process. An MCP surface for `mq`, so an agent can
send and tail without shelling out.

### Embedding what the container ingests, and its search queries

`jkb serve` calls no model, so `ingest.text` from the container is captured keyword-searchable only
until someone runs `jkb index --pending` on the host. To decide: whether a host service (the reap
service, or the watcher) runs it periodically, a host process calling the host's model on its own
schedule rather than at a client's request. Embedding search query text in the daemon waits on the
same decision; until then remote search is FTS only.

### `jkb doctor` checks the database itself

Today its only integrity check is the FTS index (`FtsIndexer::integrity_check`), so it reported no
issues after a container process opened the host's `jkb.db` through a `file:` URI (2026-09-13);
page-level corruption, the risk from exactly that, was ruled out only by a manual `PRAGMA
quick_check` on the host (`ok`). It should run `PRAGMA quick_check`.

### Values a `tasks.md` line cannot carry, written by paths that do not ask the file

The typed task ops refuse a write that would make a line unreadable, but `jkb task start`/`work`
record `repo=<dir name>` through `location::set_location_facets` (a directory with a space gives
`#repo=My App`, which ends the modifier run, so the next import rewrites the title and drops
`branch=`), and on the host `ns mv`/`tag rename` rewrite placements and facets. Two shapes to weigh:
the serializer quotes a modifier value with a space (`#repo="My App"`; `tokenize` is already
quote-aware), so every value round-trips and the check has less to refuse; or the core writers of
due dates, tags and placements consult the line rule.

### The container asks the Mac to compile the Swift notifier

Asked by the user (2026-09-14) after the notifier's consumer shipped uncompiled: the sandbox has no
`swiftc`, so every Swift change waits on a person at the Mac. Shape: the container sends
`build.request {target:"notifier", commit}` on a `host/build` topic (pure database work, so it stays
inside the op model and the daemon runs nothing); a host-side consumer (a launchd agent, like
`com.jkb.notifier`) runs a **fixed** action and sends `build.result {commit, ok, diagnostics}`, which
the container reads with `jkb mq subscribe`/`tail`. **Security is the design question, not the
plumbing:** a container-supplied source compiled on the host is host code from the container. So:
compile the **committed** tree at `commit` (an export the consumer makes), never the bind-mounted
working tree; no install, no `codesign`/`lsregister`/`launchctl`; never run the product except
`--dry-run` in a sandbox (`sandbox-exec` with no network and no writes outside the temp dir); a
closed target list and no flags from the message (`-load-plugin-*` and macro plugins are code
execution); diagnostics size-capped; one build at a time, rate-limited. Open: whether even
`--dry-run` is acceptable, or typecheck only.

## History

Superseded decisions, each with what replaced it and why. The session hand-off that records the
path from the file-spool draft to the host-owned database is
`openspec/changes/jkb-message-queue/HANDOFF.md` (not migrated).

### A file-spool message queue

The first queue design (two drafts, never implemented) was a spool directory under `~/.jkb`, one
JSON file per message written to `.tmp/` and `rename(2)`d into `log/`, because producers on two
kernels share the directory over virtiofs, where `flock`/`fcntl` locks are not reliably shared and a
torn append is undetectable. It had a ULID-shaped id, per-group processed-sets, a stored scope
filter per group, a 7-day retention that deleted messages whether or not any group had read them,
and a documented on-disk format with golden fixtures for native libraries. A spool rather than a
socket because `~/.jkb` was already bind-mounted read-write and a unix socket needed a new mount.
Replaced by tables in `jkb.db` once the sharing probe showed the container could not open the
database at all: with the host owning it, the queue could be transactional, ordered by `seq`, and
reaped by consumption rather than by age.

### A watermark computed from producer time

The spool's first draft advanced a consumer watermark from the newest id processed, `max(seen) −
skew_window`, which is to say from some producer's clock. Docker Desktop and colima VMs drift
minutes to hours after a host sleep, so one producer ahead of another would silently skip *every*
message of the one behind, not one late message, and no window value fixes it; the proptest as
written could only have passed by being weakened to the window. Found by critique before any code.
Replaced first by a processed-set, then by `seq` order under the database's write lock. The lesson:
every must-fix of that draft leaned on a clock or on age, and that pattern will be proposed again.

### Consumer liveness by the age of a file

The same draft replaced the producer's "can this be displayed" fact with "is a consumer's `.alive`
file fresh": liveness by age, which the claim model forbids, read through a cached bind mount against
a drifting clock. Its `No` arm sent the fallback banner as a two-minute message to the very consumer
just judged dead, undeliverable and unwithdrawable. Replaced by the consumer owning display: the
producer always posts.

### The notification hook ran the machine itself, with a marker file per session

Before the queue, `jkb notify hook` ran the table in the hook process and exec'd the notifier per
effect, with state in a small file per session under `$TMPDIR` (`JKB_NOTIFY_STATE` to override),
because per-notification state in the database would have cost 103 ms after every tool call against
5.8 ms without. `PostToolUse` fires after *every* tool call, so that path had to be a stat rather
than an exec. It worked only where the hook could reach the Mac's notification centre, and from the
dev container it posted nothing. The marker could also be orphaned: a container restart erased it
while the notification it named stayed on screen, unreachable by any sweep. Replaced by the daemon's
`notify_sessions` record, one HTTP round trip per event, transacted with the queue; the hook still
never opens a database, which is why the 103 ms measurement still holds as the reason.

### `notifier_usable` and `Banner` in the producer's machine

The machine once had a three-valued fact, `notifier_usable` (installed, authorized, alert style not
`none`, with `Unknown` behaving as `No`, because with style `None` the notifier posted, exited 0 and
reported success for a notification nobody could see), and a `Banner` effect for the fallback.
Replaced when the machine moved into the daemon: a producer in a container cannot ask about the
Mac, so `Needed` always posts and the consumer decides between the centre and a banner.

### A tombstone table in the consumer

The file-spool draft had the notifier keep, per notification id, the highest message id applied and
ignore an older post or withdraw, because a `Notification` hook and a batched `PostToolUse` hook run
concurrently and could land in either order. Replaced by the queue: events are applied in the order
the daemon receives them and their effects get strictly increasing `seq`, so a post can no longer be
delivered after the withdraw the machine emitted later.

### A server-side key filter, and `scanned_to`

An intermediate draft stored a scope filter per consumer group and then needed a `scanned_to` rule,
because a filtered group never passed unmatched messages and wedged the queue (a critique finding).
Replaced by pure Kafka keys (user, 2026-09-13): no server-side filter, every group is handed every
message, and the wedge cannot arise at all.

### Ordering credited to "one writer", then to `BEGIN IMMEDIATE`

The design first said order was strict because "there is one writer", false once host processes may
write directly. The next draft credited `BEGIN IMMEDIATE`, which only decides *when* the lock is
taken; a deferred transaction was reasoned through during the queue's implementation and cannot
reorder either. What makes it true is that SQLite admits one write transaction at a time and holds
its lock until commit; a two-process test exercises it.

### The host hook falling back to opening the database

The notification design on the queue had the host's hook use the daemon when reachable and fall
back to `LocalBackend` when it was down. That contradicted the hook's measured cost of a database
open and the pinned `notify_needs_no_database`, and was not built: with the daemon down, the hook
stays silent.

### One token file per home, then a variable per client

The daemon's token was first `~/.jkb/daemon/token`, then briefly beside the database; both are
replaced by `~/.jkb/daemon/<port>/token` (a host set up with `--db` wrote it where no client looked,
and a second daemon on another port overwrote the first's live token). Until the container moved
into remote mode the hook had its own address variable, `JKB_DAEMON_ADDR`; it is retired, and
`JKB_REMOTE` names the daemon for both.

### A container-local knowledge base

As an interim fix after the sharing probe, the container used its own database (`JKB_DB` on the
`jkb-kb-local` volume) while remote mode covered too little. Setting `JKB_REMOTE` container-wide
early was rejected, because remote mode refuses `JKB_DB` alongside it and refuses every unported
command, which would have taken `jkb task` away from the container's agents. Replaced at the cutover,
once nothing the agents use was refused remotely.

### Removal records and owners naming *(repo key, slug)*

The session-verbs design had removal records and session owners carry only identities, a repo key
and a slug resolved by each side inside its own fixed layout, so a record could point only at
something jkb itself laid out. As built, both name `~/repos/…` paths instead: no registry maps a repo
key to its root, and a verb run outside the repo would have had nothing to resolve against. The
safety moved to admitting only `~/repos` paths and confining at act time.

### Importing the old removal-record files

Records an older jkb wrote beside the database were first swept in place, then imported by every
sweep, then imported on request, and each version was found steerable or lossy over five review
rounds: `~/.jkb` is bind-mounted read-write, so the directory and its files are the dev container's
to replace (links, FIFOs, huge or countless files), and a pending removal written before the move
may name a checkout somebody has since gone back to, which an import would then archive. Replaced by
reporting only: `task reap` and `doctor` list the files by name (a bounded look that never opens one;
a store directory that is a link is not looked into), ask the operator to judge each checkout and
remove the file by hand, and read no lock file there.

### Reads classed by "does not write", and a poll clamp

The daemon's second review introduced `Request::is_read`, every op that does not write, to choose
the reader connection; that put the hook's `notify.open_sessions` and the queue's reads behind
container greps, and the third review narrowed it to the agent read set (`is_agent_read`). Per-op
read caps, each in a different unit, were replaced by one byte budget. A poll that clamped a too-large
batch broke `mq subscribe --batch` over 256, and was replaced by a refusal.

### `ingest.chunks`

The plan named the ingest op `ingest.chunks{text[]}`, with the client chunking. Built as
`ingest.text`, named for what it carries: the host chunks by character windows (not a format
parser), so the chunking strategy stays in the idempotency key and the body is half the size.
