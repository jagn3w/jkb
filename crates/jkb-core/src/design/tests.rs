//! Design documents: the version-token edit, the CRDT merge, undo as a forward update, and span
//! state derivation (D53.4–5).

use jkb_types::{ItemId, TaskStatus};
use yrs::Text as _;

use super::*;
use crate::{containment, task, Db};

fn db() -> Db {
    Db::open_in_memory().unwrap()
}

fn create(db: &Db, body: &str) -> String {
    let body = body.to_owned();
    db.write_txn("t", move |c, m| {
        super::create(c, m, "jkb", "Code Factory", &body)
    })
    .unwrap()
    .uid
}

fn cat(db: &Db, uid: &str) -> DesignText {
    let uid = uid.to_owned();
    db.read(move |c| read(c, &uid)).unwrap()
}

fn token(db: &Db, uid: &str) -> String {
    cat(db, uid).version.token()
}

fn text(db: &Db, uid: &str) -> String {
    cat(db, uid).text
}

fn edit_at(db: &Db, uid: &str, base: &str, e: Edit) -> Result<Written> {
    let (uid, base) = (uid.to_owned(), base.to_owned());
    db.write_txn("t", move |c, m| super::edit(c, m, &uid, &base, &e))
}

fn replace(find: &str, with: &str) -> Edit {
    Edit::Replace {
        find: find.to_owned(),
        occurrence: None,
        with: with.to_owned(),
    }
}

fn insert_after(find: &str, text: &str) -> Edit {
    Edit::InsertAfter {
        find: find.to_owned(),
        occurrence: None,
        text: text.to_owned(),
    }
}

fn span(db: &Db, uid: &str, quote: &str, reviewer: Reviewer) -> Result<String> {
    let (uid, base, quote) = (uid.to_owned(), token(db, uid), quote.to_owned());
    db.write_txn("t", move |c, m| {
        add_span(c, m, &uid, &base, &quote, None, reviewer)
    })
    .map(|w| w.span.unwrap())
}

fn approve_as(db: &Db, span: &str, who: Approver) -> Result<SpanView> {
    let span = span.to_owned();
    db.write_txn("t", move |c, m| approve(c, m, &span, &who))
}

fn view_of(db: &Db, design: &str, span: &str) -> SpanView {
    let design = design.to_owned();
    db.read(move |c| spans(c, &design))
        .unwrap()
        .into_iter()
        .find(|v| v.uid == span)
        .unwrap()
}

fn rows(db: &Db, uid: &str) -> i64 {
    let uid = uid.to_owned();
    db.read(move |c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM design_updates u JOIN items i ON i.id = u.design_id
              WHERE i.uid = ?1",
            [uid],
            |r| r.get(0),
        )?)
    })
    .unwrap()
}

fn undo_last(db: &Db) -> usize {
    db.write_txn("t", crate::undo::undo_last).unwrap()
}

#[test]
fn a_design_reads_back_its_body_with_a_version_token_that_round_trips() {
    let db = db();
    let uid = create(&db, "Hello world.");
    let doc = cat(&db, &uid);
    assert_eq!(doc.text, "Hello world.");
    assert_eq!(doc.title, "Code Factory");
    assert_eq!(doc.version.seq, 1);
    assert_eq!(Version::parse(&doc.version.token()).unwrap(), doc.version);
    let listed = db.read(|c| list(c, Some("jkb"))).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].namespace.as_deref(), Some("designs/jkb"));
    assert!(db.read(|c| list(c, Some("other"))).unwrap().is_empty());
}

/// The point of D53.4's correction: an edit resolved against the version Claude read merges with an
/// edit the operator made since, even inside one paragraph, and neither is lost.
#[test]
fn an_edit_against_an_old_version_merges_with_a_newer_edit_to_the_same_paragraph() {
    let db = db();
    let uid = create(&db, "The app reads the database. It writes nothing.");
    let base = token(&db, &uid);
    // The operator, since: a word added near the start of the paragraph.
    edit_at(&db, &uid, &base, replace("The app", "The desktop app")).unwrap();
    // Claude, against the version it read: a change near the end of the same paragraph.
    let w = edit_at(
        &db,
        &uid,
        &base,
        replace("writes nothing", "writes through ops"),
    )
    .unwrap();
    assert_eq!(w.seq, Some(3));
    assert_eq!(
        text(&db, &uid),
        "The desktop app reads the database. It writes through ops."
    );
}

