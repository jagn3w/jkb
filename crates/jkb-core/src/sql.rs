//! Small SQL helpers shared across repositories.

/// Escape `%`, `_`, and `\` so a value can be embedded literally inside a
/// `LIKE … ESCAPE '\'` pattern. Namespace paths (`_sys`, `jkb-v1-foundation`) and
/// `file://` uris (filenames like `my_file.md`) routinely contain `_`, which `LIKE`
/// otherwise treats as a single-character wildcard — silently matching sibling paths.
///
/// Callers must pair the escaped value with an explicit `ESCAPE '\'` in the SQL, e.g.
/// `n.path LIKE ? ESCAPE '\'` bound with `format!("{}/%", like_escape(path))`.
#[must_use]
pub fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A list of ids bound as ONE parameter: pair it with `IN (SELECT value FROM json_each(?))`.
///
/// **Every unbounded list goes through this or [`json_strings`]**, not a placeholder per element.
/// `SQLite` refuses a statement with more than 32,766 variables, so a placeholder list failed outright
/// for a namespace or a frontier that large — a read that should have come back cut short came back
/// an internal error — and a statement whose text grows with its list defeats `prepare_cached`, one
/// cached statement per length. A list bounded by construction (a path's ancestors, a query's kinds)
/// may still use placeholders.
#[must_use]
pub fn json_ids(ids: impl IntoIterator<Item = i64>) -> String {
    let ids: Vec<i64> = ids.into_iter().collect();
    serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_owned())
}

/// [`json_ids`] for a list of strings.
#[must_use]
pub fn json_strings(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_owned())
}

#[cfg(test)]
mod tests {
    use super::like_escape;

    /// Every list query takes more ids than `SQLite`'s 32,766-variable limit — the ids need not exist;
    /// a placeholder per id refused the statement before it looked.
    #[test]
    fn every_list_query_takes_more_ids_than_sqlite_has_variables() {
        use jkb_types::{EdgeType, ItemId};
        let db = crate::Db::open_in_memory().unwrap();
        db.read(|c| {
            let ids: Vec<ItemId> = (1..=40_000).map(ItemId::new).collect();
            let uris: Vec<String> = (1..=40_000).map(|i| format!("file:///{i}")).collect();
            assert!(crate::item::get_many(c, &ids)?.is_empty());
            assert!(crate::item::derived_from(c, &ids)?.is_empty());
            assert!(crate::item::derived_kind_counts(c, &ids, "chunk")?.is_empty());
            assert!(crate::containment::child_counts(c, &ids)?.is_empty());
            assert!(crate::edge::edges_from_many(c, &ids, EdgeType::DependsOn)?.is_empty());
            assert!(crate::tag::applications_for(c, &ids)?.is_empty());
            assert!(crate::binding::items_for_uris(c, &uris)?.is_empty());
            let q = crate::query::Query {
                ids: ids.clone(),
                ..crate::query::Query::default()
            };
            assert!(q.evaluate(c)?.is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn escapes_like_metacharacters() {
        assert_eq!(like_escape("_sys"), r"\_sys");
        assert_eq!(like_escape("a_b%c"), r"a\_b\%c");
        assert_eq!(like_escape(r"back\slash"), r"back\\slash");
        assert_eq!(like_escape("plain/path"), "plain/path");
    }
}
