use proptest::prelude::*;
use serde_json::json;

use super::{
    ack, compact, group_create, inspect, now_ms, poll, poll_needed, send, tail, topic_create,
    Created, Delivered, Draft, QueueError, Start, TopicSpec, MAX_BATCH,
};
use crate::{Db, Error};

const T0: i64 = 1_800_000_000_000;

fn draft(key: &str, n: u64) -> Draft {
    Draft {
        key: key.to_owned(),
        kind: "test.msg".to_owned(),
        payload: json!({ "n": n }),
        ttl_ms: None,
        producer: "test".to_owned(),
    }
}

fn db_with_topic(spec: TopicSpec) -> Db {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", move |c, m| topic_create(c, m, "t", &spec, T0))
        .unwrap();
    db
}

fn do_send(db: &Db, d: Draft, now: i64) -> crate::Result<i64> {
    db.write_txn("t", move |c, m| send(c, m, "t", &d, now))
}

fn do_group(db: &Db, g: &'static str, start: Start, now: i64) -> Created {
    db.write_txn("t", move |c, m| group_create(c, m, "t", g, start, now))
        .unwrap()
}

fn do_poll(db: &Db, g: &'static str, max: usize, now: i64) -> Vec<Delivered> {
    db.write_txn("t", move |c, m| poll(c, m, "t", g, max, None, now))
        .unwrap()
}

fn do_ack(db: &Db, g: &'static str, seq: i64, now: i64) -> crate::Result<i64> {
    db.write_txn("t", move |c, m| ack(c, m, "t", g, seq, now))
}

fn held_seqs(db: &Db) -> Vec<i64> {
    db.read(|c| {
        Ok(c.prepare("SELECT seq FROM mq_messages ORDER BY seq")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    })
    .unwrap()
}

fn queue_err(e: Error) -> QueueError {
    match e {
        Error::Queue(q) => q,
        other => panic!("expected a queue error, got {other}"),
    }
}

#[test]
fn a_topic_is_created_once_and_a_different_spec_is_refused() {
    let db = db_with_topic(TopicSpec::default());
    let again = db
        .write_txn("t", |c, m| {
            topic_create(c, m, "t", &TopicSpec::default(), T0)
        })
        .unwrap();
    assert_eq!(again, Created::Existing);
    let other = TopicSpec {
        max_messages: 5,
        ..TopicSpec::default()
    };
    let err = db
        .write_txn("t", move |c, m| topic_create(c, m, "t", &other, T0))
        .unwrap_err();
    assert_eq!(queue_err(err), QueueError::TopicConflict("t".to_owned()));
}

#[test]
fn a_producer_never_creates_a_topic() {
    let db = Db::open_in_memory().unwrap();
    let err = do_send(&db, draft("k", 1), T0).unwrap_err();
    assert_eq!(queue_err(err), QueueError::NoSuchTopic("t".to_owned()));
}

#[test]
fn names_keys_and_payloads_are_bounded() {
    let db = db_with_topic(TopicSpec::default());
    for bad in ["", "a//b", "a/../b", "sp ace", "semi;colon"] {
        let name = bad.to_owned();
        let err = db
            .write_txn("t", move |c, m| {
                topic_create(c, m, &name, &TopicSpec::default(), T0)
            })
            .unwrap_err();
        assert!(
            matches!(queue_err(err), QueueError::Invalid { .. }),
            "{bad:?}"
        );
    }
    let err = do_send(&db, draft("", 1), T0).unwrap_err();
    assert!(matches!(
        queue_err(err),
        QueueError::Invalid { what: "key", .. }
    ));
    let err = do_send(&db, draft(&"k".repeat(super::MAX_KEY_BYTES + 1), 1), T0).unwrap_err();
    assert!(matches!(
        queue_err(err),
        QueueError::TooLarge { what: "key", .. }
    ));
    let mut big = draft("k", 1);
    big.payload = json!("x".repeat(super::MAX_PAYLOAD_BYTES));
    let err = do_send(&db, big, T0).unwrap_err();
    assert!(matches!(
        queue_err(err),
        QueueError::TooLarge {
            what: "payload",
            ..
        }
    ));
}

#[test]
fn delivery_is_in_seq_order_from_the_committed_position() {
    let db = db_with_topic(TopicSpec::default());
    assert_eq!(do_group(&db, "g", Start::FromStart, T0), Created::New);
    let seqs: Vec<i64> = (0..5)
        .map(|n| do_send(&db, draft("k", n), T0).unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));

    let first = do_poll(&db, "g", 2, T0);
    assert_eq!(first.iter().map(|d| d.seq).collect::<Vec<_>>(), seqs[..2]);
    // Unacked messages are handed over again — at-least-once.
    assert_eq!(do_poll(&db, "g", 2, T0), first);
    assert_eq!(do_ack(&db, "g", seqs[1], T0).unwrap(), seqs[1]);
    let rest = do_poll(&db, "g", 10, T0);
    assert_eq!(rest.iter().map(|d| d.seq).collect::<Vec<_>>(), seqs[2..]);
    assert_eq!(rest[0].payload, json!({ "n": 2 }));
}

