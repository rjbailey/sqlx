use std::fs;
use std::ops::Range;

use proc_macro2::{Ident, Literal, Span};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::Type;
use syn::{Expr, LitBool, LitStr, Token};

/// Macro input shared by `query!()` and `query_file!()`
pub struct QueryMacroInput {
    pub(super) sql: String,

    pub(super) src_span: Span,

    /// Source token for the SQL string literal, when the source is a single
    /// `source = "..."` (not a concatenation `"a" + "b"` and not `source_file
    /// = ...`). Used to compute byte-range subspans pointing at individual
    /// `{...}` placeholders for diagnostics and IDE jump-to-definition.
    #[allow(dead_code)]
    pub(super) sql_token: Option<Literal>,

    pub(super) record_type: RecordType,

    /// Positional bind expressions, in order. Empty if all args were named.
    pub(super) arg_exprs: Vec<Expr>,

    /// Named bind expressions: `name = expr`. Empty if all args were positional.
    pub(super) named_args: Vec<(Ident, Expr)>,

    /// `..rest` argument: when present, every unmatched `{name}` placeholder is
    /// looked up as `rest.name`. To inline-capture a scope-local `name` alongside
    /// `..rest`, name it explicitly: `query!("...", ..rest, name = name)`. Only
    /// one `..rest` per call.
    pub(super) rest_arg: Option<Expr>,

    pub(super) checked: bool,

    pub(super) file_path: Option<String>,
}

impl QueryMacroInput {
    /// Translate a byte range within `self.sql` into a `Span` on the source
    /// SQL literal. Falls back to `self.src_span` when the source is a
    /// concatenated literal or a file, or when the underlying proc-macro
    /// implementation doesn't support sub-spanning. `Literal::subspan` is
    /// gated behind the unstable `proc_macro_span` feature, so on stable
    /// Rust this always falls back to `src_span` regardless of `sql_token`.
    pub(super) fn subspan(&self, range: Range<usize>) -> Span {
        #[cfg(any(sqlx_macros_unstable, procmacro2_semver_exempt))]
        {
            if let Some(token) = &self.sql_token {
                // The source representation is `"..."` (with the opening quote
                // at byte 0 of the token text), so value-byte offsets map to
                // source offsets shifted by 1. This is approximate — string
                // literals containing escape sequences (e.g. `\n`, `\u{...}`)
                // shift the mapping per escape — but `Literal::subspan` returns
                // `None` when the range doesn't align with token boundaries,
                // and we fall back to the overall span in that case.
                if let Some(span) = token.subspan((range.start + 1)..(range.end + 1)) {
                    return span;
                }
            }
        }
        let _ = range;
        self.src_span
    }
}

enum QuerySrc {
    String(String),
    File(String),
}

pub enum RecordType {
    Given(Type),
    Scalar,
    Generated,
}

