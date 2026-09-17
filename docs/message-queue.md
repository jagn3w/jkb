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
| `notify.event` | `session`, `event` (`needed`\|`tool_finished`\|`user_acted`\|`turn_ended`\|`session_ended`), `tool?`, `message?`, `cwd?`, `owner?`, `instance?` (an `owner` is refused without it) — every event but `session_ended` also marks the process running in the session registry | `notified` {`state`, `moved`, `effects`, `refusal?`, `sent`} |
| `notify.open_sessions` | — | `sessions` {`sessions`: [{`session`, `tool`, `owner`, `instance`, `updated_at`}]} |
| `notify.gone` | `session`, `owner`, `instance` (as `notify.open_sessions` reported them) | `notified` {…} |
| `session.started` | `session`, `source`, `pid?`, `instance?` (a `pid` is refused without it), `cwd?` | `session_start` {`was`: `unknown`\|`live`\|`ended`} (the session's state before) |
| `session.ended` | `session`, `reason`, `pid?`, `instance?` — ends that process's hold only | `session_end` {`outcome`: `recorded`\|`already_ended`} |
| `session.gone` | `session`, `pid`, `instance` (as `session.list` reported them) | `session_gone` {`ended`} |
| `session.list` | `all?`, `after?` (a page's `next`, sent back as it came) | `claude_sessions` {`sessions`: one per process holding a session, [{`session`, `pid`, `instance`, `cwd`, `started_at?`, `start_source?`, `seen_at`, `ended_at?`, `end_reason?`}], `next?`} |
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
| `task.why` | `uid` | `history` {`entries`} (budgeted) |
| `task.add` | `text`, `home?`, `under?`, `backlog?`, `global_backlog?`, `sync?`, `managed?`, `cwd?`, `client_home?` | `added` {`id`, `uid`, `home`, `binding`}, or `needs_global_backlog_assent` |
| `task.set` | `uid`, `status?`, `priority?`, `due?` | `applied` |
| `task.edit` | `uid` (any item's), `text`, `append?` | `edited` {`file_backed`} — the result is capped at 256 KiB for a task, and for any item a client of `jkb serve` edits |
| `task.tag` | `uid`, `facet_value`, `mode` (`add`\|`set`\|`rm`) | `applied` |
| `task.depend` / `task.undepend` | `uid`, `dep` | `applied` |
| `task.place` | `uid`, `ns`, `home?` | `applied` |
| `task.unplace` | `uid`, `ns` | `unplaced` {`removed`} |
| `task.bind` | `uid`, `sync?` (`managed:` when absent) | `applied` |
| `task.claim` | `uid`, `owner` (≤ 512 bytes) | `claimed` {`acquired`, `refusal`} |
| `task.release` | `uid`, `owner` (≤ 512 bytes) | `released` {`released`} |
| `task.facts` | `uid` | `task_state` {`uid`, `status`, `tags` (facet → values), `claim?`, `land_target?`, `start_refusal?` (for a finished task only), `terminal`, `open_subtasks`, `writable` (`false` for a task filed outside the daemon's file roots — asked before git work)} |
| `task.by_branch` | `repo` | `branch_tasks` {`tasks`: branch → [{`uid`, `status`, `onto?`}], every task on the branch, in id order} |
| `task.start` | `uid`, exactly one of `take` {`owner`, `displace?`} and `keep` (the claim kept), `place` {`branch`, `repo`, `onto?`} | `taken` {`taken`} — `false`, nothing written, when the claim is not the one judged (`displace`, `keep`, or none) |
| `task.take` | `uid`, `take` {`owner`, `displace?`}, `place` {`branch`, `repo`, `onto`} — the claim; the place is judged (written for trial and rolled back), not recorded | `taken` {`taken`} |
| `task.locate` | `uid`, `owner`, `place` — recorded only while `owner` holds the claim | `taken` {`taken`} |
| `task.land` | `uid`, `landed` {`branch`, `onto`, `head?`} | `landing` {`moved`, `refusal?`, `status`} — the facts the caller established (graft, green gate, disposal) are stated |
| `task.landed` | `uid`, `landed` | `landing` — `observed_landed`; a guard's refusal is still recorded, an event the task's state does not define is not |
| `task.review_findings` | `namespaces` (≤64; a client asks in pieces) | `review_findings` {`total`, `open_count`, `open_must_fix` (≤100 {`uid`, `title` (≤200 chars)})} — refused past 10 000 tasks examined |
| `task.review_file` | `run` {`reviewers`, `returned`, `error?`} (refused unless no error and every reviewer came back), `ns` (must hold nothing; no `tasks` mount may cover it), `findings` (≤1000 [{`severity` (`must-fix`\|`concern`\|`nit`), `summary` (≤2 KiB), `file?`, `line?`, `scenario?`, `fix?` (≤32 KiB each)}], ≤768 KiB serialized — one request; `jkb task review file` trims the longest texts to fit) | `review_filed` {`ns`, `uids`, `clean`} — `managed:` tasks under `<ns>/<severity>` at priority 1/2/3; no findings files one `done` "clean review" task |
| `task.review_record` | `repo`, `branch`, `sha?` (letters and digits, ≤64), `findings` (must hold at least one item) | `review_recorded` {`recorded` [{`uid`, `moved_to_review`}], `skipped_unlanded`, `unusable`, `unwritable`} — one transaction |
| `task.claims` | `after?` (a page's `next`) | `claims` {`claims` [{`uid`, `owner`}] (≤1000 a page, task order), `next?`} |
| `task.reclaim` | `dead` (≤1000 owners the client proved gone) | `reclaimed` {`cleared`, `refused` [{`owner`, `reason`}], `unwritable`} — frees, through `observed_owner_gone`, claims still held by exactly one of `dead` |
| `kb.health` | — | `health` {`schema_version`, `fts_ok`, `flagged` (≤200 {`uri`, `status`, `detail?`}), `flagged_count`, `vector_tables`, `stale_vectors`} — on the writer: FTS5's integrity check is an `INSERT`. The un-embedded count is not here: which vector table counts depends on the host's embedder, so `doctor` prints it on the host only |
| `task.staging` | `repo`, `all?` | `staging_tasks` {`tasks` [{`uid`, `title`, `status`, `tags`, `land_target`, `open_subtasks`}] (tasks with a land target; a spent batch only with `all`), `truncated`} (budgeted) — `staging ls` asks git the rest |
| `item.show` | `uid`, `preview?` (characters; by kind when absent, ≤1 000 000) | `item` {`item` {`uid`, `kind`, `status?`, `resolution?`, `priority?`, `due?`, `mime?`, `binding?`, `namespace?`, `content_chars`, `content_hash?`, `created_at`, `updated_at`, `tags` [{`facet`, `value`}], `preview`, `preview_truncated`}} |
| `item.rm` | `uid` (full), `force?` | `item_removed` {`uid`, `kind`, `placements`, `edges`, `tags`} — refused under the file roots for an item filed outside them |
| `kb.related` | `uid`, `edges?`, `depth` (≤16), `direction?` (`out`\|`in`\|`both`) | `related` {`rows` [{`uid`, `kind`, `status?`, `resolution?`, `depth`, `via`, `direction`, `snippet?`}], `truncated` (budget), `at_node_cap`} — the walk stops at 1000 items, everywhere |
| `kb.blobs` | `contains?` (non-empty), `limit` (≤10 000) | `blobs` {`blobs` [{`hash`, `size`, `mime?`, `created_at`}], `truncated`} (budgeted) |
| `kb.blob` | `prefix` (4–64 hex digits, unique) | `blob` {`hash`, `text`} — a blob that is not UTF-8, or over 8 MiB, is refused; `jkb blob cat` on the host reads any blob in-process |
| `inv.read` | `read`: `ls` \| `type` {`ns`} \| `frontier` {`ns`, `all?`, `limit?`} \| `core` {`ns`} \| `tombstones` {`ns`} \| `retread` {`uid`, `depth` (≤16)} \| `evidence` {`uid`} \| `digest` {`ns`} | `inv` {`answer`: tagged `answer` — `list`, `type` {`source?`, `type_name?`}, `units`, `tombstones`, `evidence`, `digest`} — budgeted; `retread` walks and `evidence` lists at most 1000 (`at_node_cap`); units carry a 100-character snippet, never their body; a strategy's verbs and kinds are looked up by the client from `type` |
| `inv.write` | `write`: `new` \| `digest` \| `rollup` \| `do` \| `add` \| `link` \| `promise` \| `resolve` \| `reopen` \| `stale` (the `jkb inv` verbs' fields; at most 64 edges and 64 tags) | `inv` {`answer`} — refused under the file roots when the namespace's nearest mount is a file mount outside them, or a named unit is filed outside them; a task's line is held to its round trip |
| `task.pr_facts` | `uid` | `pr_facts` {`uid`, `pr?`, `branch?`, `live_landing`, `superseded?`, `resumed_at?`, `writable`} |
| `task.open_in_repo` | `repo` (≤255 bytes) | `uids` {`uids`, `truncated`} — the repo's unfinished tasks, uid and status read only (budgeted) |
| `task.pr_record` | `uid`, `number` (> 0) | `applied` — a `note` in the task's history; refused under the file roots for a task filed outside them, and held to its line's round trip |
| `task.close_merged` | `uid`, `merged` (`yes`\|`no`\|`unknown`, the client's `gh` answer), `observed` {`live_landing`, `resumed_at?`} (the history it judged from), `pr?`, `dry_run?` | `closed` {`refusal?`} — `observed_landed`, judged in the op's transaction; held if the history no longer matches `observed`; refused under the file roots like `pr_record` |
| `ns.list` | `scope?` | `namespaces` {`paths`, `truncated`} (budgeted) |
| `ns.mv` | `from`, `to` | `moved` {`count`} — under the file roots only: refused for a reserved namespace (`_sys/…`, `tasks`), past 1000 items or 1000 namespaces, unless every mount at, above or below the subtree and every item in it is inside the roots, and when a task's line would not come back; on the host, the core's move alone |
| `kb.history` | `path` (absolute, the client's), `home?` (the client's `$HOME`, re-rooted to the host's as `kb.ambient` does) | `versions` {`uri`, `versions` [{`ts`, `blob`, `status`}], `truncated`} (budgeted) |
| `task.abandon` | `uid`, `observed?` (the claim read before the git work) | `abandoned` {`released`, `reopened`, `status`} — `released` is false only when someone else holds the task |
| `repo.gate` | `repo` | `gate` {`gate?`} — read-only: no op stores a gate |
| `session.state` | `session` | `session_is` {`state`: `live`\|`ended`\|`unknown`} (a closed set) |
| `removal.add` | `removal` {`worktree`, `repo_root`, `branch`, `uid`, `delete_branch`, `accept_dirty`, `recorded_at`, `head?`, `archive?`, `archived_at?`} | `removal_added` {`id`} |
| `removal.list` | `after?` | `removals` {`records`, `next?`} — 64 a page, oldest first; each record carries `id` and `written_via` |
| `removal.archived` | `id`, `archive`, `at` | `changed` {`changed`} — only a pending record moves |
| `removal.cancel` | `ids` (≤256) | `removals_cancelled` {`cancelled`, `sweep_holder?`} — nothing is dropped while the sweep's lease is held |
| `removal.drop` | `id` | `changed` {`changed`} |
| `lease.get` | `name` (`removal-sweep`, or `land:<repo>`) | `lease` {`lease?` {`holder`, `taken_at`}} |
| `lease.take` | `name`, `holder` (`<owner> <nonce>`), `displace?` | `changed` {`changed`} — free, or still held by exactly `displace` |
| `lease.release` | `name`, `holder` | `changed` {`changed`} — only the holder's own |
| `lease.break` | `name` | `lease_broken` {`holder?`} — refused to a client of `jkb serve` |
| `ingest.text` | `text`, `mime` (≤ 255 bytes), `namespace` | `ingested` {`document`, `namespace`, `chunk_count`, `embedded`, `already_ingested`, `warnings`} |

Every listing answer (`items`, `listing`, `tree`, `children`, `search_hits`, `task`, `history`) and `grep_hits`
carries `truncated: true` when the read was cut — at its byte budget, or a tree at its node cap — and
omits the field otherwise. `jkb task show --json` gained a `subtasks` array (`uid`, `title`,
`status`) with this op, so a cut it reports refers to something in the document.

**The task-mutate set** (`task.add`/`set`/`edit`/`tag`/`depend`/`undepend`/`place`/`unplace`/
`bind`/`claim`/`release`, and the read `task.why`; tasks S6.2) is in `crates/jkb-api/src/tasks.rs`,
one implementation each, which the host CLI runs through a `LocalBackend` too
(`crates/jkb-cli/src/task_cli.rs`). Only what cannot happen on the serving side stays in the client:
reading stdin (`task edit --stdin`), asking the terminal, and choosing the claim owner (this process's
`host:pid` or agent id). The terminal question is the op's to raise: `task add --backlog` outside any
repo answers `needs_global_backlog_assent` only once everything else about the request has validated
(a refusal comes before the question), and the client asks and sends it again with
`global_backlog` — the rule for what counts as an explicit placement is not copied into the client.
Whether a task is file-backed is decided by its binding, never by how the caller spelled its uid (a
task `task add` files has a `task:` uid). The branch and repo facet writers moved into `jkb-core` (`location.rs`) for it, with the
ref-name check whose sentence the CLI's git calls share; so did the lookup of the `tasks.md` covering
a home (`mount::tasks_file_for`), which `jkb-sync` now calls too. Every backend names the actor the
changelog records its writes under — `cli` for the host's command line, `serve` for the daemon's
clients, `reap` for the reap service's compaction — so the audit trail says where a change came from.
Two decisions in it:

- **A write that would have the host's sync write a file outside the container's view is refused.**
  A task bound to a file is written back to it by the host's `jkb sync --watch` (design H4), so a
  backend given `tasks::FileRoots` refuses, with `forbidden`, every write to a task whose binding —
  or whose uid — is a file outside the roots, creating a task filed in one (`--managed` is served), and
  binding a task to a file at all. The uid matters because a task taken out of its file is rebound
  `managed:` but keeps its `file://` uid, and sync re-attaches it by that uid when the line comes back.
  Only a file binding makes sync write: it gathers a file's items by binding, so a managed task placed
  under a tasks mount's namespace is not written into its file. The refusal test is driven from the
  one-sample-per-op list, so a task write added later without the guard fails it. `jkb serve` gives its clients `$HOME/repos` — `jkb_daemon::CLIENT_FILE_ROOT`, which
  `.container/check-config.sh` holds to the container's `${localEnv:HOME}/repos` bind — because a sync
  write there is one the container could make itself; the host CLI's backend has no roots. A path is
  judged by its components without touching the filesystem (`..` or `.` is outside; only a trailing
  `#<id>` is a fragment). That judges the path's *spelling*, which cannot see a symlink — and the
  container can plant one inside `~/repos` at a bound `tasks.md` or a directory above it. A second
  review caught it; the fix is in sync itself, which no longer follows a link on any bound file's path
  ([namespaces-and-sync.md](namespaces-and-sync.md), "Sync never follows a symbolic link"). An earlier
  version of this paragraph said links were no risk because bindings come only from host-made mounts:
  true of the binding, false of the directory it names. Pinned by
  `a_rooted_backend_refuses_every_write_to_a_task_filed_outside_its_roots`,
  `every_task_write_a_client_can_send_is_refused_for_a_task_filed_outside_the_roots`
  and, through a real daemon, `the_task_writes_go_through_the_daemon_and_stop_at_the_container_s_view`.
- **What a request can make the writer do is bounded.** A namespace path is at most 4096 bytes and 128
  segments (`ns::MAX_PATH_BYTES`/`MAX_DEPTH`, in `normalize`, so every entry point has it): `ensure`
  writes a row per ancestor, and a megabyte `a/a/…` in a request body was half a million rows of rising
  length in one transaction on the writer — and it is checked again after NFC, which can lengthen a
  path, so a stored path stays nameable. A quick-add line carries at most 64 `+ns`/`#tag`/`^dep`
  modifiers, a task body at most 256 KiB after an edit or append, a tag or due date at most 1024 bytes,
  a claim owner at most 512 (it is stored on every transition). `task.why` is charged to the read
  budget like every listing. **Every task write holds the task's whole tasks.md line to the file's
  round trip** (`jkb_sync::filed_task_problem`, run by `LocalBackend` after the op, inside its
  transaction): the line is assembled as an export would assemble it — real local id, text, status,
  priority, due date, tags, out-of-file placements, in-file dependencies — rendered alone and parsed
  back, and a write that makes a readable line come back different is refused, naming the first field
  that fails on its own. A line that was already unreadable does not block later writes: writers outside
  the typed operations do not ask the file (the MCP server's `task_update`, `jkb ns mv` on the host, `jkb tag
  rename`; the session verbs' ops and a client's `ns.mv` do, since S6.4), and refusing every write after one of them left the task
  unable to be released. Except a write that moves the task to another line (`task.bind`), which is
  judged as a new line: excused by the old line's problem, a bind from an unreadable line onto another
  task's `#id` put two tasks on one line, and the next export dropped one. Checking only the text let `task set --due "2026-07-15 17:00"`, a tag value or a
  namespace with a space, and `task bind --sync …#Fix_Login` through, and the next import from the file
  cleared the field and rewrote the title. Whether a task is in a tasks file is decided by the
  serializer owning its binding (`binding::serializer_for`: a `#<local id>` binding goes to its mount,
  since only a multi-item serializer makes one; a whole-file uri to the sync journal row the engine
  wrote for it), never by a `#` in its uri. `task edit` and `jkb item edit` also share one edit rule
  (`item::edit_content`), which refuses a text the serializer would not read back as written
  (`jkb_sync::task_content_problem`): a blank or whitespace-only line, a checkbox line in the body,
  trailing `^id`/`@due`/`#tag`/`+ns`/`!p` tokens, or what the parser normalizes (quotes, runs of spaces
  or tabs, spaces at the title's ends and a body line's start); the refusal names which. The size cap
  is checked before that probe, which parses inside the writer's transaction, and the parse is linear
  in duplicate lines. Tags and due dates are bounded in the
  core writers every path shares (`tag::MAX_TAG_BYTES`, `task::MAX_DUE_BYTES`), so `tag rm` can always
  remove one, and `ns mv` checks every path it would write.
  `task.add`'s global-backlog question is answered by running the whole create and rolling it back, so
  it is asked only when the add would otherwise succeed.
- **What stays on the host.** The session verbs are being split into client-side git and ops (tasks
  S6.4, `jkb_api::sessions`, `jkb_api::removals`): `task start`, `work`, `abandon`, `sessions` and
  `task gate` (show), `task land` and `task landed` run remotely, and so do `task review file`/`record`,
  `task reclaim` and `doctor` (stage 5); storing a gate never runs remotely
  (a stored gate is a command the host runs: a `--gate` or detected gate is run there and not
  remembered), nor does `task reap`, breaking a lease (`task reap --break-lock`,
  `task land --break-lock`), `doctor --fix`/`--backup`, `undo` (it reverts any transaction, the
  host's own included), `mount` or `sync`.
- **A review's findings are filed by an op, never through a mount** (stage 5, design-s6-4.md F).
  `/jkb-review-log` used to write a `tasks.md`, `mount create` its folder and `jkb sync` it; from the
  container that has the host read and write files at a path the container chose. `task.review_file`
  takes the reviewer's findings as data and writes rows, in both modes. A finding can no longer be
  ticked in an editor. `jkb task review file` refuses a result whose `error` is set or whose
  `reviewers` is 0: an empty result from a review that did not run, filed as clean, would let
  `task land` pass. (The command file `.claude/commands/review-log.md` still describes the mount;
  its switch is pending.)
- **The land lock is the `land:<repo key>` lease** (stage 4, decision D), taken as a compare-and-set
  with holder `<owner> <nonce> <Claude Code session or ->`. It was `.jkb/land.lock` holding a pid, which
  the other side of the bind cannot probe. A holder is stale only when proven gone, asked in this
  order: a process this host can probe decides — dead is stale, alive is not, whatever became of its
  session (a land left running by a Claude Code that died is still grafting; stage-4 review); only a
  process this host cannot probe falls back to the session it names, stale once that has ended in
  the registry. Anything else is respected until `jkb task land --break-lock` on the host. The repo key is a directory's basename, so two repos of
  one name share a lease: they land one at a time, which costs only waiting.
- **Worktree-removal records name only the shared directory** (stage 3). They moved from files beside
  the database into `worktree_removals` (V020), and the sweep's lock into the `leases` table. A record
  is acted on by the host's reap service — renamed, later deleted — so every path a client writes, or
  names by id, must be `~/`-relative, of plain segments, and under `~/repos` once resolved against the
  daemon's home (`FileRoots::admits_home_path`). That check is spelling only, and the container can
  plant links under `~/repos`, so the **reader** confines too (stage-3 review, must-fix: a linked
  `.jkb/archive` had the host sweep delete `~/Documents`). A row is **confined** unless the host's
  own CLI or reap service wrote it *and* it names no `~/` path (a client can re-point a host row
  under `~/repos` with `removal.archived`, so who wrote it proves nothing there). A confined row is
  acted on only if its repo root, resolved at the moment of acting, lies under `~/repos`; one that
  resolves elsewhere is reported REFUSED and never touched. The sweep's
  rename and removal walk that resolved path with `O_NOFOLLOW` (`jkb_core::nofollow::rename_into`,
  `remove_tree`), so a link anywhere below the root is refused rather than followed. What remains
  is git: the sweep's `git worktree remove`/`branch -D` run with the path as spelled, and a branch is
  deleted only when its tip is the commit the record names. Clients read every record, the host's
  absolute paths included. A disposal whose record the daemon refused says nothing will finish it. `task work` cancels a pending record with `removal.cancel`, one write that a sweep
  in flight refuses, rather than by taking the sweep's lease: a container `task work` killed while
  holding it would have left a holder the host cannot probe, and the host's reap service would skip
  every pass until someone broke it. A client can still take `removal-sweep` and hold it: that stops
  the host's sweep **and every `task work`** (a cancel is refused while the lease is held, even with
  nothing to cancel), acts on nothing, and `jkb task reap --break-lock` on the host ends it. A client
  holding a `land:<repo>` lease stops only landings in repos of that name, and
  `jkb task land --break-lock`, run on the host in that repo, ends it.
- **The old file store is reported, and nothing acts on it** (rounds 2–5 of the stage-3 review).
  Records an older jkb wrote beside the database were first swept in place, then imported by every
  sweep, then imported on request — and each version was found steerable or lossy: `~/.jkb` is
  bind-mounted read-write, so the directory and its files are the dev container's to replace (links,
  FIFOs, huge or countless files), and a pending removal written before the move may name a checkout
  somebody has since gone back to, which an import would then archive. So `task reap` and `doctor`
  only list the files by name (a bounded look that never opens one; a store directory that is a link
  is not looked into) and ask the operator to judge each checkout and remove the file by hand. No
  lock file there is read or honoured any more.
- **`removal.cancel` skips and names a record the client may not name** rather than refusing the
  batch, and `task work` then refuses to hand the checkout back: that record is still owed. `task work`
  sends its ids in batches under the daemon's cap, whose count the container could otherwise inflate.
- **A worktree is unregistered by its own path, never with `git worktree prune`** (rounds 3–4):
  prune drops every registration whose directory this side cannot see, which across the bind is every
  session opened on the other side. `gitrepo::forget_worktree` runs `git worktree remove` on a missing
  directory, and `worktree_remove` no longer prunes after removing (both measured on git 2.51.1).
- **Liveness is judged where the owner lives; the daemon only compares.** `task reclaim` and `doctor`
  list the claims (`task.claims`), probe each owner where they run, and send the owners they proved
  gone (`task.reclaim`, stage 5, design-s6-4.md H); the op frees the claims still held by exactly
  those strings, so a probe cannot free a claim taken after it — a new process or a resumed session
  claims under another string. **Residual, stated:** a `host:pid` owner carries no run, so a pid probed
  dead and reused by a new process that claims under the same `host:pid` before the reclaim commits
  (milliseconds) is freed; the host's in-transaction re-probe this replaced closed that window too.
  A client of the daemon is refused an owner it cannot have proved: a process of the daemon's own host,
  a session checkout outside `~/repos`, and (from anyone) an `agent:` or unreadable owner. A claim held
  by a live or unestablished owner is refused by `task.claim` from anywhere. **But the daemon does not
  judge liveness.** A client that names an owner
  can drop it (`task.release`), and so can a session op's `displace`. A misbehaving container can
  therefore free any claim, and an honest one never does, because it can prove gone only what its own
  machine can see (tasks S6.4 design, "Residual, stated"). `task mirror` is a sweep over every task.

**Ingest** (`ingest.text`; tasks S6.3, `crates/jkb-api/src/ingest.rs`). `jkb ingest <file|url>` reads
and parses its source where it runs (`jkb_ingest::read_source`: a file by its extension, a URL rendered
in a headless browser) and sends only the extracted text. In the dev container that means the
container's file, a page fetched through the container's egress firewall, and PDF/HTML parsers running
on the container's kernel — design H4 refuses a path (the host would read a host path), a URL (the host
would fetch outside the firewall) and raw bytes (the host's parsers would take the container's input).
The host chunks the text — character windows, not a format parser — so the chunking strategy stays in
the ingestion's idempotency key, and captures it through the same `Pipeline::ingest` as ever.
- **Addressed by what the host saw.** From the host's own process the raw bytes travel with the request
  (`IngestAsk::raw`, `#[serde(skip)]`, so never on the wire): the document is `b3:<hash of the bytes>`
  and the bytes are its blob, exactly as a host ingest always was, so re-ingesting a file ingested
  before this op is still a no-op. Through the daemon there are no bytes, so the document is addressed
  by `jkb_ingest::text_address` — blake3 of the text in key-derivation mode under its own context, which
  no plain hash of any bytes equals (review 2: a prefix-and-text hash was matched by a file whose bytes
  began with the prefix) — and no blob is stored.
  Not by a hash the client names, which would let it choose the uid its text is filed under; and not by
  the text's plain hash either (review 1): a UTF-8 file's text is its bytes, so a container sending a
  host file's bytes as text took the uid and ingestion row the host's own ingest of that file resumes
  into, leaving the host's document unparsed, under the container's namespace and mime, and `already
  ingested` for good. The same file ingested from the container and from the host is two documents.
- **No model call for a client.** `jkb serve` has no embedder (as for search), so a container's ingest
  is captured, keyword-searchable at once, and answered `embedded: false` with a warning; it gains vectors
  when the host runs `jkb index --pending`. A repeat of the same text answers `already_ingested`; once
  the vectors exist (written by `index --pending`, which keys no ingestion) a repeat marks the ingestion
  complete and answers `embedded: true`. Nothing runs `index --pending` on its own yet (tasks F5).
- **Size and load.** The request is bounded by the daemon's body cap (`jkb_daemon::MAX_BODY_BYTES`, 1 MiB),
  which `RemoteBackend` checks before sending any op — a body many times the cap was otherwise cut off
  mid-upload and reported as a daemon that could not be reached. A capture at the cap holds the single
  writer for about half a second (measured by the stage-6.3 review: 457 ms release, Linux), so ingests
  run one at a time under a budget of their own (`max_ingests`, 1) in place of an op permit, refused
  `busy` past it, rather than queue ahead of the notification hook's writes, which have 1 s.
- **Answer.** `namespace` is where the document is — for one ingested before, where that ingest put it.

The `notify.*` ops are the permission-notification machine, which runs in the daemon and sends its
effects on `claude/notify` as `notify.post` (payload `id`, `session`, `title`, `subtitle`, `body`; TTL
12 h) and `notify.withdraw` (`id`, `session`; no TTL), keyed `session/<id>` — and only while the topic
has a consumer group. [notifications.md](notifications.md) is their record.

**The agent read set** (`kb.*`, `task.ready`/`show`/`subtasks`; tasks S6.1) is in
`crates/jkb-api/src/kb.rs`, and each read has **one implementation there**: the host CLI serves `jkb
query`, `find`, `recent`, `search`, `ls`, `tree`, `grep`, `cat` and `jkb task next`/`show`/`subtasks`
through a `LocalBackend` too (`crates/jkb-cli/src/ops_cli.rs`), so the daemon cannot answer one of
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
  write beside it under 0.7 s). Which ops go there is said once, by `Request::is_agent_read` — the
  read set (`kb.*`, `task.ready`/`show`/`subtasks`/`why`, `task.facts`/`by_branch`/`review_findings`, `repo.gate`), **not** every op that does not write: the reader
  serves one call at a time behind a client's greps, so the queue's and the hook's own short reads
  (`mq.inspect`, `mq.tail`, `notify.open_sessions`, `session.list`, whose `SessionStart` sweep has 1 s) stay on the
  writer. Classing by "does not write" put that sweep behind a container's grep, and a third review
  caught it. The backend picks the connection from the class — no dispatch arm chooses — and `jkb
  serve` counts the read set against a **third permit budget** (`max_reads`, 16) beside ops and
  long-polls, so queued reads cannot hold the op permits a hook's write needs. A permit is released
  only when its call has returned on its blocking thread and hyper has written or dropped the answer:
  released with the request's future, a client that asked and hung up grew the reader's queue while
  the budget read empty. The answer is handed to hyper in 64 KiB frames, because given one frame it
  copied the whole answer into its buffer and dropped the body — and the permit — before sending a
  byte (measured: a loopback client that never read got its permit straight back). And a write that
  makes no progress for `write_stall` (10 s) closes its connection, since hyper has no write timeout:
  without it a client that stopped reading kept its permit until the daemon restarted. Pinned by `every_op_is_served_on_the_connection_its_class_names` (each op's
  class against the test's own list; with the writer held every read answers, with the reader held
  every other op does; no op is refused as a write to the read-only connection),
  `a_permit_outlives_a_cancelled_request_until_its_call_returns`,
  `an_unread_answer_holds_its_permit_until_the_write_deadline` and
  `reads_have_their_own_permits_and_a_bounded_answer`.
- **Every read that lists is bounded by one byte budget** (`kb::Budget`), charged row by row with
  what each row serializes to, so the answer is a prefix of the full one and says `truncated`. The
  daemon gives each read 16 MiB (`read_budget_bytes`); the host CLI's is unlimited, and the CLI says
  on stderr when an answer was cut. It replaced per-op caps, each of which a second review found
  measured in the wrong unit — chunks of context, while a document hit's context is its whole body;
  bytes of line text, while each line carries its own JSON — or missing (`kb.query` with no limit).
  Not bounded, stated: `kb.cat` and `task.show`'s own body are the one item asked for; a namespace's
  children are gathered before they are sorted and charged; and the queue's reads, which stay on the
  writer, are bounded instead by `mq::MAX_BATCH` (256): a poll or a tail asking for more is refused,
  and `jkb mq subscribe --batch` is held to it when parsed, so neither reads more than about 16 MiB of
  payload however large its topic's creator — a container included — let it grow. `session.list`,
  also on the writer, is paged instead: at most 1000 rows (`claude_session::LIST_CAP`) and an opaque
  `next` cursor when there are more, which the hook's sweep and `jkb notify sessions` follow. A row is
  usually a few hundred bytes. The bound is larger: a working directory may take 4 KiB, and JSON
  escaping can double that, so a page of 1000 is bounded at about 9 MiB. Refused, not
  clamped: the subscribe stream reads a batch shorter than it asked for as caught up, and a clamp
  announced `caught_up` after every capped poll of a backlog (a fifth review caught it). The frontier (`task.ready`) is ordered
  and limited over ids, and a task's subtasks are streamed, so at most one body is held at a time
  (`task::ready_ids`, `task::subtasks_each`). Every list in `jkb-core` a client can lengthen — ids,
  uris, and a query's `kind:` values, which come from DSL text — is bound as one JSON parameter
  (`sql::json_ids`, `json_strings`), since a placeholder per element failed past `SQLite`'s 32,766
  variables. The other bounds are on
  work rather than answer size: `kb.search` takes at most 1000 hits (the hybrid route, served only
  where there is an embedder, fuses from twice the limit) and 50 chunks of context either side;
  a query evaluates at most 64 `tag:`/`-tag:` terms (`query::MAX_TAG_TERMS`) — each is a subquery, and
  ~1000 of them from an 8 KiB request exceeded SQLite's expression depth as an internal error;
  `kb.grep` refuses an empty pattern and reads items one at a time (`item::grep_each`), counting
  past the budget, and `-c`/`-l` ask for no lines; `jkb recent` orders and limits on the server
  (`order: updated_desc`); `kb.tree` descends at most 48 levels (each is two levels of JSON, and
  serde_json refuses past 128), not into a node whose reference is one of its ancestors', and stops
  after 10,000 nodes — a cut the answer names apart (`at_node_cap`), since unlike the budget no host
  lifts it. Pinned by `every_listing_read_stays_within_its_budget_and_says_when_it_was_cut`
  across all ten listing reads; each guard was checked by removing it.

`after` is the consumer's **fetch position**, separate from its committed one as in Kafka: a consumer
that has handed messages on but not yet acked them polls with `after` set to the last seq it handed
on. Without it, a batch of unacked messages comes back from every poll and nothing past it is read.

Errors carry a stable `code`: `no_such_topic`, `topic_conflict`, `no_such_group`, `queue_full`,
`too_large`, `invalid`, `not_found` (an item a read names does not exist), `unsupported` (a search
route this backend does not serve), `forbidden` (a write whose host-side effect this client may not
cause — see the task-mutate set), `ack_beyond_end`, `corrupt_payload` (with `seq`, so a consumer can ack past
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
jkb mq subscribe <topic> --group <name> [--from-start] [--at-most-once] [--batch N (1-256)] [--interval-ms N]
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
- holds a 1 MiB body limit, and separate concurrency budgets for operations, long-polls, the agent
  read set and `ingest.text` (see their paragraphs above), answering `busy` when one is exhausted. A request past authentication holds its permit from before its body is
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

- runs `jkb mq …`, the agent read set, the task-mutate set, `jkb ingest`, the session verbs, the review
  and reclaim verbs and `jkb doctor`'s report (above) through the daemon;
- runs the commands that need no database (`notify`, `guide`, `commands`) as usual;
- **refuses everything else before it does anything** — with a reason: host-only commands (`sync`,
  `mount`, `service`, `serve`, `doctor --fix`) never go through the daemon, the rest are not ported yet;
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

- `JKB_REMOTE` set in the container — at the cutover (tasks S6.5), not before: remote mode refuses
  `JKB_DB` and every unported command, and the container's agents still need both. Decided with the user
  (2026-09-15): the cutover waits until nothing the container's agents use is refused — the session
  verbs above all, since `jkb task work` makes the worktrees agents run in. The network path
  it will take already exists; see `.container/README.md`, "The one opening to the host".
- The rest of the container's commands, then the cutover — stages S6.4/S6.5. The read set (S6.1), the
  task-mutate set (S6.2), ingest (S6.3), the session verbs, and (stage 5) `staging ls`, `stat`,
  `item`, `related`, `blob`, `history`, `inv`, `doctor`'s report, `task review` and `task reclaim` are
  served, and so are `ns ls`/`mv`, `task pr` and `task close-merged`; `view`, `tag`, `ns mk`/`rm`/`type`
  and `jkb mcp` are still refused remotely, and `undo`, `index` and `task mirror` always will be (they
  revert the host's own writes, call its model, and sweep every task).
- Embedding what the container ingests (tasks F5): captured and keyword-searchable, it stays unembedded
  until `jkb index --pending` runs on the host, and nothing runs it on a schedule.
- The MCP server's read tools (`jkb-mcp/src/logic.rs`) still read the database directly rather than
  through `jkb-api` (design H4 says they should become its callers).
- `work` (competing consumers) and `compacted` (newest per key) queue types — design Q9.
- A native, non-subprocess client (Swift) — it would speak the HTTP protocol above. `jkb-notifier
  serve`, the first consumer, runs `jkb mq subscribe` as a child for now (design N2).