#[test]
fn a_group_created_from_now_skips_what_is_already_there_and_a_rerun_keeps_its_position() {
    let db = db_with_topic(TopicSpec::default());
    let old = do_send(&db, draft("k", 1), T0).unwrap();
    assert_eq!(do_group(&db, "g", Start::FromNow, T0), Created::New);
    let new = do_send(&db, draft("k", 2), T0).unwrap();
    assert_eq!(
        do_poll(&db, "g", 10, T0)
            .iter()
            .map(|d| d.seq)
            .collect::<Vec<_>>(),
        vec![new]
    );
    do_ack(&db, "g", new, T0).unwrap();
    // A restarted consumer re-creating its group must not be rewound by `FromStart`.
    assert_eq!(do_group(&db, "g", Start::FromStart, T0), Created::Existing);
    assert!(do_poll(&db, "g", 10, T0).is_empty());
    assert!(old < new);
}

#[test]
fn ack_is_monotonic_and_cannot_pre_consume_the_future() {
    let db = db_with_topic(TopicSpec::default());
    do_group(&db, "g", Start::FromStart, T0);
    let a = do_send(&db, draft("k", 1), T0).unwrap();
    let b = do_send(&db, draft("k", 2), T0).unwrap();
    assert_eq!(do_ack(&db, "g", b, T0).unwrap(), b);
    assert_eq!(
        do_ack(&db, "g", a, T0).unwrap(),
        b,
        "an older ack never moves back"
    );
    let err = do_ack(&db, "g", b + 1, T0).unwrap_err();
    assert!(matches!(queue_err(err), QueueError::AckBeyondEnd { .. }));
    let err = do_ack(&db, "nobody", a, T0).unwrap_err();
    assert!(matches!(queue_err(err), QueueError::NoSuchGroup { .. }));
}

#[test]
fn an_expired_message_is_still_delivered_and_flagged() {
    let db = db_with_topic(TopicSpec::default());
    do_group(&db, "g", Start::FromStart, T0);
    let mut d = draft("k", 1);
    d.ttl_ms = Some(1_000);
    do_send(&db, d, T0).unwrap();
    let got = do_poll(&db, "g", 10, T0 + 5_000);
    assert_eq!(got.len(), 1, "a TTL never skips anything");
    assert!(got[0].expired);
    assert_eq!(got[0].expires_at, Some(T0 + 1_000));
}

#[test]
fn compaction_reaps_only_what_every_group_consumed_and_has_expired() {
    let spec = TopicSpec {
        default_ttl_ms: Some(1_000),
        ..TopicSpec::default()
    };
    let db = db_with_topic(spec);
    do_group(&db, "a", Start::FromStart, T0);
    do_group(&db, "b", Start::FromStart, T0);
    let s1 = do_send(&db, draft("k", 1), T0).unwrap();
    let s2 = do_send(&db, draft("k", 2), T0).unwrap();
    do_ack(&db, "a", s2, T0).unwrap();
    do_ack(&db, "b", s1, T0).unwrap();

    let later = T0 + 10_000;
    let r = db
        .write_txn("t", move |c, m| compact(c, m, later, true))
        .unwrap();
    assert_eq!(r.messages_reaped, 1, "only s1 is consumed by both groups");
    assert_eq!(held_seqs(&db), vec![s2]);

    // Consumed by both but not expired: kept while there is room.
    do_ack(&db, "b", s2, later).unwrap();
    let s3 = do_send(&db, draft("k", 3), later).unwrap();
    do_ack(&db, "a", s3, later).unwrap();
    do_ack(&db, "b", s3, later).unwrap();
    let r = db
        .write_txn("t", move |c, m| compact(c, m, later + 1, true))
        .unwrap();
    assert_eq!(r.messages_reaped, 1, "s2 expired; s3 did not");
    assert_eq!(held_seqs(&db), vec![s3]);
}

