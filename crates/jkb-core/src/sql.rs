//! Small SQL helpers shared across repositories.

/// The `ESCAPE` clause every pattern from [`subtree_like`] and [`prefix_like`] must be paired with:
/// `ESCAPE '\'`. A macro rather than a `const` so it splices into a static SQL literal through
/// `concat!` as well as into a built one through `format!`:
///
/// ```
/// let sql = concat!("SELECT 1 WHERE ?1 LIKE ?2 ", jkb_core::like_escape_clause!());
/// assert_eq!(sql, r"SELECT 1 WHERE ?1 LIKE ?2 ESCAPE '\'");
/// ```
///
/// Without it `\` is an ordinary character in a `LIKE` pattern, so an escaped `\_` matches only
/// a literal backslash followed by any character: the predicate silently matches nothing for any
/// path holding a metacharacter, rather than erroring.
#[macro_export]
macro_rules! like_escape_clause {
    () => {
        r"ESCAPE '\'"
    };
}

/// The `LIKE` pattern matching every strict descendant of the namespace path `path`, i.e.
/// `<path>/%` with `path`'s own `LIKE` metacharacters escaped. Bind it to
/// `n.path LIKE ? ESCAPE '\'` (the clause is [`like_escape_clause!`]); pair it with an
/// `n.path = ?` bound to `path` itself when the subtree includes its root.
///
/// The escape matters because real paths hold metacharacters: every `_sys` path does, and
/// `LIKE` reads `_` as any one character, so unescaped `_sys/%` also matched `xsys/…`.
#[must_use]
pub fn subtree_like(path: &str) -> String {
    prefix_like(path, '/')
}

/// The `LIKE` pattern matching every string that begins with `prefix` followed by `sep`
/// (`<prefix><sep>%`), with `prefix`'s own `LIKE` metacharacters escaped — the general form of
/// [`subtree_like`], for prefixes that are not namespace paths (a `file://` uri's `#` fragments,
/// a chunk uid's `:` index). Bind it with [`like_escape_clause!`] after the `LIKE ?`.
///
/// `sep` is written literally, so it must not itself be `%`, `_` or `\`.
#[must_use]
pub fn prefix_like(prefix: &str, sep: char) -> String {
    debug_assert!(
        !matches!(sep, '%' | '_' | '\\'),
        "separator {sep:?} is a LIKE metacharacter"
    );
    let mut out = like_escape(prefix);
    out.push(sep);
    out.push('%');
    out
}

/// Escape `%`, `_`, and `\` so a value can be embedded literally inside a `LIKE … ESCAPE '\'`
/// pattern. Namespace paths (`_sys`, `_sys/views`) and `file://` uris (filenames like
/// `my_file.md`) routinely contain `_`, which `LIKE` otherwise treats as a single-character
/// wildcard — silently matching sibling paths.
///
/// Private on purpose: a caller holding only the escaped value still has to remember the `%`
/// suffix and the `ESCAPE` clause. [`subtree_like`] and [`prefix_like`] mint the whole pattern.
fn like_escape(s: &str) -> String {
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
/// cached statement per length. Only a list bounded by construction — a path's ancestors — may still
/// use placeholders; anything a client's text can lengthen may not (a query's kinds were exempted once,
/// and `kind:a,a,…` from a request body refused its statement). Terms that are clauses rather than
/// list elements — a query's tags — cannot be bound this way and are bounded by count instead
/// (`query::MAX_TAG_TERMS`).
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
    use super::{like_escape, prefix_like, subtree_like};

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
            let kinds: Vec<String> = (1..=40_000).map(|i| format!("k{i}")).collect();
            let q = crate::query::Query {
                kinds: kinds.clone(),
                exclude_kinds: kinds,
                ..crate::query::Query::default()
            };
            assert!(q.evaluate(c)?.is_empty());
            let tags = (0..1000)
                .map(|i| format!("tag:f{i}=v"))
                .collect::<Vec<_>>()
                .join(" ");
            let refused = crate::query::parse(&tags)?.evaluate(c).unwrap_err();
            assert!(
                matches!(
                    refused,
                    crate::Error::Types(jkb_types::Error::Validation(_))
                ),
                "too many tag terms is a refusal, not SQLite's expression-depth error: {refused}"
            );
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

    /// Every string in `candidates` that `pattern` matches under `predicate` (`?1` is the
    /// candidate, `?2` the pattern), against real `SQLite`.
    fn matching(predicate: &str, pattern: &str, candidates: &[&str]) -> Vec<String> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let sql = format!("SELECT ?1 WHERE {predicate}");
        let mut stmt = conn.prepare(&sql).unwrap();
        candidates
            .iter()
            .filter(|c| stmt.exists(rusqlite::params![c, pattern]).unwrap())
            .map(|c| (*c).to_owned())
            .collect()
    }

    /// A path holding both `LIKE` wildcards matches only its own subtree through the helper,
    /// while the same pattern built without the escape also matches siblings the wildcards fit.
    #[test]
    fn a_subtree_pattern_matches_only_its_own_subtree_when_the_path_holds_wildcards() {
        let path = "_sys/a_b%c";
        let candidates = [
            "_sys/a_b%c",          // the root itself: not a strict descendant
            "_sys/a_b%c/x",        // own subtree
            "_sys/a_b%c/y/z",      // own subtree, deeper
            "xsys/a_b%c/x",        // `_` read as a wildcard
            "_sys/aXb%c/x",        // `_` read as a wildcard
            "_sys/a_bANYTHINGc/x", // `%` read as a wildcard
            "_sys/a_b%cd/x",       // a sibling whose name only begins with the path
        ];
        let predicate = concat!("?1 LIKE ?2 ", crate::like_escape_clause!());
        assert_eq!(
            matching(predicate, &subtree_like(path), &candidates),
            ["_sys/a_b%c/x", "_sys/a_b%c/y/z"]
        );
        let unescaped = matching(predicate, &format!("{path}/%"), &candidates);
        for sibling in ["xsys/a_b%c/x", "_sys/aXb%c/x", "_sys/a_bANYTHINGc/x"] {
            assert!(
                unescaped.contains(&sibling.to_owned()),
                "{sibling} in {unescaped:?}"
            );
        }
        // The escaped pattern without its `ESCAPE` clause matches nothing at all.
        assert!(matching("?1 LIKE ?2", &subtree_like(path), &candidates).is_empty());
    }

    #[test]
    fn a_prefix_pattern_takes_its_separator_literally() {
        let uri = "file:///d/my_file%.md";
        let candidates = [
            "file:///d/my_file%.md",
            "file:///d/my_file%.md#t1",
            "file:///d/myXfileYY.md#t1",
            "file:///d/my_file%.md/t1",
        ];
        let predicate = concat!("?1 LIKE ?2 ", crate::like_escape_clause!());
        assert_eq!(
            matching(predicate, &prefix_like(uri, '#'), &candidates),
            ["file:///d/my_file%.md#t1"]
        );
        assert_eq!(subtree_like("plain/path"), "plain/path/%");
        assert_eq!(prefix_like("b3:abc", ':'), "b3:abc:%");
    }
}
