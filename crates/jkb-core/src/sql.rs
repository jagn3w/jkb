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

#[cfg(test)]
mod tests {
    use super::like_escape;

    #[test]
    fn escapes_like_metacharacters() {
        assert_eq!(like_escape("_sys"), r"\_sys");
        assert_eq!(like_escape("a_b%c"), r"a\_b\%c");
        assert_eq!(like_escape(r"back\slash"), r"back\\slash");
        assert_eq!(like_escape("plain/path"), "plain/path");
    }
}