#[test]
fn a_topic_with_no_groups_consumes_nothing() {
    let spec = TopicSpec {
        default_ttl_ms: Some(1),
        max_messages: 2,
        ..TopicSpec::default()
    };
    let db = db_with_topic(spec);
    do_send(&db, draft("k", 1), T0).unwrap();
    do_send(&db, draft("k", 2), T0).unwrap();
    let r = db
        .write_txn("t", |c, m| compact(c, m, T0 + 100, true))
        .unwrap();
    assert_eq!(r.messages_reaped, 0);
    let err = do_send(&db, draft("k", 3), T0 + 100).unwrap_err();
    assert!(matches!(
        queue_err(err),
        QueueError::QueueFull { messages: 2, .. }
    ));
}

#[test]
fn at_the_cap_consumed_messages_are_reaped_oldest_first_and_otherwise_the_write_is_refused() {
    let spec = TopicSpec {
        max_messages: 3,
        ..TopicSpec::default()
    };
    let db = db_with_topic(spec);
    do_group(&db, "g", Start::FromStart, T0);
    let s: Vec<i64> = (0..3)
        .map(|n| do_send(&db, draft("k", n), T0).unwrap())
        .collect();
    let err = do_send(&db, draft("k", 9), T0).unwrap_err();
    assert!(
        matches!(queue_err(err), QueueError::QueueFull { .. }),
        "all unread"
    );

    do_ack(&db, "g", s[1], T0).unwrap();
    let s3 = do_send(&db, draft("k", 3), T0).unwrap();
    assert_eq!(
        held_seqs(&db),
        vec![s[1], s[2], s3],
        "only the oldest consumed one went"
    );
    let s4 = do_send(&db, draft("k", 4), T0).unwrap();
    assert_eq!(held_seqs(&db), vec![s[2], s3, s4]);
    let err = do_send(&db, draft("k", 5), T0).unwrap_err();
    assert!(matches!(queue_err(err), QueueError::QueueFull { .. }));
}

#[test]
fn the_byte_cap_counts_key_kind_and_payload() {
    let spec = TopicSpec {
        max_bytes: 60,
        ..TopicSpec::default()
    };
    let db = db_with_topic(spec);
    // key 1 + kind 8 + payload `{"n":1}` 7 = 16 bytes each.
    do_group(&db, "g", Start::FromStart, T0);
    let a = do_send(&db, draft("k", 1), T0).unwrap();
    for n in 2..=3 {
        do_send(&db, draft("k", n), T0).unwrap();
    }
    let err = do_send(&db, draft("k", 4), T0).unwrap_err();
    assert!(matches!(
        queue_err(err),
        QueueError::QueueFull { bytes: 48, .. }
    ));
    do_ack(&db, "g", a, T0).unwrap();
    do_send(&db, draft("k", 4), T0).unwrap();
    let mut huge = draft("k", 0);
    huge.payload = json!("x".repeat(100));
    let err = do_send(&db, huge, T0).unwrap_err();
    assert!(matches!(
        queue_err(err),
        QueueError::TooLarge {
            what: "message",
            ..
        }
    ));
}

#[test]
fn an_idle_group_is_removed_after_its_idle_period_and_stops_holding_messages_back() {
    let spec = TopicSpec {
        default_ttl_ms: Some(1),
        ..TopicSpec::default()
    };
    let idle = spec.group_idle_ms;
    let db = db_with_topic(spec);
    do_group(&db, "live", Start::FromStart, T0);
    do_group(&db, "gone", Start::FromStart, T0);
    let s = do_send(&db, draft("k", 1), T0).unwrap();
    // Just short of the idle period the abandoned group still holds the message back.
    let almost = T0 + idle - 1;
    do_poll(&db, "live", 1, almost);
    do_ack(&db, "live", s, almost).unwrap();
    let r = db
        .write_txn("t", move |c, m| compact(c, m, almost, true))
        .unwrap();
    assert_eq!((r.groups_removed, r.messages_reaped), (0, 0));

    let past = T0 + idle;
    let r = db
        .write_txn("t", move |c, m| compact(c, m, past, true))
        .unwrap();
    assert_eq!(
        r.groups_removed, 1,
        "`gone` idled out; `live` polled recently"
    );
    assert_eq!(
        r.messages_reaped, 1,
        "and with it went the only thing holding s back"
    );
    let err = db
        .write_txn("t", move |c, m| poll(c, m, "t", "gone", 1, None, past))
        .unwrap_err();
    assert!(matches!(queue_err(err), QueueError::NoSuchGroup { .. }));
}