#[test]
fn a_quote_missing_or_ambiguous_in_the_base_is_refused_and_occurrence_picks_one() {
    let db = db();
    let uid = create(&db, "one two one two");
    let base = token(&db, &uid);
    let e = edit_at(&db, &uid, &base, replace("three", "x")).unwrap_err();
    assert!(e.to_string().contains("not in the version you read"), "{e}");
    let e = edit_at(&db, &uid, &base, replace("one", "x")).unwrap_err();
    assert!(e.to_string().contains("occurs 2 times"), "{e}");
    let e = edit_at(
        &db,
        &uid,
        &base,
        Edit::Replace {
            find: "one".into(),
            occurrence: Some(3),
            with: "x".into(),
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains("--occurrence 3"), "{e}");
    edit_at(
        &db,
        &uid,
        &base,
        Edit::Replace {
            find: "one".into(),
            occurrence: Some(2),
            with: "ONE".into(),
        },
    )
    .unwrap();
    assert_eq!(text(&db, &uid), "one two ONE two");
}

/// The quote is matched in the base, never in the latest text: text that only exists now is not a
/// quote Claude can have read.
#[test]
fn a_quote_is_matched_in_the_version_read_not_the_latest() {
    let db = db();
    let uid = create(&db, "alpha");
    let base = token(&db, &uid);
    edit_at(&db, &uid, &base, insert_after("alpha", " beta")).unwrap();
    let e = edit_at(&db, &uid, &base, replace("beta", "gamma")).unwrap_err();
    assert!(e.to_string().contains("not in the version you read"), "{e}");
}

#[test]
fn an_edit_to_text_deleted_after_the_base_is_refused_and_writes_nothing() {
    let db = db();
    let uid = create(&db, "keep this. drop this.");
    let base = token(&db, &uid);
    edit_at(&db, &uid, &base, replace(" drop this.", "")).unwrap();
    let before = rows(&db, &uid);
    let e = edit_at(&db, &uid, &base, replace("drop", "lose")).unwrap_err();
    assert!(
        e.to_string().contains("deleted after the version you read"),
        "{e}"
    );
    assert_eq!(rows(&db, &uid), before);
    assert_eq!(text(&db, &uid), "keep this.");
}

#[test]
fn a_token_from_another_design_or_a_future_seq_is_refused() {
    let db = db();
    let a = create(&db, "aaa");
    let b = create(&db, "bbb");
    edit_at(&db, &b, &token(&db, &b), insert_after("bbb", "!")).unwrap();
    // Same seq shape as `a`'s, but `b`'s state vector.
    let mut foreign = cat(&db, &b).version;
    foreign.seq = 1;
    let e = edit_at(&db, &a, &foreign.token(), replace("aaa", "x")).unwrap_err();
    assert!(
        e.to_string().contains("not a version of this design"),
        "{e}"
    );
    let mut ahead = cat(&db, &a).version;
    ahead.seq = 9;
    let e = edit_at(&db, &a, &ahead.token(), replace("aaa", "x")).unwrap_err();
    assert!(
        e.to_string().contains("not a version of this design"),
        "{e}"
    );
    assert!(edit_at(&db, &a, "garbage", replace("aaa", "x")).is_err());
}

#[test]
fn a_base_older_than_the_last_compaction_is_refused_and_a_fresh_one_works() {
    let db = db();
    let uid = create(&db, "first");
    let old = token(&db, &uid);
    edit_at(&db, &uid, &old, insert_after("first", " second")).unwrap();
    let u = uid.clone();
    let done = db.write_txn("t", move |c, m| compact(c, m, &u)).unwrap();
    assert_eq!(
        done,
        Compacted {
            through: 2,
            removed: 2
        }
    );
    assert_eq!(rows(&db, &uid), 0);
    assert_eq!(text(&db, &uid), "first second");
    let e = edit_at(&db, &uid, &old, replace("first", "1st")).unwrap_err();
    assert!(e.to_string().contains("compaction"), "{e}");
    let w = edit_at(&db, &uid, &token(&db, &uid), replace("first", "1st")).unwrap();
    assert_eq!(w.seq, Some(3), "a seq after a compaction continues past it");
    assert_eq!(text(&db, &uid), "1st second");
    // A compaction alone is bookkeeping: a bare undo reaches past it to the edit, and the edit,
    // folded into the snapshot now, is refused by name rather than reverted on a guess.
    let u = uid.clone();
    db.write_txn("t", move |c, m| compact(c, m, &u)).unwrap();
    let e = db.write_txn("t", crate::undo::undo_last).unwrap_err();
    assert!(e.to_string().contains("compaction"), "{e}");
    assert_eq!(text(&db, &uid), "1st second");
}

/// A compaction deletes update rows, and the changelog names an update by its id: an id handed out
/// again would have `jkb undo` of the compacted update revert a newer one in its place.
#[test]
fn an_update_id_is_never_reused_after_a_compaction() {
    let db = db();
    let uid = create(&db, "abc");
    let create_txn: i64 = db
        .read(|c| Ok(c.query_row("SELECT MAX(txn_id) FROM changelog", [], |r| r.get(0))?))
        .unwrap();
    let u = uid.clone();
    db.write_txn("t", move |c, m| compact(c, m, &u)).unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("abc", "def")).unwrap();
    let e = db
        .write_txn("t", move |c, m| crate::undo::undo(c, m, create_txn))
        .unwrap_err();
    assert!(e.to_string().contains("compaction"), "{e}");
    assert_eq!(text(&db, &uid), "abcdef");
}

#[test]
fn undoing_a_compacted_update_is_refused_by_name() {
    let db = db();
    let uid = create(&db, "x");
    edit_at(&db, &uid, &token(&db, &uid), insert_after("x", "y")).unwrap();
    let txn: i64 = db
        .read(|c| Ok(c.query_row("SELECT MAX(txn_id) FROM changelog", [], |r| r.get(0))?))
        .unwrap();
    let u = uid.clone();
    db.write_txn("t", move |c, m| compact(c, m, &u)).unwrap();
    let e = db
        .write_txn("t", move |c, m| crate::undo::undo(c, m, txn))
        .unwrap_err();
    assert!(e.to_string().contains("compaction"), "{e}");
}

/// D53.4: `jkb undo` of an update is a NEW update that reverts it, never a deleted row — and it
/// reverts only that update, keeping what was written after it.
#[test]
fn undo_appends_a_forward_update_and_keeps_later_edits() {
    let db = db();
    let uid = create(&db, "The plan is short.");
    let base = token(&db, &uid);
    edit_at(&db, &uid, &base, replace("short", "long")).unwrap();
    let txn: i64 = db
        .read(|c| Ok(c.query_row("SELECT MAX(txn_id) FROM changelog", [], |r| r.get(0))?))
        .unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("plan", " (v2)")).unwrap();
    assert_eq!(text(&db, &uid), "The plan (v2) is long.");
    let before = rows(&db, &uid);
    db.write_txn("t", move |c, m| crate::undo::undo(c, m, txn))
        .unwrap();
    assert_eq!(text(&db, &uid), "The plan (v2) is short.");
    assert_eq!(
        rows(&db, &uid),
        before + 1,
        "undo deleted history instead of appending"
    );
}

#[test]
fn a_bare_undo_reverts_the_last_edit_including_a_pure_deletion() {
    let db = db();
    let uid = create(&db, "Keep. Remove me. Keep.");
    edit_at(&db, &uid, &token(&db, &uid), replace("Remove me. ", "")).unwrap();
    assert_eq!(text(&db, &uid), "Keep. Keep.");
    undo_last(&db);
    assert_eq!(text(&db, &uid), "Keep. Remove me. Keep.");
}

