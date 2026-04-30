//! Resolves `{...}` placeholders to bind expressions and emits native-placeholder SQL.
//!
//! Runs after [`sqlx_core::placeholders::parse_query`]. Produces:
//! - `describe_sql` for the compile-time database describe (one native placeholder
//!   per binding, with each spread collapsed to one).
//! - `runtime_template` for the SQL the macro should emit at runtime — either a
//!   single static string (no spreads) or alternating literal/binding pieces.
//! - The bindings, in describe order.

use std::ops::Range;

use proc_macro2::Span;
use quote::ToTokens;
use syn::Expr;

use crate::database::ParamIndexing;
use sqlx_core::placeholders::{Fragment, Ident, Kleene, ParsedQuery, Placeholder};
use sqlx_core::HashMap;

#[derive(Clone)]
pub(crate) enum Binding {
    Single(Expr),
    Spread(Expr),
}

pub(crate) struct RewriteOutput {
    pub describe_sql: String,
    pub runtime_template: RuntimeTemplate,
    pub bindings: Vec<Binding>,
}

impl RewriteOutput {
    /// Whether any binding is a spread. When true, the runtime SQL is built at
    /// call time (placeholder counters can't be baked in at macro expansion).
    pub fn has_spread(&self) -> bool {
        self.bindings
            .iter()
            .any(|b| matches!(b, Binding::Spread(_)))
    }
}

/// SQL emission strategy at runtime.
///
/// `Static` emits a `&'static str` (zero overhead). `Pieces` emits a `String`
/// built at runtime, with `literals` and `bindings` alternating: `literals[0]`,
/// then `bindings[0]` expansion, then `literals[1]`, etc. (so `literals.len()
/// == bindings.len() + 1`). Literals contain pure SQL with no native
/// placeholders; placeholders are emitted at runtime in lockstep with
/// `Arguments::add` so a spread of length K advances the placeholder counter
/// by K and the next single picks up the correct number.
pub(crate) enum RuntimeTemplate {
    Static(String),
    Pieces {
        literals: Vec<String>,
        bindings: Vec<BindingRef>,
    },
}

#[derive(Clone)]
pub(crate) struct BindingRef {
    pub binding_index: usize,
    pub kind: BindingRefKind,
}

