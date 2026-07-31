//! The CLI query DSL parser: a concise string into a [`Query`] (task 8.2).
//!
//! Grammar (whitespace-separated tokens; `"…"` groups spaces):
//! `kind:<k>`/`kind:<a>,<b>` (negate with `-kind:…`) `status:<s>` `resolution:<r>`
//! `priority<op><n>` `due<op><date>`/`due:today` `tag:<facet><op><value>` (negate with `-tag:…`)
//! `ns:<path>`/`ns:<path>/**` (comma-unions, repeatable)
//! `is:ready`/`is:frontier`/`is:tombstone`/`is:claimed`/`is:unclaimed`
//! `blocks:<uid>` `~"<vector term>"` and bare/quoted words for FTS.
//! Operators: `=`,`<`,`<=`,`>`,`>=` (and `:` as `=`). Malformed predicates produce
//! an actionable [`crate::Error`].

use jkb_types::{Error as TypeError, Resolution};

use super::{CmpOp, DueValue, Query, Scope, TagPred};
use crate::dsl::{has_unterminated_quote, tokenize_escaped, unquote_unescape};
use crate::Result;

/// Parse a DSL string into a [`Query`].
///
/// # Errors
/// Returns [`crate::Error::Types`] wrapping a validation error naming the offending
/// token and the expected syntax.
pub fn parse(input: &str) -> Result<Query> {
    let mut query = Query::default();
    let mut fts_terms: Vec<String> = Vec::new();
    let mut scopes: Vec<Scope> = Vec::new();

    if has_unterminated_quote(input) {
        return Err(bad(input, "unterminated `\"` quote"));
    }
    for token in tokenize_escaped(input) {
        if let Some(rest) = token.strip_prefix('~') {
            let term = unquote_unescape(rest);
            if term.is_empty() {
                return Err(bad(&token, "expected a term after `~`, e.g. `~\"topic\"`"));
            }
            query.vector = Some(term);
        } else if let Some(v) = token.strip_prefix("-kind:") {
            for part in non_empty(v, &token, "kind")?.split(',') {
                if part.is_empty() {
                    return Err(bad(&token, "empty kind in `-kind:a,b`"));
                }
                query.exclude_kinds.push(part.to_owned());
            }
        } else if let Some(v) = token.strip_prefix("kind:") {
            let v = non_empty(v, &token, "kind")?;
            // `kind:a,b` is a union; a single kind keeps using the scalar field so existing
            // callers comparing `Query::kind` are unaffected.
            if v.contains(',') {
                for part in v.split(',') {
                    if part.is_empty() {
                        return Err(bad(&token, "empty kind in `kind:a,b`"));
                    }
                    query.kinds.push(part.to_owned());
                }
            } else {
                query.kind = Some(v.to_owned());
            }
        } else if let Some(v) = token.strip_prefix("status:") {
            query.status = Some(non_empty(v, &token, "status")?.to_owned());
        } else if let Some(v) = token.strip_prefix("resolution:") {
            let v = non_empty(v, &token, "resolution")?;
            if Resolution::from_str_opt(v).is_none() {
                return Err(bad(
                    &token,
                    "resolution is one of unresolved, success, dead_end, superseded, abandoned",
                ));
            }
            query.resolution = Some(v.to_owned());
        } else if let Some(v) = token.strip_prefix("-tag:") {
            query.exclude_tags.push(parse_tag(v, &token)?);
        } else if let Some(v) = token.strip_prefix("tag:") {
            query.tags.push(parse_tag(v, &token)?);
        } else if let Some(v) = token.strip_prefix("ns:") {
            parse_ns(v, &token, &mut scopes)?;
        } else if let Some(v) = token.strip_prefix("is:") {
            match v {
                "ready" => query.ready = true,
                // `is:frontier` bundles the claim filter so handing out work is safe by
                // default (mirroring `is:ready`); a coordinator wanting the whole frontier
                // including in-flight units builds the `Query` directly.
                "frontier" => {
                    query.frontier = true;
                    query.claimed = Some(false);
                }
                "tombstone" => query.tombstone = true,
                "claimed" => query.claimed = Some(true),
                "unclaimed" => query.claimed = Some(false),
                _ => {
                    return Err(bad(
                        &token,
                        "the `is:` predicates are ready, frontier, tombstone, claimed, unclaimed",
                    ))
                }
            }
        } else if let Some(v) = token.strip_prefix("blocks:") {
            query.blocks = Some(non_empty(v, &token, "blocks")?.to_owned());
        } else if let Some((op, val)) = strip_keyword_op(&token, "priority") {
            let n = val
                .parse::<i64>()
                .map_err(|_| bad(&token, "priority takes an integer, e.g. `priority<=2`"))?;
            query.priority = Some((op, n));
        } else if let Some((op, val)) = strip_keyword_op(&token, "due") {
            if val.is_empty() {
                return Err(bad(
                    &token,
                    "due takes a date or `today`, e.g. `due<=2025-12-31`",
                ));
            }
            let due = if val == "today" {
                DueValue::Today
            } else {
                DueValue::Date(val.to_owned())
            };
            query.due = Some((op, due));
        } else {
            fts_terms.push(unquote_unescape(&token));
        }
    }

    if !fts_terms.is_empty() {
        query.fts = Some(fts_terms.join(" "));
    }
    query.scope = if scopes.len() > 1 {
        Scope::Union(scopes)
    } else {
        scopes.into_iter().next().unwrap_or(Scope::All)
    };
    Ok(query)
}