#[test]
fn compaction_waits_its_interval_unless_forced() {
    let db = db_with_topic(TopicSpec::default());
    let every = TopicSpec::default().compact_every_ms;
    let r = db.write_txn("t", |c, m| compact(c, m, T0, false)).unwrap();
    assert_eq!((r.topics_compacted, r.topics_skipped), (1, 0));
    let r = db
        .write_txn("t", move |c, m| compact(c, m, T0 + every - 1, false))
        .unwrap();
    assert_eq!((r.topics_compacted, r.topics_skipped), (0, 1));
    let r = db
        .write_txn("t", move |c, m| compact(c, m, T0 + every - 1, true))
        .unwrap();
    assert_eq!(r.topics_compacted, 1);
}

#[test]
fn inspect_reports_holdings_backlog_and_the_oldest_unconsumed() {
    let db = db_with_topic(TopicSpec::default());
    do_group(&db, "g", Start::FromStart, T0);
    let a = do_send(&db, draft("k", 1), T0).unwrap();
    do_send(&db, draft("k", 2), T0 + 7).unwrap();
    do_ack(&db, "g", a, T0).unwrap();
    let report = db.read(inspect).unwrap();
    assert_eq!(report.len(), 1);
    let t = &report[0];
    assert_eq!((t.messages, t.bytes), (2, 32));
    assert_eq!(t.oldest_unconsumed_at, Some(T0 + 7));
    assert_eq!((t.groups[0].position, t.groups[0].backlog), (a, 1));
    assert_eq!(t.groups[0].created_at, T0);
    assert_eq!(t.compacted_at, None);
    db.write_txn("t", |c, m| compact(c, m, T0 + 9, true))
        .unwrap();
    assert_eq!(db.read(inspect).unwrap()[0].compacted_at, Some(T0 + 9));
}

#[test]
fn messages_are_transport_and_never_reach_the_changelog() {
    let db = db_with_topic(TopicSpec::default());
    do_group(&db, "g", Start::FromStart, T0);
    let s = do_send(&db, draft("k", 1), T0).unwrap();
    do_poll(&db, "g", 1, T0);
    do_ack(&db, "g", s, T0).unwrap();
    let logged: i64 = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM changelog WHERE entity_type LIKE 'mq%'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(logged, 0);
}

#[test]
fn every_draft_and_spec_guard_refuses_with_a_named_error() {
    let db = db_with_topic(TopicSpec::default());
    let refuse = |d: Draft| queue_err(do_send(&db, d, T0).unwrap_err());

    let mut d = draft("k", 1);
    d.kind = "bad kind".to_owned();
    assert!(matches!(
        refuse(d),
        QueueError::Invalid { what: "kind", .. }
    ));
    let mut d = draft("k", 1);
    d.producer = "p".repeat(super::MAX_NAME_BYTES + 1);
    assert!(matches!(
        refuse(d),
        QueueError::TooLarge {
            what: "producer",
            ..
        }
    ));
    let mut d = draft("k", 1);
    d.ttl_ms = Some(-5);
    assert!(matches!(
        refuse(d),
        QueueError::Invalid { what: "ttl_ms", .. }
    ));
    // A NUL would pass `is_empty` and then fail the table's CHECK as a raw SQLite error.
    assert!(matches!(
        refuse(draft("\0host/a", 1)),
        QueueError::Invalid { what: "key", .. }
    ));
    let mut d = draft("k", 1);
    d.producer = "p\0".to_owned();
    assert!(matches!(
        refuse(d),
        QueueError::Invalid {
            what: "producer",
            ..
        }
    ));

    let long = "t".repeat(super::MAX_NAME_BYTES + 1);
    let err = db
        .write_txn("t", move |c, m| {
            topic_create(c, m, &long, &TopicSpec::default(), T0)
        })
        .unwrap_err();
    assert!(matches!(
        queue_err(err),
        QueueError::TooLarge {
            what: "topic name",
            ..
        }
    ));
    for spec in [
        TopicSpec {
            max_messages: 0,
            ..TopicSpec::default()
        },
        TopicSpec {
            max_bytes: 0,
            ..TopicSpec::default()
        },
        TopicSpec {
            default_ttl_ms: Some(0),
            ..TopicSpec::default()
        },
        TopicSpec {
            group_idle_ms: -1,
            ..TopicSpec::default()
        },
        TopicSpec {
            compact_every_ms: 0,
            ..TopicSpec::default()
        },
    ] {
        let err = db
            .write_txn("t", move |c, m| topic_create(c, m, "other", &spec, T0))
            .unwrap_err();
        assert!(
            matches!(queue_err(err), QueueError::Invalid { .. }),
            "spec refused by name, not by a CHECK"
        );
    }
}

