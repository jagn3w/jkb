//! Shared quote-aware tokenization primitives for the line-oriented DSLs.
//!
//! The CLI query DSL ([`crate::query`]), the task quick-add parser ([`crate::task`]),
//! and the `tasks` file serializer (`jkb-sync`) all split a single line into
//! whitespace-separated tokens while keeping `"…"`-quoted spans together, then strip
//! a surrounding quote pair off free words. That logic was byte-for-byte triplicated;
//! it lives here once so a fix (quoting rules, UTF-8 edge cases) lands in one place.

/// Split `input` into tokens on whitespace, keeping `"…"`-quoted spans together.
///
/// A double quote toggles "inside a quote"; whitespace inside a quote is preserved
/// in the current token, whitespace outside it ends the token. The quote characters
/// themselves are retained in the token (strip them with [`unquote`]).
#[must_use]
pub fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for c in input.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                current.push(c);
            }
            c if c.is_whitespace() && !in_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Report whether `input` opens a `"` quote it never closes.
///
/// [`tokenize`] itself is infallible — an unclosed quote simply runs to end of
/// input (the `tasks` serializer relies on this, treating a stray quote as an
/// ordinary word). The stricter line DSLs (query, quick-add) reject unterminated
/// quotes as user error; they call this first and raise their own error.
#[must_use]
pub fn has_unterminated_quote(input: &str) -> bool {
    input.chars().filter(|&c| c == '"').count() % 2 == 1
}

/// Strip a single pair of surrounding double quotes, if present.
#[must_use]
pub fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::{tokenize, unquote};

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(tokenize("a b  c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn keeps_quoted_spans_together() {
        assert_eq!(
            tokenize(r#"fix "a quoted title" done"#),
            vec!["fix", r#""a quoted title""#, "done"]
        );
    }

    #[test]
    fn detects_unterminated_quote() {
        use super::has_unterminated_quote;
        assert!(has_unterminated_quote(r#"hello "unclosed"#));
        assert!(!has_unterminated_quote(r#"hello "closed" world"#));
        assert!(!has_unterminated_quote("no quotes"));
    }

    #[test]
    fn unquote_strips_one_pair() {
        assert_eq!(unquote(r#""hello""#), "hello");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote(r#""only-open"#), r#""only-open"#);
    }
}