/// Parse `tag:` payload `facet<op>value`.
fn parse_tag(payload: &str, token: &str) -> Result<TagPred> {
    let op_idx = payload
        .find(['=', '<', '>'])
        .ok_or_else(|| bad(token, "tag needs an operator, e.g. `tag:read_year=2025`"))?;
    let facet = &payload[..op_idx];
    let (op, value) =
        leading_op(&payload[op_idx..]).ok_or_else(|| bad(token, "tag has a malformed operator"))?;
    if facet.is_empty() || value.is_empty() {
        return Err(bad(token, "tag needs both a facet and a value"));
    }
    Ok(TagPred {
        facet: facet.to_owned(),
        op,
        value: value.to_owned(),
    })
}

/// Parse `ns:` payload — comma-separated paths, each exact or a `/**` subtree.
fn parse_ns(payload: &str, token: &str, scopes: &mut Vec<Scope>) -> Result<()> {
    for part in payload.split(',') {
        if part.is_empty() {
            return Err(bad(token, "empty namespace in scope"));
        }
        scopes.push(match part.strip_suffix("/**") {
            Some(base) => Scope::Subtree(base.to_owned()),
            None => Scope::Exact(part.to_owned()),
        });
    }
    Ok(())
}

/// If `token` is `<keyword><op><value>`, return `(op, value)`.
fn strip_keyword_op<'a>(token: &'a str, keyword: &str) -> Option<(CmpOp, &'a str)> {
    let rest = token.strip_prefix(keyword)?;
    leading_op(rest)
}

/// Peel a leading comparison operator off `s`, returning `(op, remainder)`.
fn leading_op(s: &str) -> Option<(CmpOp, &str)> {
    for (prefix, op) in [
        ("<=", CmpOp::Le),
        (">=", CmpOp::Ge),
        ("<", CmpOp::Lt),
        (">", CmpOp::Gt),
        ("=", CmpOp::Eq),
        (":", CmpOp::Eq),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return Some((op, rest));
        }
    }
    None
}

/// Require a non-empty value for `field`, else an actionable error.
fn non_empty<'a>(value: &'a str, token: &str, field: &str) -> Result<&'a str> {
    if value.is_empty() {
        Err(bad(token, &format!("{field} needs a value")))
    } else {
        Ok(value)
    }
}

