//! SQL placeholder lexer for the generalized `{...}` interpolation syntax.
//!
//! Recognizes:
//!
//! - `{0}` — explicit positional, refers to a bind argument by zero-based index.
//! - `{name}` — named placeholder.
//! - `{(expr)}` — arbitrary parenthesized expression (used by the `query!` macro
//!   to evaluate Rust expressions; rejected at runtime).
//! - `{N+}`, `{name+}`, `{(expr)+}` — Kleene plus: spread an iterable, expanding
//!   to a comma-separated list of placeholders. An empty iterable expands to
//!   `NULL`.
//! - `{N*}`, `{name*}`, `{(expr)*}` — Kleene star: same as `+`, but an empty
//!   iterable expands to nothing (the surrounding SQL is the user's
//!   responsibility to make valid).
//! - `{{` / `}}` — escapes for literal `{` / `}`.
//!
//! Inside SQL string literals (`'...'`, `"..."`), line and block comments
//! (with nested block-comment support), and Postgres dollar-quoted blocks
//! (`$tag$ ... $tag$`), `{` is not interpreted. Dollar-quote skipping is
//! applied unconditionally; `$tag$ ... $tag$` is not valid SQL in
//! MySQL/SQLite, so this can only matter for queries that already would not
//! parse. The lexer recognizes only doubled-quote string escapes (`''`,
//! `""`); backslash escapes (Postgres E-strings, MySQL with default settings)
//! are not handled and will mis-tokenize if `{` falls between a backslash and
//! its escaped character.
//!
//! `{(expr)}` body parsing tracks paren depth to allow `{(foo(1, 2).bar)}`,
//! but does not skip string literals inside the body — `{(foo("a}b"))}` will
//! terminate at the inner `}`. The macro caller should extract such
//! expressions into a `let` binding first.

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;

/// Number of whitespace-separated tokens to include on each side of the error
/// position when computing [`ParseError::context`].
const NUM_CONTEXT_WORDS: usize = 3;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ParsedQuery<'a> {
    pub fragments: Vec<Fragment<'a>>,
    /// `false` if the SQL contained no `{...}` placeholders. Callers may use
    /// this to take a fast path that avoids allocating a rewritten SQL string.
    pub had_placeholder: bool,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Fragment<'a> {
    Literal(String),
    Placeholder(Placeholder<'a>),
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Placeholder<'a> {
    /// Byte range in the source SQL spanning the placeholder, including the
    /// `{` and `}` delimiters.
    pub token: Range<usize>,
    pub ident: Ident<'a>,
    pub kleene: Option<Kleene>,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Ident<'a> {
    /// Explicit positional, e.g. `{0}`, `{1}`.
    Positional(u16),
    /// Named, e.g. `{name}`.
    Named(Cow<'a, str>),
    /// Arbitrary parenthesized expression, e.g. `{(foo.bar)}`. Stores the
    /// inner text, without the surrounding parens. Resolution of this body
    /// is deferred to the consumer (the `query!` macro parses it as Rust).
    Expr(Cow<'a, str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kleene {
    /// `*` — empty iterable expands to nothing.
    Star,
    /// `+` — empty iterable expands to `NULL`.
    Plus,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ParseError {
    pub byte_position: usize,
    pub message: String,
    /// A short snippet of the source SQL around `byte_position` — roughly
    /// `NUM_CONTEXT_WORDS` whitespace-separated tokens on each side, with
    /// internal whitespace runs collapsed to a single space. Empty when the
    /// snippet would be empty (e.g. the SQL itself is empty).
    pub context: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "error parsing placeholders at byte {}: {}",
            self.byte_position, self.message
        )?;
        if !self.context.is_empty() {
            write!(f, " (near `{}`)", self.context)?;
        }
        Ok(())
    }
}

impl std::error::Error for ParseError {}

/// Parse a SQL string for `{...}` placeholders.
pub fn parse_query(sql: &str) -> Result<ParsedQuery<'_>, ParseError> {
    let bytes = sql.as_bytes();
    let mut fragments = Vec::new();
    let mut had_placeholder = false;
    let mut current = String::new();
    let mut run_start = 0;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = skip_string(bytes, i, b'\''),
            b'"' => i = skip_string(bytes, i, b'"'),
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = skip_line_comment(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_block_comment(bytes, i),
            b'$' => match try_skip_dollar_quote(bytes, i) {
                Some(end) => i = end,
                None => i += 1,
            },
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                current.push_str(&sql[run_start..i]);
                current.push('{');
                i += 2;
                run_start = i;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') => {
                current.push_str(&sql[run_start..i]);
                current.push('}');
                i += 2;
                run_start = i;
            }
            b'}' => {
                return Err(err(
                    sql,
                    i,
                    "unmatched `}` in SQL; use `}}` for a literal `}`",
                ));
            }
            b'{' => {
                current.push_str(&sql[run_start..i]);
                if !current.is_empty() {
                    fragments.push(Fragment::Literal(std::mem::take(&mut current)));
                }
                let placeholder = parse_placeholder(sql, &mut i)?;
                fragments.push(Fragment::Placeholder(placeholder));
                had_placeholder = true;
                run_start = i;
            }
            _ => i += 1,
        }
    }
    current.push_str(&sql[run_start..]);
    if !current.is_empty() {
        fragments.push(Fragment::Literal(current));
    }

    Ok(ParsedQuery {
        fragments,
        had_placeholder,
    })
}