#[test]
fn undoing_the_create_removes_the_design() {
    let db = db();
    let uid = create(&db, "body");
    undo_last(&db);
    let u = uid.clone();
    assert!(db.read(move |c| read(c, &u)).is_err());
}

/// An app peer's update: written by a `yrs` document that loaded the state, as the editor's `yjs`
/// does — merged, stored once, and refused when it builds on text the table never got.
#[test]
fn an_editor_update_merges_once_and_one_with_missing_dependencies_is_refused() {
    let db = db();
    let uid = create(&db, "abc");
    let u = uid.clone();
    let (state, _) = db.read(move |c| super::state(c, &u, None)).unwrap();
    let peer = Crdt::new();
    peer.apply(&state).unwrap();
    let ((), first) = peer
        .change(|txn, body, _| {
            body.insert(txn, 3, "d");
            Ok(())
        })
        .unwrap();
    let ((), second) = peer
        .change(|txn, body, _| {
            body.insert(txn, 4, "e");
            Ok(())
        })
        .unwrap();
    let (first, second) = (first.unwrap(), second.unwrap());
    let apply_it = |bytes: Vec<u8>| {
        let u = uid.clone();
        db.write_txn("t", move |c, m| apply(c, m, &u, &bytes))
    };
    let e = apply_it(second.clone()).unwrap_err();
    assert!(e.to_string().contains("does not have"), "{e}");
    assert!(apply_it(first.clone()).unwrap().seq.is_some());
    assert_eq!(
        apply_it(first).unwrap().seq,
        None,
        "a re-sent update was stored twice"
    );
    apply_it(second).unwrap();
    assert_eq!(text(&db, &uid), "abcde");
    assert!(apply_it(vec![1, 2, 3]).is_err());
}

#[test]
fn every_update_is_announced_on_the_designs_topic() {
    let db = db();
    let uid = create(&db, "abc");
    let name = topic(&uid);
    assert!(name.starts_with("design/design."));
    let msgs = db
        .read(move |c| crate::mq::tail(c, &name, 10, crate::mq::now_ms()))
        .unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].payload["seq"], 1);
    let bytes = STANDARD
        .decode(msgs[0].payload["update"].as_str().unwrap())
        .unwrap();
    let peer = Crdt::new();
    peer.apply(&bytes).unwrap();
    assert_eq!(peer.text(), "abc");
}

#[test]
fn state_since_a_peers_vector_is_what_it_lacks() {
    let db = db();
    let uid = create(&db, "abc");
    let u = uid.clone();
    let (full, v1) = db.read(move |c| super::state(c, &u, None)).unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("abc", "def")).unwrap();
    let u = uid.clone();
    let sv = v1.state_vector.clone();
    let (delta, v2) = db.read(move |c| super::state(c, &u, Some(&sv))).unwrap();
    assert_eq!(v2.seq, 2);
    let peer = Crdt::new();
    peer.apply(&full).unwrap();
    peer.apply(&delta).unwrap();
    assert_eq!(peer.text(), "abcdef");
}

/// Offsets are UTF-16 in Yjs, and a byte-offset document puts a quote after an emoji in the wrong
/// place: this edits after one, and anchors a span over one.
#[test]
fn quotes_after_multibyte_text_resolve_to_the_right_characters() {
    let db = db();
    let uid = create(&db, "naïve 🦀 crab — ok");
    edit_at(&db, &uid, &token(&db, &uid), replace("crab", "Ferris")).unwrap();
    assert_eq!(text(&db, &uid), "naïve 🦀 Ferris — ok");
    let s = span(&db, &uid, "🦀 Ferris", Reviewer::Operator).unwrap();
    assert_eq!(view_of(&db, &uid, &s).text, "🦀 Ferris");
    let rendered = render(&cat(&db, &uid));
    assert_eq!(
        rendered,
        format!("naïve ⟦{s} PROPOSED⟧🦀 Ferris⟦/{s}⟧ — ok")
    );
}

#[test]
fn new_text_and_an_unapproved_span_read_as_proposed() {
    let db = db();
    let uid = create(&db, "Alpha beta gamma.");
    let s = span(&db, &uid, "beta", Reviewer::Operator).unwrap();
    let v = view_of(&db, &uid, &s);
    assert_eq!(v.state, SpanState::Proposed);
    assert!(!v.demoted);
    assert_eq!((v.start, v.end), (6, 10));
}

/// D53.5: approval attests to words. An insertion inside an approved span splits it — the new words
/// are PROPOSED, the rest stay APPROVED — while one at its edge is outside it.
#[test]
fn an_insertion_inside_an_approved_span_splits_it_and_one_at_its_edge_does_not() {
    let db = db();
    let uid = create(&db, "Intro. The app never opens the database. Outro.");
    let s = span(
        &db,
        &uid,
        "The app never opens the database.",
        Reviewer::Operator,
    )
    .unwrap();
    let v = approve_as(&db, &s, Approver::Operator).unwrap();
    assert_eq!(v.state, SpanState::Approved);
    assert_eq!(v.approved_by.as_deref(), Some("operator"));

    // At either edge: outside the span, nothing demoted.
    let w = edit_at(
        &db,
        &uid,
        &token(&db, &uid),
        insert_after("Intro. ", "Note: "),
    )
    .unwrap();
    assert!(w.demoted.is_empty(), "{w:?}");
    let w = edit_at(
        &db,
        &uid,
        &token(&db, &uid),
        insert_after("database.", " Ever."),
    )
    .unwrap();
    assert!(w.demoted.is_empty(), "{w:?}");
    let v = view_of(&db, &uid, &s);
    assert_eq!(v.text, "The app never opens the database.");
    assert_eq!(v.state, SpanState::Approved);

    // Inside: demoted, and the CLI is told.
    let w = edit_at(
        &db,
        &uid,
        &token(&db, &uid),
        insert_after("never ", "directly "),
    )
    .unwrap();
    assert_eq!(w.demoted, vec![s.clone()]);
    let v = view_of(&db, &uid, &s);
    assert!(v.demoted);
    assert_eq!(v.state, SpanState::Proposed);
    let pieces: Vec<(&str, SpanState)> = v
        .pieces
        .iter()
        .map(|p| (p.text.as_str(), p.state))
        .collect();
    assert_eq!(
        pieces,
        vec![
            ("The app never ", SpanState::Approved),
            ("directly ", SpanState::Proposed),
            ("opens the database.", SpanState::Approved),
        ]
    );
    // Re-approving attests to the new words.
    let v = approve_as(&db, &s, Approver::Operator).unwrap();
    assert_eq!(v.state, SpanState::Approved);
    assert!(!v.demoted);
}

