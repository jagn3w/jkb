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

`after` is the consumer's **fetch position**, separate from its committed one as in Kafka: a consumer
that has handed messages on but not yet acked them polls with `after` set to the last seq it handed
on. Without it, a batch of unacked messages comes back from every poll and nothing past it is read.

Errors carry a stable `code`: `no_such_topic`, `topic_conflict`, `no_such_group`, `queue_full`,
`too_large`, `invalid`, `ack_beyond_end`, `corrupt_payload` (with `seq`, so a consumer can ack past
it), `bad_request`, `internal`.

## `jkb mq subscribe` — the consumer stream

For a daemon in any language: run it as a child process and speak NDJSON.

```
jkb mq subscribe <topic> --group <name> [--from-start] [--at-most-once] [--batch N] [--interval-ms N]
```

**stdout, one event per line:**

- `{"event":"message","message":{"seq":…,"key":…,"kind":…,"payload":…,"producer":…,"enqueued_at":…,"expires_at":…,"expired":…}}`
- `{"event":"unreadable","seq":…,"reason":…}` — a stored payload that does not parse; reported once.
  Ack its `seq` to move past it.
- `{"event":"error","code":…,"reason":…,"fatal":bool}` — `fatal: true` is followed by exit status 1.

**stdin, one command per line:** `{"ack":<seq>}` commits the group's position through `seq`. Anything
else is answered with a non-fatal `bad_request` error event.

**Semantics.**

- The group is created if missing: from now, or from the start with `--from-start`. An existing group
  keeps its position.
- Each message is emitted **once per run**. What was not acked when the run ends is delivered again
  by the next run — **at-least-once**, so a consumer must be idempotent.
- `--at-most-once` acks each message before emitting it: a crash loses it instead.
- **EOF on stdin ends the subscription**, after processing the acks that preceded it. Keep stdin open
  for as long as you want messages.
- A closed stdout ends it with status 0.
- A group removed while subscribed (it idled out) is recreated from now, with a non-fatal
  `no_such_group` error event that says messages in between were not delivered.

Pinned end to end by `tests/cli.rs` `mq_subscribe_speaks_ndjson_over_pipes_and_resumes_after_the_ack`.

## Not yet

- The HTTP transport for these operations (`jkb serve`) and the container's remote mode — stage S3.
- `work` (competing consumers) and `compacted` (newest per key) queue types — design Q9.
- A native, non-subprocess client (Swift) — it will speak the HTTP protocol, documented with S3.