impl Parse for QueryMacroInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut query_src: Option<(QuerySrc, Span, Option<Literal>)> = None;
        let mut positional: Vec<Expr> = Vec::new();
        let mut named: Vec<(Ident, Expr)> = Vec::new();
        let mut rest_arg: Option<Expr> = None;
        let mut record_type = RecordType::Generated;
        let mut checked = true;

        let mut expect_comma = false;

        while !input.is_empty() {
            if expect_comma {
                let _ = input.parse::<syn::token::Comma>()?;
            }

            let key: Ident = input.parse()?;

            let _ = input.parse::<syn::token::Eq>()?;

            if key == "source" {
                let lits = Punctuated::<LitStr, Token![+]>::parse_separated_nonempty(input)?;
                let first = lits.first().expect("non-empty");
                // Use the first literal's span — this is a user-typed token, so its
                // hygiene context is the caller's, which is what we need for
                // synthesizing inline-capture idents that resolve to user locals.
                let span = first.span();
                // Capture the underlying token only when there's a single literal,
                // so `subspan` can map placeholder byte ranges to precise source
                // locations. Concatenated literals (`"a" + "b"`) fall back to the
                // overall span.
                let sql_token = (lits.len() == 1).then(|| first.token());
                let query_str = lits.iter().map(LitStr::value).collect();
                query_src = Some((QuerySrc::String(query_str), span, sql_token));
            } else if key == "source_file" {
                let lit_str = input.parse::<LitStr>()?;
                query_src = Some((QuerySrc::File(lit_str.value()), lit_str.span(), None));
            } else if key == "args" {
                parse_args_list(input, &mut positional, &mut named, &mut rest_arg)?;
            } else if key == "record" {
                if !matches!(record_type, RecordType::Generated) {
                    return Err(input.error("colliding `scalar` or `record` key"));
                }

                record_type = RecordType::Given(input.parse()?);
            } else if key == "scalar" {
                if !matches!(record_type, RecordType::Generated) {
                    return Err(input.error("colliding `scalar` or `record` key"));
                }

                // we currently expect only `scalar = _`
                // a `query_as_scalar!()` variant seems less useful than just overriding the type
                // of the column in SQL
                input.parse::<syn::Token![_]>()?;
                record_type = RecordType::Scalar;
            } else if key == "checked" {
                let lit_bool = input.parse::<LitBool>()?;
                checked = lit_bool.value;
            } else {
                let message = format!("unexpected input key: {key}");
                return Err(syn::Error::new_spanned(key, message));
            }

            expect_comma = true;
        }

        let (src, src_span, sql_token) =
            query_src.ok_or_else(|| input.error("expected `source` or `source_file` key"))?;

        let file_path = src.file_path(src_span)?;

        Ok(QueryMacroInput {
            sql: src.resolve(src_span)?,
            src_span,
            sql_token,
            record_type,
            arg_exprs: positional,
            named_args: named,
            rest_arg,
            checked,
            file_path,
        })
    }
}

/// Parse the contents of `args = [ ... ]`.
///
/// Each element is one of:
/// - `expr` — a positional argument; must appear before any named or `..rest` form.
/// - `name = expr` — a named argument.
/// - `..rest` — struct-spread (#591): any `{name}` placeholder not matched by an
///   explicit named arg is looked up as `rest.name`. Only one `..rest` per call.
fn parse_args_list(
    input: ParseStream,
    positional: &mut Vec<Expr>,
    named: &mut Vec<(Ident, Expr)>,
    rest: &mut Option<Expr>,
) -> syn::Result<()> {
    let content;
    syn::bracketed!(content in input);

    while !content.is_empty() {
        if content.peek(Token![..]) {
            if rest.is_some() {
                return Err(content.error("only one `..rest` argument is allowed"));
            }
            content.parse::<Token![..]>()?;
            *rest = Some(content.parse()?);
        } else {
            // Lookahead: a named arg is `Ident = expr` (but not `Ident == expr`).
            let is_named =
                content.peek(syn::Ident) && content.peek2(Token![=]) && !content.peek2(Token![==]);

            if is_named {
                let name: Ident = content.parse()?;
                content.parse::<Token![=]>()?;
                let expr: Expr = content.parse()?;
                named.push((name, expr));
            } else {
                if !named.is_empty() || rest.is_some() {
                    let expr_for_span: Expr = content.parse()?;
                    return Err(syn::Error::new_spanned(
                        expr_for_span,
                        "positional arguments must come before named arguments and `..rest`",
                    ));
                }
                let expr: Expr = content.parse()?;
                positional.push(expr);
            }
        }

        if content.is_empty() {
            break;
        }
        content.parse::<Token![,]>()?;
    }

    Ok(())
}

impl QuerySrc {
    /// If the query source is a file, read it to a string. Otherwise return the query string.
    fn resolve(self, source_span: Span) -> syn::Result<String> {
        match self {
            QuerySrc::String(string) => Ok(string),
            QuerySrc::File(file) => read_file_src(&file, source_span),
        }
    }