/// Advance past a `'...'` or `"..."` string literal; returns the index just past
/// the closing quote. Standard SQL doubled-quote (`''`, `""`) is treated as
/// escape; backslash escapes are not handled (see module docs). Unterminated
/// strings advance to end-of-input — the database will error out when it tries
/// to parse the SQL.
fn skip_string(bytes: &[u8], mut i: usize, quote: u8) -> usize {
    i += 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    i
}

fn skip_line_comment(bytes: &[u8], mut i: usize) -> usize {
    i += 2;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(bytes: &[u8], mut i: usize) -> usize {
    i += 2;
    let mut depth = 1usize;
    while i < bytes.len() && depth > 0 {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

/// If `bytes[start]` opens a Postgres dollar-quoted string, returns the index
/// just past the matching close. Otherwise returns `None` and the caller
/// should treat the `$` as a regular literal byte.
fn try_skip_dollar_quote(bytes: &[u8], start: usize) -> Option<usize> {
    debug_assert_eq!(bytes[start], b'$');
    // Tag is `[A-Za-z_][A-Za-z0-9_]*`, possibly empty (so `$$` is a valid open).
    let mut tag_end = start + 1;
    if matches!(bytes.get(tag_end), Some(c) if c.is_ascii_alphabetic() || *c == b'_') {
        tag_end += 1;
        while matches!(bytes.get(tag_end), Some(c) if c.is_ascii_alphanumeric() || *c == b'_') {
            tag_end += 1;
        }
    }
    if bytes.get(tag_end) != Some(&b'$') {
        return None;
    }
    let tag = &bytes[start..=tag_end];
    let mut i = tag_end + 1;
    while i + tag.len() <= bytes.len() {
        if &bytes[i..i + tag.len()] == tag {
            return Some(i + tag.len());
        }
        i += 1;
    }
    Some(bytes.len())
}

/// Parse a placeholder body starting at `*i` (which must point at the opening `{`).
/// On return, `*i` points just past the closing `}`.
fn parse_placeholder<'a>(sql: &'a str, i: &mut usize) -> Result<Placeholder<'a>, ParseError> {
    let bytes = sql.as_bytes();
    debug_assert_eq!(bytes[*i], b'{');
    let token_start = *i;
    *i += 1;

    // Body ends at the first `}` at paren depth 0 (so `{(foo(1).bar)}` works).
    let body_start = *i;
    let mut depth = 0usize;
    while *i < bytes.len() {
        match bytes[*i] {
            b'(' => depth += 1,
            b')' if depth > 0 => depth -= 1,
            b'}' if depth == 0 => break,
            _ => {}
        }
        *i += 1;
    }
    if *i >= bytes.len() {
        return Err(err(
            sql,
            token_start,
            "unterminated `{` in SQL; expected `}`",
        ));
    }
    let body = &sql[body_start..*i];
    *i += 1;
    let token_end = *i;

    let (ident, kleene) = parse_body(sql, body, body_start)?;
    Ok(Placeholder {
        token: token_start..token_end,
        ident,
        kleene,
    })
}

fn parse_body<'a>(
    sql: &str,
    body: &'a str,
    body_start: usize,
) -> Result<(Ident<'a>, Option<Kleene>), ParseError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(err(
            sql,
            body_start,
            "empty placeholder `{}`; expected an index, name, or `(expr)`",
        ));
    }

    let (head, kleene) = match trimmed.as_bytes().last().copied() {
        Some(b'*') => (trimmed[..trimmed.len() - 1].trim_end(), Some(Kleene::Star)),
        Some(b'+') => (trimmed[..trimmed.len() - 1].trim_end(), Some(Kleene::Plus)),
        _ => (trimmed, None),
    };
    if head.is_empty() {
        return Err(err(
            sql,
            body_start,
            "Kleene marker `*`/`+` requires an expression before it",
        ));
    }

    if head.starts_with('(') {
        if !head.ends_with(')') {
            return Err(err(
                sql,
                body_start,
                "mismatched `(` in placeholder; expected `(...)`",
            ));
        }
        let inner = head[1..head.len() - 1].trim();
        return Ok((Ident::Expr(Cow::Borrowed(inner)), kleene));
    }

    if head.bytes().all(|b| b.is_ascii_digit()) {
        let idx: u16 = head.parse().map_err(|_| {
            err(
                sql,
                body_start,
                format!("positional placeholder index `{head}` is not a valid integer"),
            )
        })?;
        return Ok((Ident::Positional(idx), kleene));
    }

    if !is_ascii_ident(head) {
        return Err(err(
            sql,
            body_start,
            format!(
                "invalid placeholder body `{head}`; expected an integer, identifier, or `(expr)`"
            ),
        ));
    }
    Ok((Ident::Named(Cow::Borrowed(head)), kleene))
}