#[test]
fn a_payload_that_would_not_parse_back_is_refused_at_send() {
    // serde_json serializes any depth but parses at most 128 levels; stored, this would fail every
    // poll of the topic for ever.
    let db = db_with_topic(TopicSpec::default());
    let mut deep = json!(0);
    for _ in 0..200 {
        deep = json!([deep]);
    }
    let mut d = draft("k", 1);
    d.payload = deep;
    assert!(matches!(
        queue_err(do_send(&db, d, T0).unwrap_err()),
        QueueError::Invalid {
            what: "payload",
            ..
        }
    ));
}

#[test]
fn a_corrupt_stored_payload_is_named_by_seq_and_can_be_acked_past() {
    let db = db_with_topic(TopicSpec::default());
    do_group(&db, "g", Start::FromStart, T0);
    let good = do_send(&db, draft("k", 1), T0).unwrap();
    let bad = do_send(&db, draft("k", 2), T0).unwrap();
    let after = do_send(&db, draft("k", 3), T0).unwrap();
    db.write_txn("t", move |c, _| {
        c.execute(
            "UPDATE mq_messages SET payload = 'not json' WHERE seq = ?1",
            [bad],
        )?;
        Ok(())
    })
    .unwrap();

    // Everything before it is still handed over...
    let got = do_poll(&db, "g", 10, T0);
    assert_eq!(got.iter().map(|d| d.seq).collect::<Vec<_>>(), vec![good]);
    do_ack(&db, "g", good, T0).unwrap();
    // ...then it is named, so the consumer can step over it.
    let err = db
        .write_txn("t", |c, m| poll(c, m, "t", "g", 10, None, T0))
        .unwrap_err();
    assert_eq!(
        queue_err(err),
        QueueError::CorruptPayload {
            topic: "t".to_owned(),
            seq: bad,
            why: "expected ident at line 1 column 2".to_owned()
        }
    );
    do_ack(&db, "g", bad, T0).unwrap();
    assert_eq!(do_poll(&db, "g", 10, T0)[0].seq, after);
}

#[test]
fn one_send_reaps_as_many_consumed_messages_as_it_needs() {
    let spec = TopicSpec {
        max_bytes: 64,
        ..TopicSpec::default()
    };
    let db = db_with_topic(spec);
    do_group(&db, "g", Start::FromStart, T0);
    let small: Vec<i64> = (0..3)
        .map(|n| do_send(&db, draft("k", n), T0).unwrap())
        .collect();
    do_ack(&db, "g", small[2], T0).unwrap();
    // 48 bytes held, all consumed. A 40-byte message needs two of them gone, not one.
    let mut big = draft("k", 0);
    big.payload = json!("x".repeat(29));
    let seq = do_send(&db, big, T0).unwrap();
    assert_eq!(held_seqs(&db), vec![small[2], seq]);
}

#[test]
fn a_refused_send_reports_what_the_topic_really_holds() {
    let spec = TopicSpec {
        max_bytes: 60,
        ..TopicSpec::default()
    };
    let db = db_with_topic(spec);
    do_group(&db, "g", Start::FromStart, T0);
    let a = do_send(&db, draft("k", 1), T0).unwrap();
    do_send(&db, draft("k", 2), T0).unwrap();
    do_send(&db, draft("k", 3), T0).unwrap();
    do_ack(&db, "g", a, T0).unwrap();
    // 48 held, 16 consumed: reaping `a` frees too little for 40 bytes, so nothing is reaped.
    let mut big = draft("k", 0);
    big.payload = json!("x".repeat(29));
    let err = queue_err(do_send(&db, big, T0).unwrap_err());
    assert_eq!(
        err,
        QueueError::QueueFull {
            topic: "t".to_owned(),
            messages: 3,
            bytes: 48
        },
        "the refusal reports the held totals, not an in-memory reap the rollback undid"
    );
    assert_eq!(held_seqs(&db).len(), 3);
}

