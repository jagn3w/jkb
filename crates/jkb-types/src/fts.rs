//! Turning the words a person typed into an FTS5 `MATCH` expression.

/// The FTS5 `MATCH` expression for `text`: every whitespace-separated word quoted as an
/// FTS5 string, so the words are implicitly `AND`ed and none of them is read as syntax.
///
/// Passed unquoted, a word FTS5 treats as an operator or punctuation (`or`, `NOT`, `a-b`,
/// `col:x`, `foo*`, a lone `"` or `(`) made the whole statement fail with `fts5: syntax
/// error`. Quoting is the FTS5-documented escape: inside `"…"` the only special character
/// is `"` itself, written twice. A quoted word the tokenizer splits (`"a-b"`) is a phrase
/// of its tokens, which is what the word meant. FTS5 query operators are therefore not
/// reachable through this function, by design: the input is search words, not a query
/// language.
///
/// `text` with no words yields `""`, the empty phrase, which FTS5 accepts and which matches
/// no row.
#[must_use]
pub fn fts5_match_expr(text: &str) -> String {
    let mut words = text.split_whitespace().peekable();
    if words.peek().is_none() {
        return "\"\"".to_owned();
    }
    let mut out = String::with_capacity(text.len() + 8);
    for (i, word) in words.enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push('"');
        out.push_str(&word.replace('"', "\"\""));
        out.push('"');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::fts5_match_expr;

    #[test]
    fn quotes_each_word_and_doubles_embedded_quotes() {
        assert_eq!(fts5_match_expr("hello"), "\"hello\"");
        assert_eq!(fts5_match_expr("  or   a-b "), "\"or\" \"a-b\"");
        assert_eq!(fts5_match_expr("say\"hi"), "\"say\"\"hi\"");
        assert_eq!(fts5_match_expr("\""), "\"\"\"\"");
    }

    #[test]
    fn no_words_is_the_empty_phrase() {
        assert_eq!(fts5_match_expr(""), "\"\"");
        assert_eq!(fts5_match_expr(" \t\n"), "\"\"");
    }
}