/// ASCII-only identifier check, matching `proc_macro2::Ident::new`'s constraints.
fn is_ascii_ident(s: &str) -> bool {
    let mut bs = s.bytes();
    let Some(first) = bs.next() else { return false };
    (first.is_ascii_alphabetic() || first == b'_')
        && bs.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn err(sql: &str, byte_position: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        byte_position,
        message: message.into(),
        context: error_context(sql, byte_position),
    }
}

/// Walk `sql` outward from `byte_position`, returning a snippet covering
/// roughly `NUM_CONTEXT_WORDS` whitespace-separated tokens on each side.
/// Internal whitespace runs are collapsed to a single space so the snippet
/// fits cleanly on one line.
fn error_context(sql: &str, byte_position: usize) -> String {
    if sql.is_empty() {
        return String::new();
    }

    // Index each whitespace-separated run as `(start_byte, end_byte)`. Bytes
    // are valid here because ASCII whitespace doesn't appear inside multi-byte
    // UTF-8 sequences, so word boundaries always land on char boundaries.
    let bytes = sql.as_bytes();
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        words.push((start, i));
    }
    if words.is_empty() {
        return String::new();
    }

    // Pick the word at or before `byte_position`; before any word, fall back
    // to the first.
    let target = words
        .iter()
        .rposition(|&(s, _)| s <= byte_position)
        .unwrap_or(0);
    let lo = target.saturating_sub(NUM_CONTEXT_WORDS);
    let hi = std::cmp::min(target + NUM_CONTEXT_WORDS + 1, words.len());
    let start_byte = words[lo].0;
    let end_byte = words[hi - 1].1;

    sql[start_byte..end_byte]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> ParsedQuery<'_> {
        parse_query(sql).unwrap()
    }

    fn placeholders(sql: &str) -> Vec<(Ident<'_>, Option<Kleene>)> {
        parse(sql)
            .fragments
            .into_iter()
            .filter_map(|f| match f {
                Fragment::Placeholder(p) => Some((p.ident, p.kleene)),
                _ => None,
            })
            .collect()
    }

    fn expect_named(p: &(Ident<'_>, Option<Kleene>), name: &str, kleene: Option<Kleene>) {
        match p {
            (Ident::Named(n), k) if &**n == name && *k == kleene => {}
            other => panic!("expected named({name}, {kleene:?}), got {other:?}"),
        }
    }

    #[test]
    fn empty_input() {
        let p = parse("");
        assert!(!p.had_placeholder);
        assert!(p.fragments.is_empty());
    }

    #[test]
    fn legacy_passthrough() {
        // SQL with no `{` is one literal fragment, `had_placeholder == false`.
        let p = parse("SELECT $1, ? FROM t WHERE x = 'foo'");
        assert!(!p.had_placeholder);
        assert_eq!(p.fragments.len(), 1);
        match &p.fragments[0] {
            Fragment::Literal(s) => assert_eq!(s, "SELECT $1, ? FROM t WHERE x = 'foo'"),
            _ => panic!(),
        }
    }

    #[test]
    fn placeholder_kinds() {
        let ps = placeholders("SELECT {0}, {id}, {ids*}, {jds+}, {(foo.bar)}");
        assert!(matches!(ps[0].0, Ident::Positional(0)));
        expect_named(&ps[1], "id", None);
        expect_named(&ps[2], "ids", Some(Kleene::Star));
        expect_named(&ps[3], "jds", Some(Kleene::Plus));
        match &ps[4].0 {
            Ident::Expr(e) => assert_eq!(&**e, "foo.bar"),
            _ => panic!(),
        }
    }

    #[test]
    fn positional_with_kleene() {
        // Positional placeholders with `*` / `+` markers.
        let ps = placeholders("SELECT {0*}, {1+}");
        assert!(matches!(ps[0], (Ident::Positional(0), Some(Kleene::Star))));
        assert!(matches!(ps[1], (Ident::Positional(1), Some(Kleene::Plus))));
    }

    #[test]
    fn expr_with_kleene() {
        // `{(expr)}` combined with `*` / `+`.
        let ps = placeholders("SELECT {(items.iter())*}, {(rest.ids)+}");
        match &ps[0] {
            (Ident::Expr(e), Some(Kleene::Star)) => assert_eq!(&**e, "items.iter()"),
            other => panic!("expected expr+star, got {other:?}"),
        }
        match &ps[1] {
            (Ident::Expr(e), Some(Kleene::Plus)) => assert_eq!(&**e, "rest.ids"),
            other => panic!("expected expr+plus, got {other:?}"),
        }
    }

    #[test]
    fn body_whitespace_tolerated() {
        // Whitespace inside the `{...}` body is trimmed before resolving.
        let ps = placeholders("SELECT { 0 }, { id }, { ids * }");
        assert!(matches!(ps[0].0, Ident::Positional(0)));
        expect_named(&ps[1], "id", None);
        expect_named(&ps[2], "ids", Some(Kleene::Star));
    }

    #[test]
    fn brace_escapes_collapse_outside_strings() {
        // `{{` outside a SQL string becomes a literal `{` at lex time.
        let p = parse("SELECT {{ a }}");
        assert!(!p.had_placeholder);
        match &p.fragments[0] {
            Fragment::Literal(s) => assert_eq!(s, "SELECT { a }"),
            _ => panic!(),
        }
    }

    #[test]
    fn braces_inside_skipped_regions_are_not_placeholders() {
        for sql in [
            "SELECT '{not_a_placeholder}'",
            r#"SELECT 1 AS "weird{name}""#,
            "SELECT 1 -- {not_a_placeholder}\nFROM t",
            "SELECT /* outer /* {nope} */ */ 1 FROM t",
            "SELECT $tag$ {not} $tag$ FROM t",
            "SELECT $$ {not} $$ FROM t",
            "SELECT $1 FROM t", // bare `$1` is not a dollar-quote
        ] {
            assert!(!parse(sql).had_placeholder, "should not parse: {sql}");
        }
    }

    #[test]
    fn errors() {
        for (sql, expect) in [
            ("SELECT {}", "empty placeholder"),
            ("SELECT {foo", "unterminated"),
            ("SELECT } FROM t", "unmatched"),
            ("SELECT {123abc}", "invalid placeholder body"),
            ("SELECT {(", "unterminated"),
            ("SELECT {99999999}", "is not a valid integer"),
        ] {
            let err = parse_query(sql).unwrap_err();
            assert!(err.message.contains(expect), "{sql}: {err}");
        }
    }

    #[test]
    fn paren_depth_finds_matching_close() {
        // The `}` inside `foo(1, 2)` must not terminate the placeholder.
        let p = parse("SELECT {(foo(1, 2).bar)}");
        match &p.fragments[1] {
            Fragment::Placeholder(p) => match &p.ident {
                Ident::Expr(e) => assert_eq!(&**e, "foo(1, 2).bar"),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn token_byte_range() {
        let p = parse("SELECT {id}");
        match &p.fragments[1] {
            Fragment::Placeholder(p) => assert_eq!(p.token, 7..11),
            _ => panic!(),
        }
    }

    #[test]
    fn utf8_preserved_in_literals() {
        let p = parse("SELECT 'héllo' AS x, {id}");
        match &p.fragments[0] {
            Fragment::Literal(s) => assert_eq!(s, "SELECT 'héllo' AS x, "),
            _ => panic!(),
        }
    }

    #[test]
    fn error_includes_context_snippet() {
        let err = parse_query("SELECT a, b, } FROM t WHERE x = 1").unwrap_err();
        // The unmatched `}` is at byte 13; we want a few words on either side.
        assert!(err.context.contains('}'), "{}", err.context);
        // Display includes the context.
        let display = err.to_string();
        assert!(display.contains("near `"), "{display}");
    }

    #[test]
    fn error_context_handles_short_input() {
        // Position past end of input falls back to the last available word(s).
        assert_eq!(error_context("SELECT", 100), "SELECT");
        // Empty input yields an empty snippet.
        assert_eq!(error_context("", 0), "");
        // Entirely-whitespace input also yields empty.
        assert_eq!(error_context("   \n  ", 2), "");
    }

    #[test]
    fn error_context_collapses_whitespace() {
        // Multi-space and newline runs collapse to single spaces.
        let snippet = error_context("a   b\n\nc   d   e   f   g", 8);
        // Whitespace between words is exactly one space, regardless of original spacing.
        assert!(!snippet.contains("  "), "{snippet}");
        assert!(snippet.contains(" "));
    }

    #[test]
    fn error_context_centers_on_position() {
        // 7 single-letter words; positioning at word index 3 (`d`) should give
        // 3 words on either side.
        let sql = "a b c d e f g";
        let snippet = error_context(sql, 6); // points at `d`
        assert_eq!(snippet, "a b c d e f g");
        // Positioning earlier should drop trailing words.
        let snippet = error_context(sql, 0); // points at `a`
        assert_eq!(snippet, "a b c d");
    }
}
