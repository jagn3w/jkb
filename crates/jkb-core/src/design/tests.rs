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

/// The design a span belongs to.
fn design_of_span(db: &Db, span: &str) -> String {
    let span = span.to_owned();
    db.read(move |c| span_item(c, &span).map(|(_, _, d)| d))
        .unwrap()
}

/// Approve `span` at the version the reviewer reads now.
fn approve_as(db: &Db, span: &str, who: Approver) -> Result<SpanView> {
    let base = token(db, &design_of_span(db, span));
    approve_at(db, span, &base, who)
}

fn approve_at(db: &Db, span: &str, base: &str, who: Approver) -> Result<SpanView> {
    let (span, base) = (span.to_owned(), base.to_owned());
    db.write_txn("t", move |c, m| approve(c, m, &span, &base, &who))
}

/// An app peer holding the design as it is now.
fn peer_of(db: &Db, uid: &str) -> Crdt {
    let u = uid.to_owned();
    let (state, _) = db.read(move |c| super::state(c, &u, None)).unwrap();
    let peer = Crdt::new();
    peer.apply(&state).unwrap();
    peer
}

fn apply_bytes(db: &Db, uid: &str, bytes: Vec<u8>) -> Result<Written> {
    let u = uid.to_owned();
    db.write_txn("t", move |c, m| apply(c, m, &u, &bytes))
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
    // One rule for a demoted span: every piece reads PROPOSED, the untouched words too — the
    // editor draws from the pieces, and must agree with `render` and `stage`. The pieces still
    // split where the edit landed.
    assert_eq!(
        pieces,
        vec![
            ("The app never ", SpanState::Proposed),
            ("directly ", SpanState::Proposed),
            ("opens the database.", SpanState::Proposed),
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
    assert!(e.to_string().contains("overlap"), "{e}");
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
    let plan = new_plan(&db, &uid, &["scaffold"]).unwrap();
    let step_uid = plan.steps[0].uid.clone();
    let st = step_id(&db, &step_uid);
    let err = stage_into(&db, &sp, &step_uid).unwrap_err();
    assert!(err.to_string().contains("approve it first"), "{err}");
    approve_as(&db, &sp, Approver::Operator).unwrap();
    let err = stage_into(&db, &sp, &uid).unwrap_err();
    assert!(err.to_string().contains("not a plan step"), "{err}");
    assert_eq!(
        stage_into(&db, &sp, &step_uid).unwrap().state,
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
    assert_eq!(v.steps, vec![step_uid.clone()]);
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

/// Archived and the listing are one walk: a task the steps do not reach (here contained by the plan
/// itself, which `task::add_subtask` refuses — this test writes the row directly) is neither listed
/// nor counted.
#[test]
fn archived_is_judged_on_exactly_the_tasks_the_plan_lists() {
    let db = db();
    let uid = create(&db, "x");
    let p = new_plan(&db, &uid, &["one"]).unwrap();
    let listed = task_under(&db, step_id(&db, &p.steps[0].uid), "task:a");
    let _stray = task_under(&db, step_id(&db, &p.uid), "task:stray");
    set_status(&db, listed, TaskStatus::Done);
    let all = plans_of(&db, &uid, true);
    let uids: Vec<&str> = all.plans[0].tasks().map(|t| t.uid.as_str()).collect();
    assert_eq!(uids, ["task:a"], "the stray task is not listed");
    assert!(
        all.plans[0].archived,
        "so it cannot hold the plan open either"
    );
}

#[test]
fn a_task_cannot_be_made_a_subtask_of_a_plan_or_a_span() {
    let db = db();
    let uid = create(&db, "Scaffold the app.");
    let p = new_plan(&db, &uid, &["one"]).unwrap();
    let sp = span(&db, &uid, "Scaffold the app.", Reviewer::Operator).unwrap();
    for parent in [p.uid.clone(), sp] {
        let pid = step_id(&db, &parent);
        let e = db
            .write_txn("t", move |c, m| {
                let t = task::create(c, m, &task::NewTask::new("task:x", "x"))?;
                task::add_subtask(c, m, pid, t)
            })
            .unwrap_err();
        assert!(e.to_string().contains("holds no tasks"), "{parent}: {e}");
    }
    // A step and the design itself still take one.
    let step = step_id(&db, &p.steps[0].uid);
    let u = uid.clone();
    let d = db.read(move |c| design_id(c, &u)).unwrap();
    for (n, parent) in [step, d].into_iter().enumerate() {
        db.write_txn("t", move |c, m| {
            let t = task::create(c, m, &task::NewTask::new(format!("task:ok{n}"), "x"))?;
            task::add_subtask(c, m, parent, t)
        })
        .unwrap();
    }
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

const SESSION: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

fn ask(design: &str, session: &str, cwd: &str) -> prompts::RecordPrompt {
    prompts::RecordPrompt {
        design: design.to_owned(),
        session: session.to_owned(),
        cwd: cwd.to_owned(),
        launch: Launch::Discuss,
        subject: None,
        title: "Discuss · Code Factory".to_owned(),
    }
}

fn record(db: &Db, a: prompts::RecordPrompt) -> Result<PromptRecord> {
    db.write_txn("t", move |c, m| prompts::record(c, m, &a))
        .map(|r| r.prompt)
}

fn wrote(db: &Db, a: prompts::RecordPrompt) -> bool {
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
    assert_eq!(p.uid, format!("prompt:{SESSION}"));
    assert_eq!(
        p.session, SESSION,
        "stored as claude --session-id was given it"
    );
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
    assert_eq!(of(SESSION), Some(p.clone()));
    assert_eq!(
        of("0F8FAD5B-D9CB-469F-A165-70867728950E"),
        Some(p),
        "found from any case of the uuid"
    );
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
    let mut again = ask(&uid, SESSION, "/Users/me/repos/jkb");
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
        ("not-a-uuid", "/r", "t", "not a lowercase session uuid"),
        (
            "0f8fad5b-d9cb-469f-a165-70867728950",
            "/r",
            "t",
            "not a lowercase session uuid",
        ),
        (
            "0f8fad5b-d9cb-469f-a165-70867728950g",
            "/r",
            "t",
            "not a lowercase session uuid",
        ),
        // Another spelling is refused, not rewritten: claude was handed the caller's spelling.
        (
            "0F8FAD5B-D9CB-469F-A165-70867728950E",
            "/r",
            "t",
            "not a lowercase session uuid",
        ),
        (
            " 0f8fad5b-d9cb-469f-a165-70867728950e",
            "/r",
            "t",
            "not a lowercase session uuid",
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
    a.launch = Launch::Task;
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

/// A *Play* names one of this design's plans, a task's *Play* one of its tasks, and a *Discuss* or
/// *New prompt* nothing: the pane lists the subject under the design, so it must be the design's.
#[test]
fn a_prompt_subject_is_the_launchs_kind_and_this_designs() {
    let db = db();
    let uid = create(&db, "x");
    let other = create(&db, "y");
    let plan = new_plan(&db, &uid, &["a"]).unwrap();
    let step = step_id(&db, &plan.steps[0].uid);
    let mine = task_under(&db, step, "task:mine");
    // A one-off, directly under the design.
    let design_uid = uid.clone();
    let design_item = db.read(move |c| design_id(c, &design_uid)).unwrap();
    task_under(&db, design_item, "task:direct");
    let theirs = new_plan(&db, &other, &["b"]).unwrap();
    let their_step = step_id(&db, &theirs.steps[0].uid);
    task_under(&db, their_step, "task:theirs");
    db.write_txn("t", |c, m| {
        task::create(c, m, &task::NewTask::new("task:loose", "x"))
    })
    .unwrap();

    let try_record = |launch: Launch, subject: &str| {
        let mut a = ask(&uid, SESSION, "/r");
        a.launch = launch;
        a.subject = Some(subject.to_owned());
        record(&db, a).map(|_| ()).map_err(|e| e.to_string())
    };
    for (launch, subject, why) in [
        (Launch::Play, theirs.uid.as_str(), "not this design's"),
        (Launch::Task, "task:theirs", "not this design's"),
        (Launch::Task, "task:loose", "not this design's"),
        (Launch::Play, "task:mine", "is a task"),
        (Launch::Task, plan.uid.as_str(), "is a exec_plan"),
        (Launch::Play, uid.as_str(), "is a design"),
        (Launch::Discuss, plan.uid.as_str(), "names no subject"),
        (Launch::New, "task:mine", "names no subject"),
    ] {
        let err = try_record(launch, subject).unwrap_err();
        assert!(err.contains(why), "{launch:?} {subject}: {err}");
    }
    // A Play and a task's Play are started on something: naming nothing is refused by name.
    for launch in [Launch::Play, Launch::Task] {
        for subject in [None, Some("  ")] {
            let mut a = ask(&uid, SESSION, "/r");
            a.launch = launch;
            a.subject = subject.map(str::to_owned);
            let err = record(&db, a).unwrap_err().to_string();
            assert!(err.contains("name it with a subject"), "{launch:?}: {err}");
        }
    }
    assert!(
        prompts_of(&db, &uid).is_empty(),
        "nothing refused was written"
    );

    try_record(Launch::Task, "task:mine").unwrap();
    // The task then leaves the design: recording the session again (the terminal's toggle) still
    // moves its cwd, since a re-record writes only that and its subject was checked when first
    // recorded.
    db.write_txn("t", move |c, m| {
        containment::contain(c, m, mine, their_step, 0)
    })
    .unwrap();
    let mut again = ask(&uid, SESSION, "/Users/me/r");
    again.launch = Launch::Task;
    again.subject = Some("task:mine".to_owned());
    let moved = record(&db, again).unwrap();
    assert_eq!(moved.cwd, "/Users/me/r");
    assert_eq!(moved.subject.as_deref(), Some("task:mine"));
    for (session, launch, subject) in [
        (
            "11111111-2222-3333-4444-555555555555",
            Launch::Task,
            "task:direct",
        ),
        (
            "22222222-2222-3333-4444-555555555555",
            Launch::Play,
            plan.uid.as_str(),
        ),
    ] {
        let mut a = ask(&uid, session, "/r");
        a.launch = launch;
        a.subject = Some(subject.to_owned());
        record(&db, a).unwrap();
    }
    assert_eq!(prompts_of(&db, &uid).len(), 3);
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

// ---- export (D55.5–6) ---------------------------------------------------------------------------

fn exported(db: &Db, uid: &str) -> Exported {
    let uid = uid.to_owned();
    db.read(move |c| export::export(c, &uid)).unwrap()
}

/// The body after the generated header, which must parse, name `uid` and hash the body.
fn exported_body(db: &Db, uid: &str) -> String {
    let file = exported(db, uid).text;
    let generated = export::parse(&file);
    let export::Generated::File {
        uid: named, body, ..
    } = generated
    else {
        panic!("not a generated file: {file}");
    };
    assert_eq!(named, uid);
    assert!(generated.intact(), "the header hashes the body: {file}");
    body.to_owned()
}

fn set_target(db: &Db, uid: &str, path: &str) -> Result<i64> {
    let (uid, path) = (uid.to_owned(), path.to_owned());
    db.write_txn("t", move |c, m| {
        export::set_doc_target(c, m, &uid, &path)?;
        Ok(m.txn_id)
    })
}

fn changelog_rows(db: &Db) -> i64 {
    db.read(|c| Ok(c.query_row("SELECT count(*) FROM changelog", [], |r| r.get(0))?))
        .unwrap()
}

fn undo_txn(db: &Db, txn: i64) -> Result<usize> {
    db.write_txn("t", move |c, m| crate::undo::undo(c, m, txn))
}

/// D55.6: only approved words reach the file. Uncovered text, an unapproved span, and words edited
/// into an approved span after its approval (which demote the whole span) are all PROPOSED and left
/// out; no span markers.
#[test]
fn an_export_is_the_approved_text_alone_under_a_header_naming_the_version() {
    let db = db();
    let uid = create(
        &db,
        "Intro, not decided.\n## D1\nDecided.\n## D2\nPending.\n",
    );
    assert_eq!(
        exported_body(&db, &uid),
        "",
        "nothing approved, nothing exported"
    );
    let d1 = span(&db, &uid, "## D1\nDecided.\n", Reviewer::Operator).unwrap();
    span(&db, &uid, "## D2\nPending.\n", Reviewer::Operator).unwrap();
    approve_as(&db, &d1, Approver::Operator).unwrap();
    assert_eq!(exported_body(&db, &uid), "## D1\nDecided.\n");
    let file = exported(&db, &uid);
    assert!(!file.text.contains('⟦'), "no span markers: {}", file.text);
    assert_eq!(
        file.text.lines().next().unwrap(),
        export::header(&uid, &cat(&db, &uid).version, "## D1\nDecided.\n")
    );
    // Words added inside the approved span since its approval demote the whole span, so none of
    // it is approved text any more (every piece of a demoted span reads PROPOSED, D53.5).
    edit_at(
        &db,
        &uid,
        &token(&db, &uid),
        insert_after("Decided", " maybe"),
    )
    .unwrap();
    assert_eq!(exported_body(&db, &uid), "");
    // ...and the header names the new version, so a file rendered before reads as drift.
    assert_eq!(
        exported(&db, &uid).text.lines().next().unwrap(),
        export::header(&uid, &cat(&db, &uid).version, "")
    );
}

/// D53.5: a demoted span is PROPOSED as a whole, so none of it is exported — never the approved
/// pieces that survived the edit, joined. Each edit below would otherwise publish text nobody
/// approved: a deletion inverts the sentence, a replacement leaves a hole, an insertion is split
/// around.
#[test]
fn a_demoted_span_is_not_exported_at_all() {
    for (edit, what) in [
        (replace("not ", ""), "a deletion"),
        (replace("not", "always"), "a replacement"),
        (insert_after("must", " never"), "an insertion"),
    ] {
        let db = db();
        let uid = create(&db, "Kept.\nWe must not log tokens.\n");
        let kept = span(&db, &uid, "Kept.\n", Reviewer::Operator).unwrap();
        let rule = span(&db, &uid, "We must not log tokens.\n", Reviewer::Operator).unwrap();
        approve_as(&db, &kept, Approver::Operator).unwrap();
        approve_as(&db, &rule, Approver::Operator).unwrap();
        assert_eq!(exported_body(&db, &uid), "Kept.\nWe must not log tokens.\n");
        let w = edit_at(&db, &uid, &token(&db, &uid), edit).unwrap();
        assert_eq!(w.demoted, vec![rule.clone()], "{what}");
        assert_eq!(
            exported_body(&db, &uid),
            "Kept.\n",
            "{what}: the demoted span is left out whole"
        );
        // Re-approving the new words exports them.
        approve_as(&db, &rule, Approver::Operator).unwrap();
        let body = exported_body(&db, &uid);
        assert!(body.starts_with("Kept.\nWe must "), "{what}: {body}");
    }
}

/// Skipped PROPOSED text between two approved spans leaves its line breaks (at most a paragraph
/// break) and never its words, so spans quoted without their trailing newline do not run together.
#[test]
fn a_skipped_gap_keeps_its_line_breaks_between_spans() {
    let db = db();
    let uid = create(
        &db,
        "## D1\nDecided.\n\n## D2\nPending.\n\n## D3\nAlso decided. Draft. Kept.\n",
    );
    for quote in ["## D1\nDecided.", "## D3\nAlso decided.", "Kept."] {
        let s = span(&db, &uid, quote, Reviewer::Operator).unwrap();
        approve_as(&db, &s, Approver::Operator).unwrap();
    }
    assert_eq!(
        exported_body(&db, &uid),
        "## D1\nDecided.\n\n## D3\nAlso decided. Kept.\n"
    );
}

/// A file always ends in a newline, so an editor adding one is not drift.
#[test]
fn an_export_ends_in_a_newline() {
    let db = db();
    let uid = create(&db, "Decided, no newline");
    let s = span(&db, &uid, "Decided, no newline", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    assert_eq!(exported_body(&db, &uid), "Decided, no newline\n");
}

/// The header carries the body's hash, so whether a generated file was hand-edited is answered
/// from the file alone — no database.
#[test]
fn the_header_hashes_the_body_so_a_hand_edit_is_seen_without_a_database() {
    let db = db();
    let uid = create(&db, "Decided.\n");
    let s = span(&db, &uid, "Decided.\n", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let file = exported(&db, &uid).text;
    assert!(export::parse(&file).intact());
    let edited = format!("{file}A line nobody approved.\n");
    let parsed = export::parse(&edited);
    assert!(matches!(parsed, export::Generated::File { .. }));
    assert!(!parsed.intact(), "an added line");
    assert!(!export::parse(&file.replace("Decided", "Undecided")).intact());
    // The version token is information only: a design that moved on leaves the file intact.
    let (head, body) = file.split_once('\n').unwrap();
    let token = cat(&db, &uid).version.token();
    let moved = format!("{}\n{body}", head.replace(&token, "99.AAAA"));
    assert!(export::parse(&moved).intact());
}

#[test]
fn only_the_header_marks_a_generated_file() {
    let hash = export::body_hash("body");
    let generated =
        format!("<!-- generated from jkb design design:a-1, edit there (version 1.x, blake3 {hash}) -->\nbody");
    assert_eq!(export::generated_from(&generated), Some("design:a-1"));
    assert!(export::parse(&generated).intact());
    assert_eq!(export::parse("# Hand-written\n"), export::Generated::Hand);
    assert_eq!(export::parse(""), export::Generated::Hand);
    // Prose that mentions the header mid-line, or a fenced block that shows it, is hand-written.
    assert_eq!(
        export::parse("# T\nThe first line reads `<!-- generated from jkb design <uid>, …`.\n"),
        export::Generated::Hand
    );
    assert_eq!(
        export::parse(&format!("# T\n```\n{generated}\n```\n")),
        export::Generated::Hand
    );
    // A first line that claims to be the header but does not read is never taken as hand-written:
    // trimmed by hand, no hash, a byte-order mark or indentation in front.
    for bad in [
        "<!-- generated from jkb design design:a-1, edit there -->\nbody".to_owned(),
        "<!-- generated from jkb design design:a-1, edit there (version 1.x) -->\nbody".to_owned(),
        format!("\u{feff}{generated}"),
        format!("  {generated}"),
        "<!-- generated from jkb design\nbody".to_owned(),
        generated.replace(&hash, "XYZ"),
        // Something put above the header: a blank line, front matter.
        format!("\n{generated}"),
        format!("---\ntitle: x\n---\n{generated}"),
    ] {
        assert!(
            matches!(export::parse(&bad), export::Generated::Malformed(_)),
            "{bad:?}"
        );
        assert_eq!(export::generated_from(&bad), None);
    }
}

/// The doc target is a `docs/` file, one design's alone, recorded and undoable.
#[test]
fn a_doc_target_is_a_docs_file_one_design_owns() {
    let db = db();
    let a = create(&db, "a");
    let b = create(&db, "b");
    for bad in [
        "/abs/docs/a.md",
        "docs/../x.md",
        "docs//a.md",
        "./docs/a.md",
        "README.md",
        "docs/",
        "",
    ] {
        assert!(set_target(&db, &a, bad).is_err(), "{bad:?} accepted");
    }
    set_target(&db, &a, "docs/a.md").unwrap();
    assert_eq!(exported(&db, &a).doc_target.as_deref(), Some("docs/a.md"));
    assert_eq!(exported(&db, &a).repo.as_deref(), Some("jkb"));
    let e = set_target(&db, &b, "docs/a.md").unwrap_err().to_string();
    assert!(e.contains(&a), "{e}");
    // A path is relative to the design's repo: another repo's design may use the same one.
    let web = db
        .write_txn("t", |c, m| super::create(c, m, "web", "Web", "w"))
        .unwrap()
        .uid;
    set_target(&db, &web, "docs/a.md").unwrap();
    assert_eq!(exported(&db, &web).repo.as_deref(), Some("web"));
    // Setting it again is no write: no changelog row, so undo takes back the write before it.
    let rows = changelog_rows(&db);
    set_target(&db, &a, "docs/a.md").unwrap();
    assert_eq!(changelog_rows(&db), rows, "a repeat logs nothing");
    set_target(&db, &a, "docs/a2.md").unwrap();
    let listed = db.read(|c| list(c, Some("jkb"))).unwrap();
    let row = listed.iter().find(|d| d.uid == a).unwrap();
    assert_eq!(row.meta.doc_target.as_deref(), Some("docs/a2.md"));
    undo_last(&db);
    assert_eq!(exported(&db, &a).doc_target.as_deref(), Some("docs/a.md"));
    // `exports` renders exactly the designs that have a target.
    let all = db.read(|c| export::exports(c, Some("jkb"))).unwrap();
    assert_eq!(
        all.iter().map(|e| e.uid.as_str()).collect::<Vec<_>>(),
        vec![a.as_str()]
    );
}

/// Each key is its own write: undoing an older doc-target write keeps sources recorded since.
#[test]
fn undoing_a_doc_target_keeps_the_sources_recorded_after_it() {
    let db = db();
    let a = create(&db, "a");
    let target_txn = set_target(&db, &a, "docs/a.md").unwrap();
    let source = Source {
        path: "README.md".into(),
        blake3: crate::blob::hash_bytes(b"r"),
    };
    {
        let (a, source) = (a.clone(), source.clone());
        db.write_txn("t", move |c, m| export::add_sources(c, m, &a, &[source]))
            .unwrap();
    }
    undo_txn(&db, target_txn).unwrap();
    let meta = {
        let a = a.clone();
        db.read(move |c| export::meta(c, &a)).unwrap()
    };
    assert_eq!(meta.doc_target, None, "the target write is taken back");
    assert_eq!(meta.sources, vec![source], "the later sources are not");
}

/// Undo cannot hand one file to two designs: the target's uniqueness is the table's, and an undo
/// that would break it is refused with nothing changed.
#[test]
fn undo_cannot_give_two_designs_one_doc_target() {
    let db = db();
    let a = create(&db, "a");
    let b = create(&db, "b");
    set_target(&db, &a, "docs/a.md").unwrap();
    let moved = set_target(&db, &a, "docs/a2.md").unwrap();
    set_target(&db, &b, "docs/a.md").unwrap();
    assert!(undo_txn(&db, moved).is_err(), "A back onto B's file");
    assert_eq!(exported(&db, &a).doc_target.as_deref(), Some("docs/a2.md"));
    assert_eq!(exported(&db, &b).doc_target.as_deref(), Some("docs/a.md"));
}

/// D55.5: sources are recorded by path with their blake3; recording a path again re-hashes it, and
/// recording it unchanged is no write.
#[test]
fn sources_are_recorded_by_path_and_rehashed_in_place() {
    let db = db();
    let uid = create(&db, "x");
    let add = |sources: Vec<Source>| {
        let uid = uid.clone();
        db.write_txn("t", move |c, m| export::add_sources(c, m, &uid, &sources))
    };
    let src = |path: &str, bytes: &[u8]| Source {
        path: path.to_owned(),
        blake3: crate::blob::hash_bytes(bytes),
    };
    add(vec![
        src("docs/a.md", b"a"),
        src("openspec/x/design.md", b"x"),
    ])
    .unwrap();
    let rows = changelog_rows(&db);
    add(vec![src("docs/a.md", b"a")]).unwrap();
    assert_eq!(
        changelog_rows(&db),
        rows,
        "an unchanged source logs nothing"
    );
    add(vec![src("docs/a.md", b"a2")]).unwrap();
    let meta = {
        let uid = uid.clone();
        db.read(move |c| export::meta(c, &uid)).unwrap()
    };
    assert_eq!(
        meta.sources,
        vec![src("docs/a.md", b"a2"), src("openspec/x/design.md", b"x")]
    );
    let row = {
        let uid = uid.clone();
        db.read(move |c| row(c, &uid)).unwrap()
    };
    assert_eq!(row.meta, meta);
    // Undo re-hash: the old hash comes back, the other source stays.
    undo_last(&db);
    let meta = {
        let uid = uid.clone();
        db.read(move |c| export::meta(c, &uid)).unwrap()
    };
    assert_eq!(
        meta.sources,
        vec![src("docs/a.md", b"a"), src("openspec/x/design.md", b"x")]
    );
    assert!(add(vec![Source {
        path: "a.md".into(),
        blake3: "not-hex".into()
    }])
    .is_err());
    assert!(add(vec![src("../a.md", b"a")]).is_err());
}

// ---- review round 1 (subtask 3): approval attests to a range, read-version approval, the
// ---- applied delta, the create undo, derived-state gates ---------------------------------------

/// Re-anchor `span` over UTF-16 `[start, end)` as an app peer would, through `design.apply`.
fn reanchor(db: &Db, uid: &str, span: &str, start: u32, end: u32) -> Result<Written> {
    let peer = peer_of(db, uid);
    let ((), update) = peer
        .change(|txn, body, spans| Crdt::anchor(txn, body, spans, span, start, end))
        .unwrap();
    apply_bytes(db, uid, update.unwrap())
}

/// Must-fix 1: an approval attests to the words in the span, not to whatever the anchors cover
/// later. Moving an approved span's anchors over unapproved text through `design.apply` demotes it.
#[test]
fn moving_an_approved_spans_anchors_over_other_text_demotes_it() {
    let db = db();
    let uid = create(&db, "one. two. three.");
    let a = span(&db, &uid, "one.", Reviewer::Operator).unwrap();
    approve_as(&db, &a, Approver::Operator).unwrap();
    let w = reanchor(&db, &uid, &a, 0, 16).unwrap();
    assert_eq!(w.demoted, vec![a.clone()]);
    let v = view_of(&db, &uid, &a);
    assert_eq!(v.text, "one. two. three.");
    assert_eq!(v.state, SpanState::Proposed, "{v:?}");
    assert!(v.demoted);
    let plan = new_plan(&db, &uid, &["s"]).unwrap();
    assert!(stage_into(&db, &a, &plan.steps[0].uid).is_err());
    // Shrinking the anchors off approved words demotes it too: they no longer hold what was read.
    let b = {
        let uid = create(&db, "alpha beta gamma");
        let b = span(&db, &uid, "alpha beta", Reviewer::Operator).unwrap();
        approve_as(&db, &b, Approver::Operator).unwrap();
        reanchor(&db, &uid, &b, 0, 5).unwrap();
        view_of(&db, &uid, &b)
    };
    assert_eq!(b.state, SpanState::Proposed, "{b:?}");
    // …and none of its pieces still draws as approved: the editor draws from the pieces.
    assert!(b.demoted);
    assert!(
        b.pieces.iter().all(|p| p.state == SpanState::Proposed),
        "{b:?}"
    );
}

/// Must-fix 1: the overlap rule holds for a span written through `design.apply`, too.
#[test]
fn an_applied_update_that_makes_spans_overlap_is_refused() {
    let db = db();
    let uid = create(&db, "one. two. three.");
    span(&db, &uid, "one.", Reviewer::Operator).unwrap();
    let b = span(&db, &uid, "three.", Reviewer::Operator).unwrap();
    let before = rows(&db, &uid);
    let e = reanchor(&db, &uid, &b, 0, 16).unwrap_err();
    assert!(e.to_string().contains("overlap"), "{e}");
    assert_eq!(rows(&db, &uid), before, "a refused update was stored");
    // Re-anchoring clear of the other span is fine.
    reanchor(&db, &uid, &b, 5, 16).unwrap();
}

#[derive(Debug, Clone)]
enum Op {
    Insert(usize, char),
    Delete(usize, usize),
    Reanchor(usize, usize),
}

fn op() -> impl proptest::strategy::Strategy<Value = Op> {
    use proptest::prelude::*;
    prop_oneof![
        (0usize..64, prop::char::range('A', 'Z')).prop_map(|(p, c)| Op::Insert(p, c)),
        (0usize..64, 1usize..4).prop_map(|(p, n)| Op::Delete(p, n)),
        (0usize..64, 0usize..64).prop_map(|(a, b)| Op::Reanchor(a, b)),
    ]
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

    /// The invariant approval exists for (D53.5): **a span reads approved only while its text is
    /// exactly the words that were approved**, whatever editors and peers do to the text and to the
    /// anchors since.
    #[test]
    fn a_span_reads_approved_only_while_its_text_is_the_approved_words(
        len in 6usize..24,
        lo in 0usize..24,
        width in 1usize..8,
        ops in proptest::collection::vec(op(), 1..8),
    ) {
        // Distinct characters, so every quote is unique.
        let body: String = "abcdefghijklmnopqrstuvwxyz0123456789".chars().take(len).collect();
        let lo = lo % len;
        let hi = (lo + width).min(len);
        let quote = body[lo..hi].to_owned();
        let db = db();
        let uid = create(&db, &body);
        let s = span(&db, &uid, &quote, Reviewer::Operator).unwrap();
        approve_as(&db, &s, Approver::Operator).unwrap();
        for o in ops {
            let now = text(&db, &uid);
            let n = u32::try_from(now.len()).unwrap();
            match o {
                Op::Insert(p, c) => {
                    let p = u32::try_from(p).unwrap() % (n + 1);
                    let peer = peer_of(&db, &uid);
                    let ((), u) = peer
                        .change(|txn, body, _| {
                            body.insert(txn, p, &c.to_string());
                            Ok(())
                        })
                        .unwrap();
                    apply_bytes(&db, &uid, u.unwrap()).unwrap();
                }
                Op::Delete(p, k) if n > 0 => {
                    let p = u32::try_from(p).unwrap() % n;
                    let k = u32::try_from(k).unwrap().min(n - p);
                    let peer = peer_of(&db, &uid);
                    let ((), u) = peer
                        .change(|txn, body, _| {
                            body.remove_range(txn, p, k);
                            Ok(())
                        })
                        .unwrap();
                    apply_bytes(&db, &uid, u.unwrap()).unwrap();
                }
                Op::Reanchor(a, b) if n > 0 => {
                    let a = u32::try_from(a).unwrap() % n;
                    let b = u32::try_from(b).unwrap() % n;
                    let (a, b) = (a.min(b), a.max(b) + 1);
                    reanchor(&db, &uid, &s, a, b).unwrap();
                }
                Op::Delete(..) | Op::Reanchor(..) => {}
            }
            let v = view_of(&db, &uid, &s);
            if v.state != SpanState::Proposed {
                proptest::prop_assert_eq!(&v.text, &quote, "{:?}", v);
            }
        }
    }
}

/// Must-fix 2: what `design.apply` stores is the change it made, so undoing a peer's full-state
/// update (sync step 2 carries the whole delete set) reverts only that change — the text deleted
/// long before stays deleted.
#[test]
fn undoing_a_full_state_update_reverts_only_what_it_changed() {
    let db = db();
    let uid = create(&db, "A B C");
    edit_at(&db, &uid, &token(&db, &uid), replace("B ", "")).unwrap();
    assert_eq!(text(&db, &uid), "A C");
    let peer = peer_of(&db, &uid);
    peer.change(|txn, body, _| {
        body.insert(txn, 3, "!");
        Ok(())
    })
    .unwrap();
    apply_bytes(&db, &uid, peer.encode_state()).unwrap();
    assert_eq!(text(&db, &uid), "A C!");
    undo_last(&db);
    assert_eq!(text(&db, &uid), "A C");
}

/// Must-fix 3: undoing a design's creation deletes the item, which would cascade every later update
/// away — refused once anything else wrote to the design; still allowed while nothing has.
#[test]
fn undoing_a_designs_creation_is_refused_once_it_was_written_since() {
    let db = db();
    let uid = create(&db, "body");
    let u = uid.clone();
    let created: i64 = db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT u.txn_id FROM design_updates u JOIN items i ON i.id = u.design_id
                  WHERE i.uid = ?1 AND u.seq = 1",
                [u],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("body", " more")).unwrap();
    let e = db
        .write_txn("t", move |c, m| crate::undo::undo(c, m, created))
        .unwrap_err();
    assert!(e.to_string().contains("design_updates"), "{e}");
    assert_eq!(text(&db, &uid), "body more");
}

/// Must-fix 4: an approval is of the version the reviewer read. A span changed since is refused;
/// a change elsewhere in the design is not the span's and does not stop it.
#[test]
fn approval_is_at_the_version_read_and_refused_when_the_span_changed_since() {
    let db = db();
    let uid = create(&db, "Store it in Postgres. Other.");
    let s = span(&db, &uid, "Store it in Postgres.", Reviewer::Operator).unwrap();
    let read = token(&db, &uid);
    edit_at(&db, &uid, &token(&db, &uid), replace("Postgres", "MongoDB")).unwrap();
    let e = approve_at(&db, &s, &read, Approver::Operator).unwrap_err();
    assert!(e.to_string().contains("changed since"), "{e}");
    assert_eq!(view_of(&db, &uid, &s).state, SpanState::Proposed);

    let read = token(&db, &uid);
    edit_at(&db, &uid, &token(&db, &uid), replace("Other.", "Else.")).unwrap();
    let v = approve_at(&db, &s, &read, Approver::Operator).unwrap();
    assert_eq!(v.state, SpanState::Approved);
    assert_eq!(v.text, "Store it in MongoDB.");
    assert!(approve_at(&db, &s, "nonsense", Approver::Operator).is_err());
}

/// A span whose anchors a peer removed reads PROPOSED, and `stage` agrees with that derived state.
#[test]
fn an_unanchored_approved_span_is_not_staged() {
    let db = db();
    let uid = create(&db, "one. two.");
    let s = span(&db, &uid, "one.", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let plan = new_plan(&db, &uid, &["s"]).unwrap();
    let peer = peer_of(&db, &uid);
    let sp = s.clone();
    let ((), u) = peer
        .change(|txn, _, spans| {
            yrs::Map::remove(spans, txn, &sp);
            Ok(())
        })
        .unwrap();
    apply_bytes(&db, &uid, u.unwrap()).unwrap();
    let v = view_of(&db, &uid, &s);
    assert!(!v.anchored);
    assert_eq!(v.state, SpanState::Proposed);
    let e = stage_into(&db, &s, &plan.steps[0].uid).unwrap_err();
    assert!(e.to_string().contains("PROPOSED"), "{e}");
}

/// A span whose words were all deleted covers nothing, and an approval attests to words.
#[test]
fn a_span_with_no_words_is_not_approved() {
    let db = db();
    let uid = create(&db, "aXb");
    let s = span(&db, &uid, "X", Reviewer::Operator).unwrap();
    edit_at(&db, &uid, &token(&db, &uid), replace("X", "")).unwrap();
    let v = view_of(&db, &uid, &s);
    assert_eq!((v.start, v.end), (1, 1));
    let e = approve_as(&db, &s, Approver::Operator).unwrap_err();
    assert!(e.to_string().contains("covers no words"), "{e}");
}

/// IMPLEMENTED needs every staged step to have tasks, all done: one step's finished work does not
/// stand in for another step's.
#[test]
fn implemented_needs_tasks_done_under_every_staged_step() {
    let db = db();
    let uid = create(&db, "Do both.");
    let s = span(&db, &uid, "Do both.", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let plan = new_plan(&db, &uid, &["one", "two"]).unwrap();
    stage_into(&db, &s, &plan.steps[0].uid).unwrap();
    stage_into(&db, &s, &plan.steps[1].uid).unwrap();
    let t1 = task_under(&db, step_id(&db, &plan.steps[0].uid), "task:one");
    set_status(&db, t1, TaskStatus::Done);
    assert_eq!(view_of(&db, &uid, &s).state, SpanState::Staged);
    let t2 = task_under(&db, step_id(&db, &plan.steps[1].uid), "task:two");
    set_status(&db, t2, TaskStatus::Done);
    assert_eq!(view_of(&db, &uid, &s).state, SpanState::Implemented);
}

/// A span is staged only into a step of its own design's plans.
#[test]
fn a_span_is_not_staged_into_another_designs_plan() {
    let db = db();
    let mine = create(&db, "Mine.");
    let s = span(&db, &mine, "Mine.", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let other = create(&db, "Theirs.");
    let theirs = new_plan(&db, &other, &["x"]).unwrap();
    let e = stage_into(&db, &s, &theirs.steps[0].uid).unwrap_err();
    assert!(e.to_string().contains("another design"), "{e}");
    let own = new_plan(&db, &mine, &["y"]).unwrap();
    stage_into(&db, &s, &own.steps[0].uid).unwrap();
}

/// An approval and a staging are announced on the design's topic, so a subscriber redraws states.
#[test]
fn approving_and_staging_are_announced_on_the_designs_topic() {
    let db = db();
    let uid = create(&db, "Announce me.");
    let s = span(&db, &uid, "Announce me.", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let plan = new_plan(&db, &uid, &["s"]).unwrap();
    stage_into(&db, &s, &plan.steps[0].uid).unwrap();
    let name = topic(&uid);
    let msgs = db
        .read(move |c| crate::mq::tail(c, &name, 50, crate::mq::now_ms()))
        .unwrap();
    let states: Vec<&str> = msgs
        .iter()
        .filter(|m| m.kind == "span")
        .map(|m| m.payload["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["APPROVED", "STAGED"]);
}

/// Past `COMPACT_AT` rows a write folds the older ones away, keeping the newest `COMPACT_KEEP`: the
/// text is unchanged and the newest update can still be undone.
#[test]
fn a_long_editing_session_is_compacted_as_it_goes() {
    let db = db();
    let uid = create(&db, "x");
    let n = usize::try_from(COMPACT_AT).unwrap() + 4;
    for _ in 0..n {
        edit_at(&db, &uid, &token(&db, &uid), insert_after("x", "y")).unwrap();
    }
    assert!(rows(&db, &uid) <= COMPACT_AT, "{}", rows(&db, &uid));
    let expect = format!("x{}", "y".repeat(n));
    assert_eq!(text(&db, &uid), expect);
    undo_last(&db);
    assert_eq!(text(&db, &uid), format!("x{}", "y".repeat(n - 1)));
}

#[test]
fn a_span_with_no_width_renders_its_open_marker_before_its_close() {
    let db = db();
    let uid = create(&db, "aXb");
    let s = span(&db, &uid, "X", Reviewer::Operator).unwrap();
    edit_at(&db, &uid, &token(&db, &uid), replace("X", "")).unwrap();
    assert_eq!(render(&cat(&db, &uid)), format!("a⟦{s} PROPOSED⟧⟦/{s}⟧b"));
}

// ---- review round 2 ----------------------------------------------------------------------------

/// The overlap rule holds for `jkb undo` too: reverting an update that shrank a span, after a
/// neighbour was added in the room it made, would lay the span back over the neighbour.
#[test]
fn undoing_an_anchor_shrinking_update_over_a_newer_neighbour_is_refused() {
    let db = db();
    let uid = create(&db, "one. two. three.");
    let a = span(&db, &uid, "one. two.", Reviewer::Operator).unwrap();
    reanchor(&db, &uid, &a, 0, 4).unwrap();
    let shrink: i64 = db
        .read(|c| Ok(c.query_row("SELECT MAX(txn_id) FROM changelog", [], |r| r.get(0))?))
        .unwrap();
    let b = span(&db, &uid, "two.", Reviewer::Operator).unwrap();
    let e = db
        .write_txn("t", move |c, m| crate::undo::undo(c, m, shrink))
        .unwrap_err();
    assert!(e.to_string().contains("overlap"), "{e}");
    let (va, vb) = (view_of(&db, &uid, &a), view_of(&db, &uid, &b));
    assert!(va.end <= vb.start, "{va:?} {vb:?}");
}

fn span_messages(db: &Db, uid: &str) -> Vec<String> {
    let name = topic(uid);
    db.read(move |c| crate::mq::tail(c, &name, 50, crate::mq::now_ms()))
        .unwrap()
        .into_iter()
        .filter(|m| m.kind == "span")
        .map(|m| m.payload["state"].as_str().unwrap().to_owned())
        .collect()
}

/// `jkb undo` of an approval or a staging is announced like the approval or staging was, so an
/// open editor does not keep drawing the reverted state.
#[test]
fn undoing_an_approval_or_a_staging_is_announced() {
    let db = db();
    let uid = create(&db, "Announce me.");
    let s = span(&db, &uid, "Announce me.", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let plan = new_plan(&db, &uid, &["s"]).unwrap();
    stage_into(&db, &s, &plan.steps[0].uid).unwrap();
    undo_last(&db);
    assert_eq!(view_of(&db, &uid, &s).state, SpanState::Approved);
    let plan_txn: i64 = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT MAX(txn_id) FROM changelog WHERE entity_type = 'items' AND op = 'update'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    db.write_txn("t", move |c, m| crate::undo::undo(c, m, plan_txn))
        .unwrap();
    assert_eq!(view_of(&db, &uid, &s).state, SpanState::Proposed);
    assert_eq!(
        span_messages(&db, &uid),
        ["APPROVED", "STAGED", "APPROVED", "PROPOSED"]
    );
}

/// `archived` and IMPLEMENTED read one per-step count: a plan with a step holding no tasks is not
/// archived, so it is never hidden while a span staged into that step is still STAGED.
#[test]
fn a_plan_with_an_empty_step_is_not_archived() {
    let db = db();
    let uid = create(&db, "Do both.");
    let s = span(&db, &uid, "Do both.", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let p = new_plan(&db, &uid, &["one", "two"]).unwrap();
    stage_into(&db, &s, &p.steps[1].uid).unwrap();
    let t = task_under(&db, step_id(&db, &p.steps[0].uid), "task:one");
    set_status(&db, t, TaskStatus::Done);
    assert_eq!(view_of(&db, &uid, &s).state, SpanState::Staged);
    assert!(!plans_of(&db, &uid, true).plans[0].archived);
    let t2 = task_under(&db, step_id(&db, &p.steps[1].uid), "task:two");
    set_status(&db, t2, TaskStatus::Done);
    assert_eq!(view_of(&db, &uid, &s).state, SpanState::Implemented);
    assert!(plans_of(&db, &uid, true).plans[0].archived);
}

/// Approval over text with surrogate pairs before and inside the span: the attested ids are
/// counted in UTF-16 units, as a string item's clock is, so an edit outside keeps it APPROVED and
/// one inside demotes it.
#[test]
fn approval_counts_surrogate_pairs_in_utf16_units() {
    let db = db();
    // Several items, each with surrogate pairs, before and inside the span: an offset counted in
    // anything but UTF-16 units lands on the wrong item's ids.
    let uid = create(&db, "head. Keep Ferris here. tail");
    edit_at(&db, &uid, &token(&db, &uid), insert_after("head.", " 🦀🦀")).unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("tail", " 🦀")).unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("Keep", " 🦀")).unwrap();
    edit_at(&db, &uid, &token(&db, &uid), replace("head.", "🦀 head.")).unwrap();
    assert_eq!(
        text(&db, &uid),
        "🦀 head. 🦀🦀 Keep 🦀 Ferris here. tail 🦀"
    );
    let s = span(&db, &uid, "Keep 🦀 Ferris here.", Reviewer::Operator).unwrap();
    let v = approve_as(&db, &s, Approver::Operator).unwrap();
    assert_eq!(v.state, SpanState::Approved, "{v:?}");
    assert_eq!(v.text, "Keep 🦀 Ferris here.");
    edit_at(
        &db,
        &uid,
        &token(&db, &uid),
        insert_after("🦀 head.", " 🦀🦀"),
    )
    .unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("tail 🦀", " 🦀")).unwrap();
    let v = view_of(&db, &uid, &s);
    assert_eq!(v.state, SpanState::Approved, "{v:?}");
    assert_eq!(v.text, "Keep 🦀 Ferris here.");
    let w = edit_at(&db, &uid, &token(&db, &uid), insert_after("Keep 🦀", "🦀")).unwrap();
    assert_eq!(w.demoted, vec![s.clone()]);
    let v = view_of(&db, &uid, &s);
    // The pieces split exactly at the inserted pair: an offset counted in anything but UTF-16
    // units lands elsewhere. Every piece of a demoted span reads PROPOSED.
    let split: Vec<&str> = v.pieces.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(split, ["Keep 🦀", "🦀", " Ferris here."]);
    assert!(v.pieces.iter().all(|p| p.state == SpanState::Proposed));
    // …and the words added since the approval are still told apart, by provenance (`added`).
    let added: Vec<&str> = v
        .pieces
        .iter()
        .filter(|p| p.added)
        .map(|p| p.text.as_str())
        .collect();
    assert_eq!(added, ["🦀"]);
}

// ---- review rounds 3-4: the design-only creation guard -----------------------------------------

/// A later edit that was itself undone no longer holds the design's creation: undo the edit (its
/// forward revert is no work of its own), then the creation undoes.
#[test]
fn a_designs_creation_is_undone_once_the_later_edits_are_undone() {
    let db = db();
    let uid = create(&db, "");
    let made: i64 = db
        .read(|c| Ok(c.query_row("SELECT MAX(txn_id) FROM changelog", [], |r| r.get(0))?))
        .unwrap();
    let peer = Crdt::new();
    let ((), u) = peer
        .change(|txn, body, _| {
            body.insert(txn, 0, "later words");
            Ok(())
        })
        .unwrap();
    apply_bytes(&db, &uid, u.unwrap()).unwrap();
    let e = db
        .write_txn("t", move |c, m| crate::undo::undo(c, m, made))
        .unwrap_err();
    assert!(e.to_string().contains("design_updates"), "{e}");
    undo_last(&db);
    db.write_txn("t", move |c, m| crate::undo::undo(c, m, made))
        .unwrap();
    let u = uid.clone();
    assert!(db.read(move |c| read(c, &u)).is_err());
}

/// A compaction is never undone, so a compacted design keeps its creation.
#[test]
fn a_compacted_designs_creation_is_not_undone() {
    let db = db();
    let uid = create(&db, "body");
    let made: i64 = db
        .read(|c| Ok(c.query_row("SELECT MAX(txn_id) FROM changelog", [], |r| r.get(0))?))
        .unwrap();
    let u = uid.clone();
    db.write_txn("t", move |c, m| compact(c, m, &u)).unwrap();
    let e = db
        .write_txn("t", move |c, m| crate::undo::undo(c, m, made))
        .unwrap_err();
    assert!(e.to_string().contains("compacted"), "{e}");
}

/// The compaction refusal comes first: it is permanent, so the refusal must not name a later
/// transaction to undo instead (that would undo the user's own edits for nothing).
#[test]
fn a_compacted_designs_refusal_comes_before_naming_later_work() {
    let db = db();
    let uid = create(&db, "body");
    let made: i64 = db
        .read(|c| Ok(c.query_row("SELECT MAX(txn_id) FROM changelog", [], |r| r.get(0))?))
        .unwrap();
    let u = uid.clone();
    db.write_txn("t", move |c, m| compact(c, m, &u)).unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("body", " more")).unwrap();
    let e = db
        .write_txn("t", move |c, m| crate::undo::undo(c, m, made))
        .unwrap_err()
        .to_string();
    assert!(e.contains("compacted"), "{e}");
    assert!(!e.contains("undo transaction"), "{e}");
}

/// The refusal names the newest later transaction — the one `jkb undo` takes back next.
#[test]
fn a_designs_creation_refusal_names_the_newest_later_transaction() {
    let db = db();
    let uid = create(&db, "a");
    let made: i64 = db
        .read(|c| Ok(c.query_row("SELECT MAX(txn_id) FROM changelog", [], |r| r.get(0))?))
        .unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("a", "b")).unwrap();
    edit_at(&db, &uid, &token(&db, &uid), insert_after("ab", "c")).unwrap();
    let newest: i64 = db
        .read(|c| Ok(c.query_row("SELECT MAX(txn_id) FROM design_updates", [], |r| r.get(0))?))
        .unwrap();
    let e = db
        .write_txn("t", move |c, m| crate::undo::undo(c, m, made))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&format!("undo transaction {newest} first")),
        "{e}"
    );
}

/// A `stages` edge written around `design.stage` (`jkb inv link <span> stages <step>`) into another
/// design's plan does not make the span STAGED: the state counts only its own design's steps.
#[test]
fn a_stages_edge_into_another_designs_plan_does_not_stage_the_span() {
    let db = db();
    let mine = create(&db, "Mine.");
    let s = span(&db, &mine, "Mine.", Reviewer::Operator).unwrap();
    approve_as(&db, &s, Approver::Operator).unwrap();
    let other = create(&db, "Theirs.");
    let theirs = new_plan(&db, &other, &["x"]).unwrap();
    let (span_id, step) = (step_id(&db, &s), step_id(&db, &theirs.steps[0].uid));
    db.write_txn("t", move |c, m| {
        edge::link(c, m, span_id, step, EdgeType::Stages, None)
    })
    .unwrap();
    let v = view_of(&db, &mine, &s);
    assert_eq!(v.state, SpanState::Approved);
    assert!(v.steps.is_empty(), "{v:?}");
}