    fn file_path(&self, source_span: Span) -> syn::Result<Option<String>> {
        if let QuerySrc::File(ref file) = *self {
            let path = crate::common::resolve_path(file, source_span)?
                .canonicalize()
                .map_err(|e| syn::Error::new(source_span, e))?;

            Ok(Some(
                path.to_str()
                    .ok_or_else(|| {
                        syn::Error::new(
                            source_span,
                            "query file path cannot be represented as a string",
                        )
                    })?
                    .to_string(),
            ))
        } else {
            Ok(None)
        }
    }
}

fn read_file_src(source: &str, source_span: Span) -> syn::Result<String> {
    let file_path = crate::common::resolve_path(source, source_span)?;

    fs::read_to_string(&file_path).map_err(|e| {
        syn::Error::new(
            source_span,
            format!(
                "failed to read query file at {}: {}",
                file_path.display(),
                e
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_str;

    /// A `Parse` wrapper that delegates to `parse_args_list`, so we can test
    /// the args-list grammar in isolation via `syn::parse_str`.
    struct ArgsList {
        positional: Vec<Expr>,
        named: Vec<(Ident, Expr)>,
        rest: Option<Expr>,
    }

    impl Parse for ArgsList {
        fn parse(input: ParseStream) -> syn::Result<Self> {
            let mut out = ArgsList {
                positional: Vec::new(),
                named: Vec::new(),
                rest: None,
            };
            parse_args_list(input, &mut out.positional, &mut out.named, &mut out.rest)?;
            Ok(out)
        }
    }

    fn parse(input: &str) -> syn::Result<ArgsList> {
        parse_str::<ArgsList>(input)
    }

    #[test]
    fn empty_args_list() {
        let out = parse("[]").unwrap();
        assert!(out.positional.is_empty());
        assert!(out.named.is_empty());
        assert!(out.rest.is_none());
    }

    #[test]
    fn positional_only() {
        let out = parse("[1, 2, 3]").unwrap();
        assert_eq!(out.positional.len(), 3);
        assert!(out.named.is_empty());
    }

    #[test]
    fn named_only() {
        let out = parse("[a = 1, b = 2]").unwrap();
        assert!(out.positional.is_empty());
        assert_eq!(out.named.len(), 2);
        assert_eq!(out.named[0].0.to_string(), "a");
        assert_eq!(out.named[1].0.to_string(), "b");
    }

    #[test]
    fn rest_only() {
        let out = parse("[..my_struct]").unwrap();
        assert!(out.rest.is_some());
    }

    #[test]
    fn positional_then_named_then_rest() {
        let out = parse("[1, 2, a = 3, ..rest]").unwrap();
        assert_eq!(out.positional.len(), 2);
        assert_eq!(out.named.len(), 1);
        assert!(out.rest.is_some());
    }

    #[test]
    fn equality_in_positional_is_not_named() {
        // `a == b` is one positional expression, not `a = (=b)`.
        let out = parse("[a == b]").unwrap();
        assert_eq!(out.positional.len(), 1);
        assert!(out.named.is_empty());
    }

    #[test]
    fn trailing_comma_accepted() {
        let out = parse("[1, 2,]").unwrap();
        assert_eq!(out.positional.len(), 2);
    }

    fn err_msg(input: &str) -> String {
        match parse(input) {
            Ok(_) => panic!("expected error parsing `{input}`"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn rejects_positional_after_named() {
        let msg = err_msg("[a = 1, 2]");
        assert!(msg.contains("must come before"), "{msg}");
    }

    #[test]
    fn rejects_positional_after_rest() {
        let msg = err_msg("[..rest, 1]");
        assert!(msg.contains("must come before"), "{msg}");
    }

    #[test]
    fn rejects_double_rest() {
        let msg = err_msg("[..a, ..b]");
        assert!(msg.contains("only one"), "{msg}");
    }
}