#[test]
fn a_fetch_position_past_the_committed_one_reads_on_without_acking() {
    let db = db_with_topic(TopicSpec::default());
    do_group(&db, "g", Start::FromStart, T0);
    let s: Vec<i64> = (0..4)
        .map(|n| do_send(&db, draft("k", n), T0).unwrap())
        .collect();
    let after = s[1];
    let got = db
        .write_txn("t", move |c, m| poll(c, m, "t", "g", 10, Some(after), T0))
        .unwrap();
    assert_eq!(got.iter().map(|d| d.seq).collect::<Vec<_>>(), s[2..]);
    // The committed position did not move: a poll from it still starts at the beginning.
    assert_eq!(do_poll(&db, "g", 10, T0)[0].seq, s[0]);
    // A fetch position behind the committed one does not rewind it.
    do_ack(&db, "g", s[2], T0).unwrap();
    let got = db
        .write_txn("t", move |c, m| poll(c, m, "t", "g", 10, Some(0), T0))
        .unwrap();
    assert_eq!(got.iter().map(|d| d.seq).collect::<Vec<_>>(), vec![s[3]]);
}

#[test]
fn an_idle_poll_needs_no_write_until_its_touch_is_due() {
    let spec = TopicSpec {
        group_idle_ms: 400,
        ..TopicSpec::default()
    };
    let db = db_with_topic(spec);
    do_group(&db, "g", Start::FromStart, T0);
    let needed = |now: i64, after: Option<i64>| {
        db.read(move |c| poll_needed(c, "t", "g", after, now))
            .unwrap()
    };
    assert!(
        needed(T0, None),
        "never polled: the first poll records itself"
    );
    do_poll(&db, "g", 10, T0);
    assert!(
        !needed(T0 + 1, None),
        "nothing new and just touched: no write"
    );
    // The touch is due at a quarter of the idle period here, so a live consumer outlives compaction.
    assert!(needed(T0 + 100, None));
    let s = do_send(&db, draft("k", 1), T0 + 2).unwrap();
    assert!(needed(T0 + 3, None), "a message to hand over");
    assert!(!needed(T0 + 3, Some(s)), "…but not past the fetch position");
    let err = db
        .read(|c| poll_needed(c, "t", "ghost", None, T0))
        .unwrap_err();
    assert!(matches!(queue_err(err), QueueError::NoSuchGroup { .. }));
}

#[test]
fn tail_shows_the_newest_messages_oldest_first_without_moving_any_group() {
    let db = db_with_topic(TopicSpec::default());
    do_group(&db, "g", Start::FromStart, T0);
    let seqs: Vec<i64> = (0..5)
        .map(|n| do_send(&db, draft("k", n), T0).unwrap())
        .collect();
    let got = db.read(|c| tail(c, "t", 2, T0)).unwrap();
    assert_eq!(got.iter().map(|d| d.seq).collect::<Vec<_>>(), seqs[3..]);
    assert_eq!(
        do_poll(&db, "g", 10, T0).len(),
        5,
        "the group's position is untouched"
    );
    let err = db.read(|c| tail(c, "missing", 2, T0)).unwrap_err();
    assert!(matches!(queue_err(err), QueueError::NoSuchTopic(_)));

    // An unreadable payload is shown, flagged, and does not hide the newer messages after it.
    let bad = seqs[3];
    db.write_txn("t", move |c, _| {
        c.execute(
            "UPDATE mq_messages SET payload = 'nope' WHERE seq = ?1",
            [bad],
        )?;
        Ok(())
    })
    .unwrap();
    let got = db.read(|c| tail(c, "t", 2, T0)).unwrap();
    assert_eq!(got.len(), 2);
    assert!(got[0].unreadable && !got[1].unreadable);
    assert_eq!(got[0].payload, serde_json::json!("nope"));
}

// --- the reaping rules, against a model ------------------------------------------------------

#[derive(Debug, Clone)]
enum Step {
    Send {
        ttl: Option<i64>,
        pad: usize,
    },
    Ack {
        group: usize,
        back: usize,
        stale: bool,
    },
    Advance(i64),
    Compact,
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let step = prop_oneof![
        4 => (proptest::option::of(1i64..50), 0usize..40).prop_map(|(ttl, pad)| Step::Send { ttl, pad }),
        3 => (0usize..2, 0usize..4, proptest::bool::weighted(0.2))
            .prop_map(|(group, back, stale)| Step::Ack { group, back, stale }),
        1 => (1i64..40).prop_map(Step::Advance),
        1 => Just(Step::Compact),
    ];
    prop::collection::vec(step, 0..60)
}