#[test]
fn a_deletion_inside_an_approved_span_demotes_it_with_the_removed_words() {
    let db = db();
    let uid = create(&db, "It must not write.");
    let s = span(&db, &uid, "It must not write.", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let w = edit_at(&db, &uid, &token(&db, &uid), replace("not ", "")).unwrap();
    assert_eq!(w.demoted, vec![s.clone()]);
    let v = view_of(&db, &uid, &s);
    assert_eq!(v.text, "It must write.");
    let removed: Vec<&str> = v
        .pieces
        .iter()
        .filter(|p| p.removed)
        .map(|p| p.text.as_str())
        .collect();
    assert_eq!(removed, vec!["not "]);
}

#[test]
fn only_the_reviewer_a_span_names_approves_it() {
    let db = db();
    let uid = create(&db, "one. two.");
    let mine = span(&db, &uid, "one.", Reviewer::Operator).unwrap();
    let e = approve_as(&db, &mine, Approver::Claude("session:x".into())).unwrap_err();
    assert!(e.to_string().contains("only the operator"), "{e}");
    let claudes = span(&db, &uid, "two.", Reviewer::Claude).unwrap();
    let v = approve_as(&db, &claudes, Approver::Claude("session:x".into())).unwrap();
    assert_eq!(v.approved_by.as_deref(), Some("session:x"));
}

#[test]
fn spans_do_not_overlap() {
    let db = db();
    let uid = create(&db, "one two three");
    span(&db, &uid, "one two", Reviewer::Operator).unwrap();
    let e = span(&db, &uid, "two three", Reviewer::Operator).unwrap_err();
    assert!(e.to_string().contains("overlaps"), "{e}");
    span(&db, &uid, "three", Reviewer::Operator).unwrap();
}

#[test]
fn replacing_a_spans_text_reanchors_it_over_the_new_words() {
    let db = db();
    let uid = create(&db, "before [old words] after");
    let s = span(&db, &uid, "old words", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let w = edit_at(
        &db,
        &uid,
        &token(&db, &uid),
        Edit::Span {
            span: s.clone(),
            with: "new text".into(),
        },
    )
    .unwrap();
    assert_eq!(w.demoted, vec![s.clone()]);
    assert_eq!(text(&db, &uid), "before [new text] after");
    let v = view_of(&db, &uid, &s);
    assert_eq!(v.text, "new text");
    assert_eq!(v.state, SpanState::Proposed);
}

fn step(db: &Db, uid: &str) -> ItemId {
    let uid = uid.to_owned();
    db.write_txn("t", move |c, m| {
        item::upsert(
            c,
            m,
            &item::NewItem {
                uid,
                kind: STEP_KIND.into(),
                content: Some("scaffold".into()),
                content_hash: None,
                mime: None,
            },
        )
    })
    .unwrap()
}

fn task_under(db: &Db, step: ItemId, uid: &str) -> ItemId {
    let uid = uid.to_owned();
    db.write_txn("t", move |c, m| {
        let t = task::create(c, m, &task::NewTask::new(uid, "do it"))?;
        containment::contain(c, m, t, step, 0)?;
        Ok(t)
    })
    .unwrap()
}

fn stage_into(db: &Db, span: &str, step: &str) -> Result<SpanView> {
    let (span, step) = (span.to_owned(), step.to_owned());
    db.write_txn("t", move |c, m| stage(c, m, &span, &step))
}

/// STAGED and IMPLEMENTED are derived from the graph, never set: a reopened task takes IMPLEMENTED
/// back with no write to the span at all.
#[test]
fn staged_and_implemented_are_derived_from_edges_and_task_status() {
    let db = db();
    let uid = create(&db, "Build the scaffold.");
    let sp = span(&db, &uid, "Build the scaffold.", Reviewer::Operator).unwrap();
    let st = step(&db, "step:scaffold");
    let err = stage_into(&db, &sp, "step:scaffold").unwrap_err();
    assert!(err.to_string().contains("approve it first"), "{err}");
    approve_as(&db, &sp, Approver::Operator).unwrap();
    let err = stage_into(&db, &sp, &uid).unwrap_err();
    assert!(err.to_string().contains("not a plan step"), "{err}");
    assert_eq!(
        stage_into(&db, &sp, "step:scaffold").unwrap().state,
        SpanState::Staged
    );
    // A step with no tasks under it has implemented nothing.
    assert_eq!(view_of(&db, &uid, &sp).state, SpanState::Staged);
    let parent = task_under(&db, st, "task:a");
    let child = task_under(&db, parent, "task:b");
    let set = |t: ItemId, status: TaskStatus| {
        db.write_txn("t", move |c, m| task::set_status(c, m, t, status))
            .unwrap();
    };
    set(parent, TaskStatus::Done);
    assert_eq!(
        view_of(&db, &uid, &sp).state,
        SpanState::Staged,
        "a nested task is open"
    );
    set(child, TaskStatus::Done);
    let v = view_of(&db, &uid, &sp);
    assert_eq!(v.state, SpanState::Implemented);
    assert_eq!(v.steps, vec!["step:scaffold".to_owned()]);
    set(child, TaskStatus::Open);
    assert_eq!(view_of(&db, &uid, &sp).state, SpanState::Staged);
}

#[test]
fn undoing_a_span_removes_its_anchor_and_its_item() {
    let db = db();
    let uid = create(&db, "abc def");
    let s = span(&db, &uid, "def", Reviewer::Operator).unwrap();
    undo_last(&db);
    let u = uid.clone();
    assert!(db.read(move |c| spans(c, &u)).unwrap().is_empty());
    let s2 = s.clone();
    assert!(db
        .read(move |c| item::id_for_uid(c, &s2))
        .unwrap()
        .is_none());
    // …and the document no longer anchors it either.
    let u = uid.clone();
    let id = db.read(move |c| design_id(c, &u)).unwrap();
    let anchors = db
        .read(move |c| {
            let (doc, _) = load(c, id)?;
            Ok(doc.anchors(&s).is_some())
        })
        .unwrap();
    assert!(!anchors);
}

#[test]
fn undoing_an_approval_takes_it_back() {
    let db = db();
    let uid = create(&db, "abc def");
    let s = span(&db, &uid, "def", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    undo_last(&db);
    let v = view_of(&db, &uid, &s);
    assert_eq!(v.state, SpanState::Proposed);
    assert!(v.approved_by.is_none());
}

#[test]
fn a_design_with_a_document_cannot_be_removed_even_with_force() {
    let db = db();
    let uid = create(&db, "body");
    let u = uid.clone();
    let e = db
        .write_txn("t", move |c, m| {
            let id = design_id(c, &u)?;
            item::remove(c, m, id, true)
        })
        .unwrap_err();
    assert!(e.to_string().contains("design"), "{e}");
}

#[test]
fn a_revert_restores_deleted_text_beside_text_written_after_it() {
    // Pure CRDT: delete a word, write next to where it was, revert the delete.
    let doc = Crdt::new();
    doc.change(|txn, body, _| {
        body.insert(txn, 0, "one two three");
        Ok(())
    })
    .unwrap();
    let ((), delete) = doc
        .change(|txn, body, _| {
            body.remove_range(txn, 4, 4);
            Ok(())
        })
        .unwrap();
    doc.change(|txn, body, _| {
        body.insert(txn, 4, "2 ");
        Ok(())
    })
    .unwrap();
    assert_eq!(doc.text(), "one 2 three");
    let revert = doc.revert(&delete.unwrap()).unwrap().unwrap();
    let peer = Crdt::new();
    peer.apply(&doc.encode_state()).unwrap();
    assert_eq!(doc.text(), peer.text());
    assert!(doc.text().contains("two "), "{}", doc.text());
    assert!(doc.text().contains("2 "), "{}", doc.text());
    // The revert is an ordinary update: a peer that had the delete merges it to the same text.
    let other = Crdt::new();
    other.apply(&peer.encode_state()).unwrap();
    assert_eq!(other.text(), doc.text());
    assert!(!revert.is_empty());
}

/// A subscriber joins a design's topic before anything is written to it, so the topic exists from
/// the design's creation — even one created with no text, whose first update comes later.
#[test]
fn a_design_has_its_topic_from_creation_even_with_no_text() {
    let db = db();
    let uid = create(&db, "");
    let name = topic(&uid);
    let n = name.clone();
    db.write_txn("t", move |c, m| {
        mq::group_create(c, m, &n, "app", mq::Start::FromNow, mq::now_ms())
    })
    .unwrap();
    let peer = Crdt::new();
    let ((), update) = peer
        .change(|txn, body, _| {
            body.insert(txn, 0, "first words");
            Ok(())
        })
        .unwrap();
    let (u, bytes) = (uid.clone(), update.unwrap());
    let w = db
        .write_txn("t", move |c, m| apply(c, m, &u, &bytes))
        .unwrap();
    assert_eq!(w.seq, Some(1));
    let msgs = db
        .read(move |c| mq::tail(c, &name, 10, mq::now_ms()))
        .unwrap();
    assert_eq!(msgs.len(), 1);
}

fn discuss(db: &Db, uid: &str, base: Option<String>, start: u32, end: u32) -> Result<Discussion> {
    let u = uid.to_owned();
    db.read(move |c| discussion(c, &u, base.as_deref(), start, end))
}

#[test]
fn a_discussion_resolves_its_range_in_the_version_read_into_the_quote_an_edit_names() {
    let db = db();
    let uid = create(&db, "the cat and the cat");
    let base = token(&db, &uid);
    // Written after the selection was made: the range still means what the editor showed.
    edit_at(&db, &uid, &base, insert_after("and", " a dog")).unwrap();
    let d = discuss(&db, &uid, Some(base.clone()), 16, 19).unwrap();
    assert_eq!(d.quote, "cat");
    assert_eq!((d.occurrence, d.occurrences), (Some(2), 2));
    assert_eq!(d.version.token(), base);
    assert_eq!(d.title, "Code Factory");
    // What the prompt tells Claude to run lands on exactly those words, merged over the later edit.
    edit_at(
        &db,
        &uid,
        &base,
        Edit::Replace {
            find: d.quote,
            occurrence: d.occurrence,
            with: "bird".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(text(&db, &uid), "the cat and a dog the bird");
    // A quote that occurs once needs no occurrence; no base reads the current version.
    let d = discuss(&db, &uid, None, 12, 17).unwrap();
    assert_eq!(
        (d.quote.as_str(), d.occurrence, d.occurrences),
        ("a dog", None, 1)
    );
}

#[test]
fn a_discussion_is_refused_for_a_range_no_quote_could_name() {
    let db = db();
    let uid = create(&db, "aaa 🦀 end");
    let refused = |start, end, why: &str| {
        let e = discuss(&db, &uid, None, start, end).unwrap_err();
        assert!(e.to_string().contains(why), "{start}..{end}: {e}");
    };
    refused(2, 2, "empty");
    refused(3, 99, "past the end");
    // The crab is two UTF-16 units: a range ending between them splits it.
    refused(4, 5, "splits a character");
    // "aa" at 1 overlaps the match at 0, so `--find aa` could never reach it.
    refused(1, 3, "overlaps an earlier occurrence");
    let d = discuss(&db, &uid, None, 4, 6).unwrap();
    assert_eq!(d.quote, "🦀");
    // A version this design never reached.
    let future = format!("9{}", token(&db, &uid));
    let e = discuss(&db, &uid, Some(future), 0, 1).unwrap_err();
    assert!(e.to_string().contains("not a version"), "{e}");
}

#[test]
fn a_discussion_names_the_spans_its_range_touches_with_their_states() {
    let db = db();
    let uid = create(&db, "alpha beta gamma");
    let beta = span(&db, &uid, "beta", Reviewer::Claude).unwrap();
    approve_as(&db, &beta, Approver::Operator).unwrap();
    let d = discuss(&db, &uid, None, 3, 8).unwrap();
    assert_eq!(d.quote, "ha be");
    assert_eq!(d.spans.len(), 1);
    assert_eq!(d.spans[0].uid, beta);
    assert_eq!(d.spans[0].state, SpanState::Approved);
    assert_eq!(d.spans[0].reviewer, Reviewer::Claude);
    assert!(discuss(&db, &uid, None, 0, 5).unwrap().spans.is_empty());
}

// ---- execution plans (D53.6) ---------------------------------------------------------------

fn new_plan(db: &Db, design: &str, steps: &[&str]) -> Result<plan::PlanView> {
    let design = design.to_owned();
    let steps: Vec<String> = steps.iter().map(|s| (*s).to_owned()).collect();
    db.write_txn("t", move |c, m| {
        plan::create(c, m, &design, "Ship it", &steps)
    })
}

fn plans_of(db: &Db, design: &str, all: bool) -> plan::Plans {
    let design = design.to_owned();
    db.read(move |c| plan::list(c, &design, all)).unwrap()
}

fn step_id(db: &Db, uid: &str) -> ItemId {
    let uid = uid.to_owned();
    db.read(move |c| item::id_for_uid(c, &uid))
        .unwrap()
        .unwrap()
}

fn set_status(db: &Db, t: ItemId, status: TaskStatus) {
    db.write_txn("t", move |c, m| task::set_status(c, m, t, status))
        .unwrap();
}

#[test]
fn a_plan_is_contained_by_its_design_with_its_steps_in_order() {
    let db = db();
    let uid = create(&db, "Build it.");
    let p = new_plan(&db, &uid, &["scaffold", " database ", "frontend"]).unwrap();
    assert!(p.uid.starts_with("plan:ship-it-"), "{}", p.uid);
    assert_eq!(p.design, uid);
    assert_eq!(p.title, "Ship it");
    let texts: Vec<&str> = p.steps.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["scaffold", "database", "frontend"]);
    assert!(
        !p.archived,
        "a plan with no tasks is a draft, not finished work"
    );
    let plan_uid = p.uid.clone();
    let p = db
        .write_txn("t", move |c, m| plan::add_step(c, m, &plan_uid, "deploy"))
        .unwrap();
    assert_eq!(p.steps.len(), 4);
    assert_eq!(p.steps[3].text, "deploy");
    assert_eq!(plans_of(&db, &uid, false).plans, vec![p]);
}

#[test]
fn a_plan_refuses_what_is_not_a_design_and_empty_steps() {
    let db = db();
    let uid = create(&db, "x");
    let e = new_plan(&db, "design:nope", &["a"]).unwrap_err();
    assert!(e.to_string().contains("no design"), "{e}");
    let e = new_plan(&db, &uid, &["a", "  "]).unwrap_err();
    assert!(e.to_string().contains("needs some text"), "{e}");
    let many: Vec<&str> = std::iter::repeat_n("s", plan::MAX_STEPS + 1).collect();
    assert!(new_plan(&db, &uid, &many).is_err());
    let p = new_plan(&db, &uid, &["a"]).unwrap();
    let step = p.steps[0].uid.clone();
    let e = db
        .write_txn("t", move |c, m| plan::add_step(c, m, &step, "b"))
        .unwrap_err();
    assert!(e.to_string().contains("not a plan"), "{e}");
    assert_eq!(
        plans_of(&db, &uid, true).plans.len(),
        1,
        "the refused plans wrote nothing"
    );
}

/// Archived is derived on every read: every task under the plan terminal (and at least one), and
/// a reopened task brings the plan back with no write to it.
#[test]
fn a_plan_whose_tasks_are_all_terminal_is_archived_and_hidden_unless_asked() {
    let db = db();
    let uid = create(&db, "x");
    let p = new_plan(&db, &uid, &["one", "two"]).unwrap();
    let (s1, s2) = (step_id(&db, &p.steps[0].uid), step_id(&db, &p.steps[1].uid));
    let parent = task_under(&db, s1, "task:a");
    let child = task_under(&db, parent, "task:b");
    let other = task_under(&db, s2, "task:c");
    let listed = plans_of(&db, &uid, false);
    let tasks: Vec<(&str, u32)> = listed.plans[0]
        .tasks()
        .map(|t| (t.uid.as_str(), t.depth))
        .collect();
    assert_eq!(tasks, [("task:a", 0), ("task:b", 1), ("task:c", 0)]);
    let t = &listed.plans[0].steps[0].tasks[0];
    assert_eq!(t.status.as_deref(), Some("open"));
    assert!(t.strategy.starts_with("default:"), "{}", t.strategy);
    assert_eq!(t.claimed_by, None);

    set_status(&db, parent, TaskStatus::Done);
    set_status(&db, other, TaskStatus::Cancelled);
    assert!(
        !plans_of(&db, &uid, false).plans[0].archived,
        "a nested task is open"
    );
    set_status(&db, child, TaskStatus::Done);
    let hidden = plans_of(&db, &uid, false);
    assert!(hidden.plans.is_empty());
    assert_eq!(hidden.hidden, 1);
    let all = plans_of(&db, &uid, true);
    assert_eq!(all.hidden, 0);
    assert!(all.plans[0].archived);
    let shown = p.uid.clone();
    assert!(db.read(move |c| plan::show(c, &shown)).unwrap().archived);
    set_status(&db, child, TaskStatus::Open);
    assert!(!plans_of(&db, &uid, false).plans[0].archived);
}

#[test]
fn a_task_lists_its_claim_holder() {
    let db = db();
    let uid = create(&db, "x");
    let p = new_plan(&db, &uid, &["one"]).unwrap();
    let t = task_under(&db, step_id(&db, &p.steps[0].uid), "task:a");
    db.write_txn("t", move |c, m| crate::claim::claim(c, m, t, "agent-1"))
        .unwrap();
    let listed = plans_of(&db, &uid, false);
    assert_eq!(
        listed.plans[0].steps[0].tasks[0].claimed_by.as_deref(),
        Some("agent-1")
    );
}

#[test]
fn a_step_lists_the_spans_staged_into_it_and_a_task_knows_its_place() {
    let db = db();
    let uid = create(&db, "Scaffold the app. Then the database.");
    let p = new_plan(&db, &uid, &["scaffold", "database"]).unwrap();
    let sp = span(&db, &uid, "Scaffold the app.", Reviewer::Operator).unwrap();
    approve_as(&db, &sp, Approver::Operator).unwrap();
    stage_into(&db, &sp, &p.steps[0].uid).unwrap();
    let shown = plans_of(&db, &uid, false).plans.remove(0);
    let staged: Vec<&str> = shown.steps[0]
        .spans
        .iter()
        .map(|s| s.uid.as_str())
        .collect();
    assert_eq!(staged, [sp.as_str()]);
    assert!(shown.steps[1].spans.is_empty());

    let step = step_id(&db, &p.steps[0].uid);
    let parent = task_under(&db, step, "task:p");
    let child = task_under(&db, parent, "task:q");
    let place = db
        .read(move |c| plan::place_of(c, child))
        .unwrap()
        .expect("under a design");
    assert_eq!(place.design.0, uid);
    assert_eq!(
        place.plan.as_ref().map(|p| p.0.as_str()),
        Some(p.uid.as_str())
    );
    assert_eq!(place.step.as_ref().map(|s| s.1.as_str()), Some("scaffold"));
    assert_eq!(place.spans.len(), 1);

    // A one-off: directly under the design, listed beside the plans, with no step.
    let u = uid.clone();
    let d = db.read(move |c| design_id(c, &u)).unwrap();
    let one_off = task_under(&db, d, "task:one-off");
    assert_eq!(
        plans_of(&db, &uid, false)
            .tasks
            .iter()
            .map(|t| t.uid.as_str())
            .collect::<Vec<_>>(),
        ["task:one-off"]
    );
    let place = db
        .read(move |c| plan::place_of(c, one_off))
        .unwrap()
        .unwrap();
    assert!(place.plan.is_none() && place.step.is_none());
    // A task under no design has no place.
    let loose = db
        .write_txn("t", |c, m| {
            task::create(c, m, &task::NewTask::new("task:loose", "x"))
        })
        .unwrap();
    assert!(db
        .read(move |c| plan::place_of(c, loose))
        .unwrap()
        .is_none());
}

#[test]
fn undoing_a_plan_takes_back_the_plan_and_its_steps() {
    let db = db();
    let uid = create(&db, "x");
    let p = new_plan(&db, &uid, &["a", "b"]).unwrap();
    undo_last(&db);
    assert!(plans_of(&db, &uid, true).plans.is_empty());
    let step = p.steps[0].uid.clone();
    assert!(db
        .read(move |c| item::id_for_uid(c, &step))
        .unwrap()
        .is_none());
}

const SESSION: &str = "0F8FAD5B-D9CB-469F-A165-70867728950E";

fn ask(design: &str, session: &str, cwd: &str) -> prompts::NewPrompt {
    prompts::NewPrompt {
        design: design.to_owned(),
        session: session.to_owned(),
        cwd: cwd.to_owned(),
        launch: Launch::Discuss,
        subject: None,
        title: "Discuss · Code Factory".to_owned(),
    }
}

fn record(db: &Db, a: prompts::NewPrompt) -> Result<PromptRecord> {
    db.write_txn("t", move |c, m| prompts::record(c, m, &a))
        .map(|r| r.prompt)
}

fn wrote(db: &Db, a: prompts::NewPrompt) -> bool {
    db.write_txn("t", move |c, m| prompts::record(c, m, &a))
        .unwrap()
        .wrote
}

fn prompts_of(db: &Db, design: &str) -> Vec<PromptRecord> {
    let design = design.to_owned();
    db.read(move |c| prompts::list(c, &design)).unwrap()
}

#[test]
fn a_prompt_is_recorded_under_its_design_by_its_session_uuid() {
    let db = db();
    let uid = create(&db, "x");
    let p = record(&db, ask(&uid, SESSION, "/repos/jkb")).unwrap();
    let session = SESSION.to_ascii_lowercase();
    assert_eq!(p.uid, format!("prompt:{session}"));
    assert_eq!(p.session, session, "stored as claude --resume takes it");
    assert_eq!(p.design, uid);
    assert_eq!(p.cwd, "/repos/jkb");
    assert_eq!(p.launch, Launch::Discuss);
    assert_eq!(p.subject, None);
    assert_eq!(p.title, "Discuss · Code Factory");
    let prompt_uid = p.uid.clone();
    let id = db
        .read(move |c| item::id_for_uid(c, &prompt_uid))
        .unwrap()
        .unwrap();
    let design_uid = uid.clone();
    let design_item = db.read(move |c| design_id(c, &design_uid)).unwrap();
    assert_eq!(
        db.read(move |c| containment::parent(c, id)).unwrap(),
        Some(design_item),
        "contained by its design (n:1)"
    );
    assert_eq!(prompts_of(&db, &uid), vec![p.clone()]);

    // Announced on the design's topic, after the update its body was written by.
    let name = topic(&uid);
    let msgs = db
        .read(move |c| crate::mq::tail(c, &name, 10, crate::mq::now_ms()))
        .unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].kind, "update");
    assert_eq!(msgs[1].kind, "prompt");
    assert_eq!(msgs[1].payload["design"], uid.as_str());
    assert_eq!(msgs[1].payload["prompt"], p.uid.as_str());
    // Recording it again from the same place writes nothing, so announces nothing.
    assert!(!wrote(&db, ask(&uid, SESSION, "/repos/jkb")));
    let name = topic(&uid);
    assert_eq!(
        db.read(move |c| crate::mq::tail(c, &name, 10, crate::mq::now_ms()))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_session_resolves_to_the_prompt_it_was_recorded_with() {
    let db = db();
    let uid = create(&db, "x");
    let of = |session: &'static str| db.read(move |c| prompts::of_session(c, session)).unwrap();
    assert_eq!(of(SESSION), None, "nothing recorded yet");
    let p = record(&db, ask(&uid, SESSION, "/repos/jkb")).unwrap();
    assert_eq!(
        of(SESSION),
        Some(p.clone()),
        "found from any case of the uuid"
    );
    assert_eq!(of("0f8fad5b-d9cb-469f-a165-70867728950e"), Some(p));
    assert_eq!(
        of("not-a-uuid"),
        None,
        "a session no launch could have recorded is no prompt, not an error"
    );
    assert_eq!(of("00000000-0000-0000-0000-000000000000"), None);
}

#[test]
fn a_session_is_one_prompt_and_rerecording_it_moves_only_its_cwd() {
    let db = db();
    let uid = create(&db, "x");
    assert!(wrote(&db, ask(&uid, SESSION, "/repos/jkb")));
    let first = record(&db, ask(&uid, SESSION, "/repos/jkb")).unwrap();
    let mut again = ask(&uid, &SESSION.to_ascii_lowercase(), "/Users/me/repos/jkb");
    again.title = "Something else".to_owned();
    let moved = record(&db, again).unwrap();
    assert_eq!(moved.uid, first.uid);
    assert_eq!(moved.cwd, "/Users/me/repos/jkb");
    assert_eq!(moved.title, first.title, "the first launch named it");
    assert_eq!(prompts_of(&db, &uid).len(), 1);
    undo_last(&db);
    assert_eq!(
        prompts_of(&db, &uid)[0].cwd,
        "/repos/jkb",
        "the move is undone"
    );

    let other = create(&db, "y");
    let err = record(&db, ask(&other, SESSION, "/repos/jkb")).unwrap_err();
    assert!(
        err.to_string().contains("already recorded for design"),
        "{err}"
    );
    assert!(prompts_of(&db, &other).is_empty());
}

#[test]
fn a_prompt_refuses_what_resume_could_not_use() {
    let db = db();
    let uid = create(&db, "x");
    for (session, cwd, title, why) in [
        ("not-a-uuid", "/r", "t", "not a session uuid"),
        (
            "0f8fad5b-d9cb-469f-a165-70867728950",
            "/r",
            "t",
            "not a session uuid",
        ),
        (
            "0f8fad5b-d9cb-469f-a165-70867728950g",
            "/r",
            "t",
            "not a session uuid",
        ),
        (SESSION, "repos/jkb", "t", "absolute path"),
        (SESSION, "/r\0x", "t", "absolute path"),
        (SESSION, "/r", "  \n", "needs a title"),
    ] {
        let mut a = ask(&uid, session, cwd);
        a.title = title.to_owned();
        let err = record(&db, a).unwrap_err();
        assert!(err.to_string().contains(why), "{session} {cwd:?}: {err}");
    }
    let mut a = ask(&uid, SESSION, "/r");
    a.subject = Some("task:nope".to_owned());
    assert!(record(&db, a)
        .unwrap_err()
        .to_string()
        .contains("task:nope"));
    assert!(record(&db, ask("design:nope", SESSION, "/r")).is_err());
    assert!(Launch::parse("bogus")
        .unwrap_err()
        .to_string()
        .contains("bogus"));
    assert!(
        prompts_of(&db, &uid).is_empty(),
        "nothing refused was written"
    );

    let plan = new_plan(&db, &uid, &["a"]).unwrap();
    let mut a = ask(&uid, SESSION, "/r");
    a.launch = Launch::Play;
    a.subject = Some(plan.uid.clone());
    a.title = format!("{}\nsecond line", "é".repeat(300));
    let p = record(&db, a).unwrap();
    assert_eq!(p.launch, Launch::Play);
    assert_eq!(p.subject.as_deref(), Some(plan.uid.as_str()));
    assert_eq!(p.title.chars().count(), prompts::MAX_TITLE_CHARS);
    assert!(p.title.ends_with('…') && !p.title.contains('\n'));
}

#[test]
fn prompts_list_newest_first_and_undo_takes_one_back() {
    let db = db();
    let uid = create(&db, "x");
    let a = record(&db, ask(&uid, SESSION, "/r")).unwrap();
    let b = record(&db, ask(&uid, "11111111-2222-3333-4444-555555555555", "/r")).unwrap();
    // A plan contained by the same design is not one of its prompts.
    new_plan(&db, &uid, &["a"]).unwrap();
    assert_eq!(prompts_of(&db, &uid), vec![b.clone(), a.clone()]);
    undo_last(&db); // the plan
    undo_last(&db); // b
    assert_eq!(prompts_of(&db, &uid), vec![a]);
    let gone = b.uid;
    assert!(db
        .read(move |c| item::id_for_uid(c, &gone))
        .unwrap()
        .is_none());
}
