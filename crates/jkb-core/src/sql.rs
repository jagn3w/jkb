//! Small SQL helpers shared across repositories.

/// The `GLOB` pattern matching every strict descendant of the namespace path `path`, i.e.
/// `<path>/*` with `path`'s own `GLOB` metacharacters escaped. Bind it to `n.path GLOB ?`; pair
/// it with an `n.path = ?` bound to `path` itself when the subtree includes its root.
///
/// `GLOB`, not `LIKE`, because `LIKE` folds ASCII case (`SQLite`'s `case_sensitive_like` is off by
/// default, and it is a deprecated connection-global switch) while every identity it was matched
/// against — a namespace path, a `file://` uri, a uid — is compared case-exactly by `=`. The two
/// arms of one `path = ? OR path LIKE ?` disagreed: syncing `tasks.md` gathered the bindings of a
/// `Tasks.md` beside it and cancelled them as tasks the file no longer held. A `GLOB` is
/// case-exact and needs no `ESCAPE` clause, so there is nothing for a caller to pair it with.
///
/// The escape matters because real paths hold metacharacters (`*`, `?` and `[` for `GLOB`); an
/// unescaped `a?b/*` would also match `axb/…`.
#[must_use]
pub fn subtree_glob(path: &str) -> String {
    prefix_glob(path, '/')
}

/// The `GLOB` pattern matching every string that begins with `prefix` followed by `sep`
/// (`<prefix><sep>*`), with `prefix`'s own `GLOB` metacharacters escaped — the general form of
/// [`subtree_glob`], for prefixes that are not namespace paths (a `file://` uri's `#` fragments,
/// a chunk uid's `:` index). Bind it to `col GLOB ?`; it matches case-exactly.
///
/// `sep` is written literally, so it must not itself be `*`, `?` or `[`.
#[must_use]
pub fn prefix_glob(prefix: &str, sep: char) -> String {
    debug_assert!(
        !matches!(sep, '*' | '?' | '['),
        "separator {sep:?} is a GLOB metacharacter"
    );
    let mut out = glob_escape(prefix);
    out.push(sep);
    out.push('*');
    out
}

/// Escape `*`, `?` and `[` so a value matches literally inside a `GLOB` pattern, by wrapping each
/// in a one-character class (`[*]`, `[?]`, `[[]`). A `]` outside a class is already literal, and
/// `GLOB` has no escape character, so nothing else needs it.
///
/// Private on purpose: [`subtree_glob`] and [`prefix_glob`] mint the whole pattern, so a caller
/// cannot hold an escaped value and forget the `*` suffix.
fn glob_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[') {
            out.push('[');
            out.push(c);
            out.push(']');
        } else {
            out.push(c);
        }
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
    use super::{glob_escape, prefix_glob, subtree_glob};

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
    fn escapes_glob_metacharacters() {
        assert_eq!(glob_escape("_sys"), "_sys");
        assert_eq!(glob_escape("a*b?c[d]"), "a[*]b[?]c[[]d]");
        assert_eq!(glob_escape(r"back\slash%_"), r"back\slash%_");
        assert_eq!(glob_escape("plain/path"), "plain/path");
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

    /// A path holding every `GLOB` and `LIKE` metacharacter matches only its own subtree.
    #[test]
    fn a_subtree_pattern_matches_only_its_own_subtree_when_the_path_holds_wildcards() {
        let path = "_sys/a*b?c[d]%_";
        let candidates = [
            "_sys/a*b?c[d]%_",     // the root itself: not a strict descendant
            "_sys/a*b?c[d]%_/x",   // own subtree
            "_sys/a*b?c[d]%_/y/z", // own subtree, deeper
            "xsys/a*b?c[d]%_/x",   // `_` read as a LIKE wildcard
            "_sys/aXXb?c[d]%_/x",  // `*` read as a wildcard
            "_sys/a*bXc[d]%_/x",   // `?` read as a wildcard
            "_sys/a*b?cd%_/x",     // `[d]` read as a class
            "_sys/aXXbYcd%_/x",    // all three read as wildcards
            "_sys/a*b?c[d]%_Z/x",  // a sibling whose name only begins with the path
            "_sys/a*b?c[d]%_/",    // the empty child is still under the separator
        ];
        assert_eq!(
            matching("?1 GLOB ?2", &subtree_glob(path), &candidates),
            [
                "_sys/a*b?c[d]%_/x",
                "_sys/a*b?c[d]%_/y/z",
                "_sys/a*b?c[d]%_/"
            ]
        );
        let unescaped = matching("?1 GLOB ?2", &format!("{path}/*"), &candidates);
        for sibling in ["_sys/a*b?cd%_/x", "_sys/aXXbYcd%_/x"] {
            assert!(
                unescaped.contains(&sibling.to_owned()),
                "{sibling} in {unescaped:?}"
            );
        }
    }

    /// The seam is case-exact, as every `=` it is paired with is: under `LIKE` (`SQLite`'s
    /// default `case_sensitive_like` off) `Tasks.md`'s fragments matched `tasks.md`'s pattern.
    #[test]
    fn a_prefix_pattern_is_case_exact() {
        let candidates = [
            "file:///d/tasks.md#t1",
            "file:///d/Tasks.md#t1",
            "file:///d/TASKS.MD#t1",
        ];
        assert_eq!(
            matching(
                "?1 GLOB ?2",
                &prefix_glob("file:///d/tasks.md", '#'),
                &candidates
            ),
            ["file:///d/tasks.md#t1"]
        );
        assert_eq!(
            matching(
                "?1 GLOB ?2",
                &subtree_glob("Notes"),
                &["Notes/a", "notes/a", "NOTES/a"]
            ),
            ["Notes/a"]
        );
        // What the seam replaced: the same prefix under LIKE folds case.
        assert_eq!(
            matching("?1 LIKE ?2", "file:///d/tasks.md#%", &candidates).len(),
            3
        );
    }

    #[test]
    fn a_prefix_pattern_takes_its_separator_literally() {
        let uri = "file:///d/my_file%*.md";
        let candidates = [
            "file:///d/my_file%*.md",
            "file:///d/my_file%*.md#t1",
            "file:///d/my_file%XX.md#t1",
            "file:///d/my_file%*.md/t1",
        ];
        assert_eq!(
            matching("?1 GLOB ?2", &prefix_glob(uri, '#'), &candidates),
            ["file:///d/my_file%*.md#t1"]
        );
        assert_eq!(subtree_glob("plain/path"), "plain/path/*");
        assert_eq!(prefix_glob("b3:abc", ':'), "b3:abc:*");
    }
}