fn held_rows(db: &Db) -> Vec<(i64, i64)> {
    db.read(|c| {
        Ok(c.prepare("SELECT seq, size FROM mq_messages ORDER BY seq")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    })
    .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Against a model, with varied message sizes and both caps reachable: nothing unconsumed is ever
    /// reaped; a send succeeds exactly when reaping what every group consumed makes room, and the caps
    /// hold afterwards; positions never move backwards, even for a stale ack; delivery is in seq order.
    #[test]
    fn reaping_never_loses_an_unconsumed_message(steps in steps()) {
        const GROUPS: [&str; 2] = ["a", "b"];
        const MAX_MESSAGES: i64 = 6;
        const MAX_BYTES: i64 = 160;
        let spec = TopicSpec { max_messages: MAX_MESSAGES, max_bytes: MAX_BYTES, ..TopicSpec::default() };
        let db = db_with_topic(spec);
        for g in GROUPS {
            do_group(&db, g, Start::FromStart, T0);
        }
        let mut now = T0;
        let mut positions = [0i64; 2];
        for step in steps {
            let before = held_rows(&db);
            let through = positions.iter().copied().min().unwrap_or(0);
            match step {
                Step::Send { ttl, pad } => {
                    let mut d = draft("k", 0);
                    d.ttl_ms = ttl;
                    d.payload = json!("x".repeat(pad));
                    let size = i64::try_from(1 + 8 + serde_json::to_string(&d.payload).unwrap().len()).unwrap();
                    // The model: reap consumed rows oldest-first until it fits, if that can.
                    let mut msgs = i64::try_from(before.len()).unwrap();
                    let mut bytes: i64 = before.iter().map(|r| r.1).sum();
                    for &(seq, sz) in &before {
                        if msgs < MAX_MESSAGES && bytes + size <= MAX_BYTES { break; }
                        if seq > through { break; }
                        msgs -= 1;
                        bytes -= sz;
                    }
                    let room = msgs < MAX_MESSAGES && bytes + size <= MAX_BYTES;
                    match do_send(&db, d, now) {
                        Ok(seq) => {
                            prop_assert!(room, "sent though the model had no room");
                            let after = held_rows(&db);
                            prop_assert_eq!(after.last().map(|r| r.0), Some(seq));
                            prop_assert!(i64::try_from(after.len()).unwrap() <= MAX_MESSAGES);
                            prop_assert!(after.iter().map(|r| r.1).sum::<i64>() <= MAX_BYTES);
                            for gone in before.iter().filter(|r| !after.contains(r)) {
                                prop_assert!(gone.0 <= through, "reaped unconsumed seq {}", gone.0);
                            }
                        }
                        Err(e) => {
                            let full = matches!(queue_err(e), QueueError::QueueFull { .. });
                            prop_assert!(full, "a refused send must be QueueFull");
                            prop_assert!(!room, "refused though reaping consumed messages made room");
                            prop_assert_eq!(held_rows(&db), before, "a refusal reaps nothing");
                        }
                    }
                }
                Step::Ack { group, back, stale } => {
                    let delivered = do_poll(&db, GROUPS[group], 10, now);
                    prop_assert!(delivered.windows(2).all(|w| w[0].seq < w[1].seq));
                    let seq = if stale {
                        // BELOW the position: re-acking the position itself cannot tell `max` from
                        // plain assignment.
                        Some((positions[group] - 1).max(0))
                    } else {
                        delivered.len().checked_sub(back + 1).map(|i| delivered[i].seq)
                    };
                    if let Some(seq) = seq {
                        let pos = do_ack(&db, GROUPS[group], seq, now).unwrap();
                        prop_assert!(pos >= positions[group], "position moved back");
                        prop_assert_eq!(pos, positions[group].max(seq));
                        positions[group] = pos;
                    }
                }
                Step::Advance(ms) => now += ms,
                Step::Compact => {
                    db.write_txn("t", move |c, m| compact(c, m, now, true)).unwrap();
                    let after = held_rows(&db);
                    for gone in before.iter().filter(|r| !after.contains(r)) {
                        prop_assert!(gone.0 <= through, "compaction reaped unconsumed seq {}", gone.0);
                    }
                }
            }
        }
    }
}

// --- two processes ---------------------------------------------------------------------------

/// A later-committed message always has a higher seq, across processes — `SQLite` admits one write
/// transaction at a time and holds its lock to commit. If one ever committed below the reader's acked
/// position, the reader would never be handed it: poll only looks past the position. So the guard for
/// the cross-process claim is that **every writer's every message is delivered**, counted per writer;
/// the in-order assertion catches a delivery bug within one process. Watched failing with `poll`
/// skipping one seq.
#[test]
fn two_processes_never_hand_a_reader_a_lower_seq() {
    const PER_WRITER: u64 = 150;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jkb.db");
    let db = Db::open(&path).unwrap();
    db.write_txn("t", |c, m| {
        topic_create(c, m, "t", &TopicSpec::default(), now_ms())
    })
    .unwrap();
    db.write_txn("t", |c, m| {
        group_create(c, m, "t", "reader", Start::FromStart, now_ms())
    })
    .unwrap();

    let exe = std::env::current_exe().unwrap();
    let mut writers: Vec<std::process::Child> = (0..2)
        .map(|i| {
            std::process::Command::new(&exe)
                .args([
                    "mq::tests::writer_process",
                    "--exact",
                    "--ignored",
                    "--nocapture",
                ])
                .env("JKB_MQ_TEST_DB", &path)
                .env("JKB_MQ_TEST_WRITER", i.to_string())
                .env("JKB_MQ_TEST_COUNT", PER_WRITER.to_string())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();

    let mut last = 0i64;
    let mut seen = 0u64;
    let mut per_writer = [0u64; 2];
    let mut done = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(2);
    while seen < 2 * PER_WRITER {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {seen} messages"
        );
        let batch = db
            .write_txn("reader", |c, m| {
                poll(c, m, "t", "reader", 25, None, now_ms())
            })
            .unwrap();
        for d in &batch {
            assert!(
                d.seq > last,
                "handed seq {} after {} — a cumulative ack would skip it",
                d.seq,
                last
            );
            last = d.seq;
            seen += 1;
            match d.key.as_str() {
                "writer-0" => per_writer[0] += 1,
                "writer-1" => per_writer[1] += 1,
                other => panic!("unexpected key {other}"),
            }
        }
        if let Some(d) = batch.last() {
            let seq = d.seq;
            db.write_txn("reader", move |c, m| {
                ack(c, m, "t", "reader", seq, now_ms())
            })
            .unwrap();
        } else if done {
            break;
        } else {
            done = writers.iter_mut().all(|w| w.try_wait().unwrap().is_some());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    for mut w in writers {
        assert!(w.wait().unwrap().success(), "a writer process failed");
    }
    assert_eq!(
        per_writer,
        [PER_WRITER, PER_WRITER],
        "every message from each writer was delivered — a lost one is what a cross-process inversion looks like"
    );
}

/// The writer half of `two_processes_never_hand_a_reader_a_lower_seq`, run as a separate process.
/// A no-op when run by itself.
#[test]
#[ignore = "helper process for two_processes_never_hand_a_reader_a_lower_seq"]
fn writer_process() {
    let Ok(path) = std::env::var("JKB_MQ_TEST_DB") else {
        return;
    };
    let writer = std::env::var("JKB_MQ_TEST_WRITER").unwrap();
    let count: u64 = std::env::var("JKB_MQ_TEST_COUNT").unwrap().parse().unwrap();
    let db = Db::open(&path).unwrap();
    for n in 0..count {
        let d = Draft {
            producer: format!("writer-{writer}"),
            ..draft(&format!("writer-{writer}"), n)
        };
        db.write_txn("writer", move |c, m| send(c, m, "t", &d, now_ms()))
            .unwrap();
    }
}

/// A poll or tail over a topic holding more than [`MAX_BATCH`] messages reads at most that many: both
/// run on the daemon's writer beside the notification hook, whatever the topic's creator let it hold.
#[test]
fn a_batch_is_bounded_whatever_it_asks_for() {
    let db = db_with_topic(TopicSpec::default());
    for n in 0..300 {
        do_send(&db, draft("k", n), 1_000).unwrap();
    }
    do_group(&db, "g", Start::FromStart, 1_000);
    assert_eq!(do_poll(&db, "g", usize::MAX, 1_000).len(), MAX_BATCH);
    let refused = db.read(|c| tail(c, "t", MAX_BATCH + 1, 1_000)).unwrap_err();
    assert!(
        matches!(
            queue_err(refused),
            QueueError::Invalid { what: "limit", .. }
        ),
        "a tail past the batch is refused"
    );
    assert_eq!(
        db.read(|c| tail(c, "t", MAX_BATCH, 1_000)).unwrap().len(),
        MAX_BATCH
    );
}
