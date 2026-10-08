//! `design.*` through the backend: the wire shapes, the version-token edit, and who may approve.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use jkb_core::Db;
use serde_json::{json, Value};

use crate::rbac::{Caller, Tickets};
use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};

fn call(b: &LocalBackend, r: Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

#[allow(clippy::needless_pass_by_value)] // every call site builds its request inline
fn ok(b: &LocalBackend, r: Value) -> Response {
    call(b, r.clone()).unwrap_or_else(|e| panic!("{r}: {e:?}"))
}

struct Kb {
    db: Db,
    op: LocalBackend,
    tickets: Arc<Tickets>,
}

impl Kb {
    fn new() -> Self {
        let db = Db::open_in_memory().unwrap();
        let tickets = Arc::new(Tickets::default());
        Self {
            op: LocalBackend::new(db.clone()).with_tickets(Arc::clone(&tickets)),
            db,
            tickets,
        }
    }

    fn as_role(&self, role: &str) -> LocalBackend {
        let token = match ok(
            &self.op,
            json!({ "op": "role.grant", "role": role, "agent": format!("{role}-agent") }),
        ) {
            Response::Granted { token, .. } => token,
            other => panic!("{other:?}"),
        };
        LocalBackend::new(self.db.clone())
            .with_tickets(Arc::clone(&self.tickets))
            .with_caller(Caller::Token(token))
    }
}

fn create(b: &LocalBackend, body: &str) -> String {
    match ok(
        b,
        json!({ "op": "design.create", "repo": "jkb", "title": "Factory", "body": body }),
    ) {
        Response::DesignCreated { design } => {
            assert_eq!(design.namespace.as_deref(), Some("designs/jkb"));
            assert!(design.topic.starts_with("design/"));
            design.uid
        }
        other => panic!("{other:?}"),
    }
}

fn cat(b: &LocalBackend, uid: &str) -> crate::designs::DesignDoc {
    match ok(b, json!({ "op": "design.cat", "uid": uid })) {
        Response::DesignText { design } => *design,
        other => panic!("{other:?}"),
    }
}

fn written(r: Response) -> crate::designs::Written {
    match r {
        Response::DesignWritten { written } => written,
        other => panic!("{other:?}"),
    }
}

#[test]
fn claude_edits_by_quote_against_the_token_it_read_and_both_edits_survive() {
    let kb = Kb::new();
    let uid = create(&kb.op, "The app reads. It writes nothing.");
    let read = cat(&kb.op, &uid);
    assert_eq!(read.text, "The app reads. It writes nothing.");
    // The operator's edit lands first…
    let r = ok(
        &kb.op,
        json!({ "op": "design.edit", "uid": uid, "base": read.version,
                "edit": { "how": "insert_after", "find": "The app", "text": " (desktop)" } }),
    );
    assert!(
        r.announces_a_send(),
        "a stored update wakes the design's subscribers"
    );
    // …and Claude's, against the version it read, merges with it.
    let w = written(ok(
        &kb.op,
        json!({ "op": "design.edit", "uid": uid, "base": read.version,
                "edit": { "how": "replace", "find": "writes nothing", "with": "writes via ops" } }),
    ));
    assert_eq!(w.seq, Some(3));
    assert_eq!(
        cat(&kb.op, &uid).text,
        "The app (desktop) reads. It writes via ops."
    );
    let e = call(
        &kb.op,
        json!({ "op": "design.edit", "uid": uid, "base": read.version,
                "edit": { "how": "replace", "find": "nowhere", "with": "x" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

#[test]
fn an_editor_syncs_by_state_and_apply_in_base64() {
    let kb = Kb::new();
    let uid = create(&kb.op, "abc");
    let (update, version) = match ok(&kb.op, json!({ "op": "design.state", "uid": uid })) {
        Response::DesignUpdate {
            update, version, ..
        } => (update, version),
        other => panic!("{other:?}"),
    };
    let peer = jkb_core::design::crdt::Crdt::new();
    peer.apply(&STANDARD.decode(update).unwrap()).unwrap();
    let ((), mine) = peer
        .change(|txn, body, _| {
            yrs::Text::insert(body, txn, 3, "def");
            Ok(())
        })
        .unwrap();
    let w = written(ok(
        &kb.op,
        json!({ "op": "design.apply", "uid": uid, "update": STANDARD.encode(mine.unwrap()) }),
    ));
    assert!(w.seq.is_some());
    assert_ne!(w.version, version);
    assert_eq!(cat(&kb.op, &uid).text, "abcdef");
    let e = call(
        &kb.op,
        json!({ "op": "design.apply", "uid": uid, "update": "%%%" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

/// D53.5 through D52 RBAC: writing a design is a designer's; a span naming the operator is approved by
/// the operator alone, one naming Claude by a Claude session (recorded by its label).
#[test]
fn a_span_is_approved_only_by_the_reviewer_it_names() {
    let kb = Kb::new();
    let designer = kb.as_role("designer");
    let implementer = kb.as_role("implementer");
    let uid = create(&designer, "one. two.");
    let e = call(
        &implementer,
        json!({ "op": "design.create", "repo": "jkb", "title": "x" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    let version = cat(&designer, &uid).version;
    let mine = written(ok(
        &designer,
        json!({ "op": "design.span", "uid": uid, "base": version, "find": "one." }),
    ))
    .span
    .unwrap();
    let claudes = written(ok(
        &designer,
        json!({ "op": "design.span", "uid": uid, "base": version, "find": "two.",
                "reviewer": "claude" }),
    ))
    .span
    .unwrap();
    let e = call(&designer, json!({ "op": "design.approve", "span": mine })).unwrap_err();
    assert!(e.message.contains("only the operator"), "{e:?}");
    match ok(
        &designer,
        json!({ "op": "design.approve", "span": claudes }),
    ) {
        Response::DesignSpan { span } => {
            assert_eq!(span.state, "APPROVED");
            assert!(span.approved_by.unwrap().starts_with("grant:"));
        }
        other => panic!("{other:?}"),
    }
    match ok(&kb.op, json!({ "op": "design.approve", "span": mine })) {
        Response::DesignSpan { span } => {
            assert_eq!(span.approved_by.as_deref(), Some("operator"));
        }
        other => panic!("{other:?}"),
    }
    let marked = cat(&kb.op, &uid).marked;
    assert_eq!(
        marked,
        format!("⟦{mine} APPROVED⟧one.⟦/{mine}⟧ ⟦{claudes} APPROVED⟧two.⟦/{claudes}⟧")
    );
    // Compaction is the operator's.
    let e = call(&designer, json!({ "op": "design.compact", "uid": uid })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden);
    match ok(&kb.op, json!({ "op": "design.compact", "uid": uid })) {
        Response::DesignCompacted { through, .. } => assert_eq!(through, 3),
        other => panic!("{other:?}"),
    }
}