/// Build an actionable parse error naming the offending token.
fn bad(token: &str, expected: &str) -> crate::Error {
    crate::Error::Types(TypeError::Validation(format!(
        "invalid query token `{token}`: {expected}"
    )))
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::query::{CmpOp, DueValue, Scope};

    #[test]
    fn parses_structured_predicates() {
        let q = parse("kind:task status:open priority<=2 tag:size=small ns:tasks/**").unwrap();
        assert_eq!(q.kind.as_deref(), Some("task"));
        assert_eq!(q.status.as_deref(), Some("open"));
        assert_eq!(q.priority, Some((CmpOp::Le, 2)));
        assert_eq!(q.tags.len(), 1);
        assert_eq!(q.tags[0].facet, "size");
        assert_eq!(q.scope, Scope::Subtree("tasks".to_owned()));
    }

    #[test]
    fn parses_due_today_and_dates() {
        assert_eq!(
            parse("due:today").unwrap().due,
            Some((CmpOp::Eq, DueValue::Today))
        );
        assert_eq!(
            parse("due<=2025-12-31").unwrap().due,
            Some((CmpOp::Le, DueValue::Date("2025-12-31".to_owned())))
        );
    }

    #[test]
    fn parses_scope_union_and_fts_and_vector() {
        let q = parse("ns:books/**,articles/2025/** hello ~\"continuations\"").unwrap();
        assert_eq!(
            q.scope,
            Scope::Union(vec![
                Scope::Subtree("books".to_owned()),
                Scope::Subtree("articles/2025".to_owned()),
            ])
        );
        assert_eq!(q.fts.as_deref(), Some("hello"));
        assert_eq!(q.vector.as_deref(), Some("continuations"));
    }

    #[test]
    fn quoted_fts_phrase_keeps_spaces() {
        let q = parse("\"exact phrase\"").unwrap();
        assert_eq!(q.fts.as_deref(), Some("exact phrase"));
    }

    #[test]
    fn is_ready_and_blocks() {
        let q = parse("is:ready blocks:task:abc").unwrap();
        assert!(q.ready);
        assert_eq!(q.blocks.as_deref(), Some("task:abc"));
    }

    #[test]
    fn parses_the_investigation_predicates() {
        let q = parse("is:frontier resolution:unresolved -tag:staleness=stale kind:hypothesis,gap")
            .unwrap();
        assert!(q.frontier);
        assert_eq!(
            q.claimed,
            Some(false),
            "`is:frontier` must exclude claimed work, like `is:ready`"
        );
        assert_eq!(q.resolution.as_deref(), Some("unresolved"));
        assert_eq!(q.exclude_tags.len(), 1);
        assert_eq!(q.exclude_tags[0].facet, "staleness");
        assert!(q.tags.is_empty(), "`-tag:` must not also apply positively");
        assert_eq!(q.kinds, vec!["hypothesis", "gap"]);
        assert!(q.kind.is_none(), "a comma list uses `kinds`, not `kind`");

        let q = parse("-kind:reflection,view").unwrap();
        assert_eq!(q.exclude_kinds, vec!["reflection", "view"]);
        assert!(q.kind.is_none() && q.kinds.is_empty(), "negation only");

        let q = parse("is:tombstone").unwrap();
        assert!(q.tombstone);
        assert_eq!(parse("is:claimed").unwrap().claimed, Some(true));
        assert_eq!(parse("is:unclaimed").unwrap().claimed, Some(false));
    }

    #[test]
    fn unknown_resolution_and_is_predicates_are_rejected_with_the_valid_set() {
        let err = parse("resolution:refuted").unwrap_err().to_string();
        assert!(err.contains("dead_end"), "{err}");
        let err = parse("is:blocked").unwrap_err().to_string();
        assert!(err.contains("frontier"), "{err}");
    }

    #[test]
    fn priority_like_word_is_fts_not_a_predicate() {
        let q = parse("priorities").unwrap();
        assert!(q.priority.is_none());
        assert_eq!(q.fts.as_deref(), Some("priorities"));
    }

    #[test]
    fn malformed_predicates_error_with_token() {
        for bad in ["priority<=x", "tag:size", "is:done", "kind:"] {
            let err = parse(bad).unwrap_err();
            assert!(err.to_string().contains(bad) || err.to_string().contains("invalid query"));
        }
    }

    #[test]
    fn unterminated_quote_is_rejected() {
        let err = parse("hello \"unclosed phrase").unwrap_err();
        assert!(err.to_string().contains("unterminated"), "{err}");
    }
}