#[derive(Clone)]
pub(crate) enum BindingRefKind {
    Single,
    Spread(Kleene),
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct RewriteError {
    pub byte_range: Range<usize>,
    pub message: String,
}

pub(crate) fn rewrite(
    parsed: &ParsedQuery<'_>,
    arg_exprs: &[Expr],
    named_args: &[(syn::Ident, Expr)],
    rest_arg: Option<&Expr>,
    placeholder_char: char,
    indexing: ParamIndexing,
    subspan: impl Fn(Range<usize>) -> Span,
) -> Result<RewriteOutput, RewriteError> {
    if !parsed.had_placeholder {
        if !named_args.is_empty() || rest_arg.is_some() {
            return Err(err(
                0..0,
                "named or `..rest` arguments were provided but the SQL contains no `{...}` placeholders",
            ));
        }
        let sql: String = parsed
            .fragments
            .iter()
            .map(|f| match f {
                Fragment::Literal(s) => s.as_str(),
                _ => unreachable!("had_placeholder is false, so all fragments must be literals"),
            })
            .collect();
        return Ok(RewriteOutput {
            describe_sql: sql.clone(),
            runtime_template: RuntimeTemplate::Static(sql),
            bindings: arg_exprs.iter().cloned().map(Binding::Single).collect(),
        });
    }

    let named: HashMap<String, &Expr> =
        named_args.iter().map(|(n, e)| (n.to_string(), e)).collect();

    let has_spread = parsed
        .fragments
        .iter()
        .any(|f| matches!(f, Fragment::Placeholder(p) if p.kleene.is_some()));

    let mut bindings = Vec::new();
    let mut describe_sql = String::new();
    let mut next_param = 1usize;
    let mut runtime_literals: Vec<String> = vec![String::new()];
    let mut runtime_bindings: Vec<BindingRef> = Vec::new();

    for frag in &parsed.fragments {
        match frag {
            Fragment::Literal(s) => {
                describe_sql.push_str(s);
                runtime_literals.last_mut().unwrap().push_str(s);
            }
            Fragment::Placeholder(p) => {
                let resolved = resolve(p, arg_exprs, &named, rest_arg, &subspan)?;
                write_placeholder(&mut describe_sql, placeholder_char, indexing, next_param);
                next_param += 1;

                let (binding, kind) = match p.kleene {
                    None => (Binding::Single(resolved), BindingRefKind::Single),
                    Some(kleene) => (Binding::Spread(resolved), BindingRefKind::Spread(kleene)),
                };
                runtime_bindings.push(BindingRef {
                    binding_index: bindings.len(),
                    kind,
                });
                runtime_literals.push(String::new());
                bindings.push(binding);
            }
            _ => unreachable!("rewriter must handle every Fragment variant"),
        }
    }

    let runtime_template = if has_spread {
        RuntimeTemplate::Pieces {
            literals: runtime_literals,
            bindings: runtime_bindings,
        }
    } else {
        // No spread: describe_sql is exactly the runtime SQL — emit as `&'static str`.
        RuntimeTemplate::Static(describe_sql.clone())
    };

    Ok(RewriteOutput {
        describe_sql,
        runtime_template,
        bindings,
    })
}

fn resolve(
    p: &Placeholder<'_>,
    arg_exprs: &[Expr],
    named: &HashMap<String, &Expr>,
    rest: Option<&Expr>,
    subspan: &impl Fn(Range<usize>) -> Span,
) -> Result<Expr, RewriteError> {
    // The synthesized tokens get the byte-range span of this placeholder, so
    // jump-to-definition and "find references" point at `{...}` rather than the
    // entire SQL literal.
    let span = subspan(p.token.clone());
    match &p.ident {
        Ident::Positional(i) => arg_exprs.get(usize::from(*i)).cloned().ok_or_else(|| {
            err(
                p.token.clone(),
                format!(
                    "positional placeholder `{{{i}}}` is out of range; only {} positional \
                     argument(s) provided",
                    arg_exprs.len()
                ),
            )
        }),
        Ident::Named(name) => {
            if let Some(expr) = named.get(&**name) {
                return Ok((*expr).clone());
            }
            // `..rest` resolution takes precedence over inline capture (#591) so we don't
            // have to disambiguate `rest.name` vs a scope-local `name`.
            if let Some(rest_expr) = rest {
                return Ok(syn::Expr::Field(syn::ExprField {
                    attrs: Vec::new(),
                    base: Box::new(rest_expr.clone()),
                    dot_token: syn::Token![.](span),
                    member: syn::Member::Named(syn::Ident::new_raw(name, span)),
                }));
            }
            // Inline capture: synthesize an `Expr::Path(name)` at the placeholder's
            // span. The placeholder lives within a user-typed token, so its hygiene
            // context is the caller's scope and the synthesized ident resolves to
            // the user's local of that name. `new_raw` so reserved words (`type`,
            // `match`, etc.) don't panic — the user would write `let r#type = ...`
            // to bind one.
            Ok(syn::Expr::Path(syn::ExprPath {
                attrs: Vec::new(),
                qself: None,
                path: syn::Ident::new_raw(name, span).into(),
            }))
        }
        Ident::Expr(text) => {
            let parsed: Expr = syn::parse_str(text).map_err(|e| {
                err(
                    p.token.clone(),
                    format!("failed to parse expression `{text}`: {e}"),
                )
            })?;
            Ok(respan(parsed, span))
        }
        _ => unreachable!("rewriter must handle every Ident variant"),
    }
}

/// Set every token's span in `expr` to `span`. Required for `{(expr)}` because
/// `syn::parse_str` emits tokens at `Span::call_site()`, which has macro_rules
/// wrapper hygiene and won't resolve user-scope locals.
fn respan(expr: Expr, span: Span) -> Expr {
    use proc_macro2::{Group, TokenStream, TokenTree};
    fn walk(ts: TokenStream, span: Span) -> TokenStream {
        ts.into_iter()
            .map(|tt| match tt {
                TokenTree::Group(g) => {
                    let mut new = Group::new(g.delimiter(), walk(g.stream(), span));
                    new.set_span(span);
                    TokenTree::Group(new)
                }
                TokenTree::Ident(mut i) => {
                    i.set_span(span);
                    TokenTree::Ident(i)
                }
                TokenTree::Punct(mut p) => {
                    p.set_span(span);
                    TokenTree::Punct(p)
                }
                TokenTree::Literal(mut l) => {
                    l.set_span(span);
                    TokenTree::Literal(l)
                }
            })
            .collect()
    }
    syn::parse2(walk(expr.to_token_stream(), span)).expect("respan should round-trip")
}

fn write_placeholder(out: &mut String, ch: char, indexing: ParamIndexing, n: usize) {
    out.push(ch);
    if matches!(indexing, ParamIndexing::OneIndexed) {
        use std::fmt::Write as _;
        write!(out, "{n}").expect("writing to String is infallible");
    }
}

fn err(byte_range: Range<usize>, message: impl Into<String>) -> RewriteError {
    RewriteError {
        byte_range,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proc_macro2::Span;
    use sqlx_core::placeholders::parse_query;
    use syn::parse_quote;

    fn ident(name: &str) -> syn::Ident {
        syn::Ident::new(name, Span::call_site())
    }

    fn run(
        sql: &str,
        positional: Vec<Expr>,
        named: Vec<(syn::Ident, Expr)>,
        rest: Option<&Expr>,
        placeholder_char: char,
        indexing: ParamIndexing,
    ) -> Result<RewriteOutput, RewriteError> {
        rewrite(
            &parse_query(sql).unwrap(),
            &positional,
            &named,
            rest,
            placeholder_char,
            indexing,
            |_| Span::call_site(),
        )
    }

    fn run_pg(
        sql: &str,
        positional: Vec<Expr>,
        named: Vec<(syn::Ident, Expr)>,
    ) -> Result<RewriteOutput, RewriteError> {
        run(sql, positional, named, None, '$', ParamIndexing::OneIndexed)
    }

    fn run_anon(
        sql: &str,
        positional: Vec<Expr>,
        named: Vec<(syn::Ident, Expr)>,
    ) -> Result<RewriteOutput, RewriteError> {
        run(sql, positional, named, None, '?', ParamIndexing::Implicit)
    }

    fn ok(r: Result<RewriteOutput, RewriteError>) -> RewriteOutput {
        r.unwrap_or_else(|e| panic!("rewrite failed: {e}"))
    }
    fn err_msg(r: Result<RewriteOutput, RewriteError>) -> String {
        match r {
            Ok(_) => panic!("expected error"),
            Err(e) => e.message,
        }
    }

    #[test]
    fn legacy_passthrough_no_placeholders() {
        let out = ok(run_pg("SELECT $1, $2", vec![], vec![]));
        assert_eq!(out.describe_sql, "SELECT $1, $2");
        assert_eq!(out.bindings.len(), 0);
    }

    #[test]
    fn positional_pg() {
        let out = ok(run_pg(
            "SELECT {0}, {1}",
            vec![parse_quote!(10), parse_quote!("hi")],
            vec![],
        ));
        assert_eq!(out.describe_sql, "SELECT $1, $2");
        assert!(matches!(
            out.runtime_template,
            RuntimeTemplate::Static(ref s) if s == "SELECT $1, $2"
        ));
    }

    #[test]
    fn positional_anon_format() {
        let out = ok(run_anon(
            "SELECT {0}, {1}",
            vec![parse_quote!(10), parse_quote!("hi")],
            vec![],
        ));
        assert_eq!(out.describe_sql, "SELECT ?, ?");
    }

    #[test]
    fn named_pg() {
        let out = ok(run_pg(
            "SELECT {id}",
            vec![],
            vec![(ident("id"), parse_quote!(10))],
        ));
        assert_eq!(out.describe_sql, "SELECT $1");
        assert_eq!(out.bindings.len(), 1);
    }

    #[test]
    fn inline_capture_synthesizes_path_expr() {
        let out = ok(run_pg("SELECT {foo}", vec![], vec![]));
        match &out.bindings[0] {
            Binding::Single(Expr::Path(p)) => {
                // `Ident::new_raw` always prefixes with `r#` in the textual form;
                // semantically `r#foo` and `foo` resolve to the same name.
                let ident = p.path.get_ident().unwrap().to_string();
                assert_eq!(ident.trim_start_matches("r#"), "foo");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn inline_capture_handles_reserved_keyword() {
        // Reserved Rust keywords (`type`, `match`, etc.) used as placeholder
        // names produce raw idents that resolve to a user's `let r#type = ...`.
        let out = ok(run_pg("SELECT {type}", vec![], vec![]));
        assert_eq!(out.bindings.len(), 1);
    }

    #[test]
    fn rest_handles_reserved_keyword_field() {
        // `..rest` field-access path also goes through `Ident::new_raw`.
        let rest: Expr = parse_quote!(my_struct);
        let out = ok(run(
            "SELECT {match}",
            vec![],
            vec![],
            Some(&rest),
            '$',
            ParamIndexing::OneIndexed,
        ));
        assert!(matches!(out.bindings[0], Binding::Single(Expr::Field(_))));
    }

    #[test]
    fn spread_collapses_to_one_describe_placeholder() {
        let out = ok(run_pg(
            "SELECT * FROM t WHERE id IN ({ids*})",
            vec![],
            vec![(ident("ids"), parse_quote!(vec![1, 2, 3]))],
        ));
        assert_eq!(out.describe_sql, "SELECT * FROM t WHERE id IN ($1)");
        match &out.runtime_template {
            RuntimeTemplate::Pieces { literals, bindings } => {
                assert_eq!(literals, &["SELECT * FROM t WHERE id IN (", ")"]);
                assert!(matches!(
                    bindings[0].kind,
                    BindingRefKind::Spread(Kleene::Star)
                ));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn spread_with_singles_keeps_literals_pure_sql() {
        // With any spread present, runtime literals must NOT contain $N — placeholders
        // are emitted at runtime in lockstep with `add` so spread expansion shifts
        // subsequent placeholder numbers correctly.
        let out = ok(run_pg(
            "SELECT {foo}, * FROM t WHERE id IN ({ids*}) AND y = {bar}",
            vec![],
            vec![
                (ident("foo"), parse_quote!(7)),
                (ident("ids"), parse_quote!(vec![1, 2])),
                (ident("bar"), parse_quote!(8)),
            ],
        ));
        assert_eq!(
            out.describe_sql,
            "SELECT $1, * FROM t WHERE id IN ($2) AND y = $3"
        );
        match &out.runtime_template {
            RuntimeTemplate::Pieces { literals, bindings } => {
                assert_eq!(
                    literals,
                    &["SELECT ", ", * FROM t WHERE id IN (", ") AND y = ", ""]
                );
                assert_eq!(bindings.len(), 3);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn question_mark_in_string_is_not_misread_as_native() {
        // The lexer doesn't interpret `?` inside a string literal as a placeholder,
        // and the rewriter doesn't reject the query for "mixing" styles.
        let out = ok(run_anon(
            "SELECT 'a?b' WHERE id = {x}",
            vec![],
            vec![(ident("x"), parse_quote!(1))],
        ));
        assert_eq!(out.describe_sql, "SELECT 'a?b' WHERE id = ?");
    }

    #[test]
    fn dollar_n_in_string_is_not_misread_as_native() {
        let out = ok(run_pg(
            "SELECT '$1 cost' WHERE id = {x}",
            vec![],
            vec![(ident("x"), parse_quote!(1))],
        ));
        assert_eq!(out.describe_sql, "SELECT '$1 cost' WHERE id = $1");
    }

    #[test]
    fn out_of_range_positional() {
        let msg = err_msg(run_pg("SELECT {3}", vec![], vec![]));
        assert!(msg.contains("out of range"), "{msg}");
    }

    #[test]
    fn paren_expr_field_and_method() {
        let out = ok(run_pg("SELECT {(foo.bar)}", vec![], vec![]));
        assert!(matches!(out.bindings[0], Binding::Single(Expr::Field(_))));
        let out = ok(run_pg("SELECT {(foo.bar())}", vec![], vec![]));
        assert!(matches!(
            out.bindings[0],
            Binding::Single(Expr::MethodCall(_))
        ));
    }

    #[test]
    fn paren_expr_invalid_syntax() {
        let msg = err_msg(run_pg("SELECT {(foo +)}", vec![], vec![]));
        assert!(msg.contains("failed to parse expression"), "{msg}");
    }

    #[test]
    fn rest_resolves_unmatched_named_to_field_access() {
        let rest: Expr = parse_quote!(my_struct);
        let out = ok(run(
            "INSERT INTO t (a, b) VALUES ({a}, {b})",
            vec![],
            vec![],
            Some(&rest),
            '$',
            ParamIndexing::OneIndexed,
        ));
        for b in &out.bindings {
            assert!(matches!(b, Binding::Single(Expr::Field(_))));
        }
    }

    #[test]
    fn rest_does_not_override_explicit_named() {
        let rest: Expr = parse_quote!(my_struct);
        let out = ok(run(
            "INSERT INTO t (id, b) VALUES ({id}, {b})",
            vec![],
            vec![(ident("id"), parse_quote!(99i32))],
            Some(&rest),
            '$',
            ParamIndexing::OneIndexed,
        ));
        // `{id}` → 99i32 (explicit), `{b}` → my_struct.b (rest).
        assert!(matches!(out.bindings[0], Binding::Single(Expr::Lit(_))));
        assert!(matches!(out.bindings[1], Binding::Single(Expr::Field(_))));
    }

    #[test]
    fn mixing_native_and_braces_produces_clashing_describe_sql() {
        // The rewriter no longer rejects queries that contain both native
        // placeholders and `{...}` interpolation — the database surfaces the
        // conflict at describe time. This test pins that the rewriter's
        // describe_sql contains both: the user's literal `$1` and the
        // rewriter's `$1` for `{foo}`. Postgres reports "there is no
        // parameter $2" against the resulting two-arg prepared statement.
        let out = ok(run_pg(
            "SELECT $1, {foo}",
            vec![],
            vec![(ident("foo"), parse_quote!(7))],
        ));
        assert_eq!(out.describe_sql, "SELECT $1, $1");
    }

    #[test]
    fn repeated_placeholder_clones_binding() {
        // `{x}` referenced twice: each reference clones the bound expression.
        // Like `format!`, repeated references re-evaluate at runtime if the
        // expression has side effects.
        let out = ok(run_pg(
            "SELECT {x} + {x}",
            vec![],
            vec![(ident("x"), parse_quote!(5i32))],
        ));
        assert_eq!(out.bindings.len(), 2);
        assert!(matches!(out.bindings[0], Binding::Single(Expr::Lit(_))));
        assert!(matches!(out.bindings[1], Binding::Single(Expr::Lit(_))));
    }
}
