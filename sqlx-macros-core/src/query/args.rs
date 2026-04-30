use crate::database::DatabaseExt;
use crate::query::rewrite::{Binding, BindingRef, BindingRefKind, RewriteOutput, RuntimeTemplate};
use crate::query::{QueryMacroInput, Warnings};
use either::Either;
use proc_macro2::{Ident, Literal, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use sqlx_core::config::Config;
use sqlx_core::describe::Describe;
use sqlx_core::placeholders::Kleene;
use sqlx_core::type_checking;
use sqlx_core::type_info::TypeInfo;
use syn::spanned::Spanned;
use syn::{Expr, ExprCast, ExprGroup, Type};

/// Build the bind-setup tokens and the SQL expression for a query.
///
/// Returns `(setup, sql_expr)`, where:
/// - `setup` is a sequence of statements that introduces a local binding
///   `query_args: Result<DB::Arguments, BoxDynError>`.
/// - `sql_expr` evaluates to a value implementing `SqlSafeStr`: a `&'static str`
///   in the common case, or `AssertSqlSafe<String>` when any spread placeholder
///   forces the SQL to be built at runtime.
pub fn quote_args_and_sql<DB: DatabaseExt>(
    input: &QueryMacroInput,
    rewrite_out: &RewriteOutput,
    config: &Config,
    warnings: &mut Warnings,
    info: &Describe<DB>,
) -> crate::Result<(TokenStream, TokenStream)> {
    let db_path = DB::db_path();

    let param_types = describe_param_types::<DB>(input, config, warnings, info)?;

    if rewrite_out.bindings.is_empty() {
        let sql_expr = static_sql_expr(input, rewrite_out);
        let setup = quote! {
            let query_args = ::core::result::Result::<_, ::sqlx::error::BoxDynError>::Ok(
                <#db_path as ::sqlx::database::Database>::Arguments::default(),
            );
        };
        return Ok((setup, sql_expr));
    }

    // Per-binding setup: `let __sqlx_argN = &(expr)` plus an `if false { ... }`
    // type-check block when describe gave us a parameter type. The type-check
    // block makes `cargo check` reject mismatches at compile time without ever
    // running.
    let arg_names: Vec<Ident> = (0..rewrite_out.bindings.len())
        .map(|i| format_ident!("__sqlx_arg{i}"))
        .collect();
    let binding_setup = bind_locals(&rewrite_out.bindings, &arg_names, &param_types);

    match &rewrite_out.runtime_template {
        RuntimeTemplate::Static(sql) => {
            let sql_lit = Literal::string(sql);
            let setup = static_setup(&db_path, &arg_names, binding_setup);
            Ok((setup, quote! { #sql_lit }))
        }
        RuntimeTemplate::Pieces {
            literals,
            bindings: piece_bindings,
        } => {
            let setup = pieces_setup(
                &db_path,
                &arg_names,
                literals,
                piece_bindings,
                binding_setup,
            );
            Ok((setup, quote! { ::sqlx::AssertSqlSafe(__sqlx_sql) }))
        }
    }
}

// Resolve describe-reported parameter types. `None` for MySQL/SQLite (no per-param
// types from describe) and for `*_unchecked!()` macros.
fn describe_param_types<DB: DatabaseExt>(
    input: &QueryMacroInput,
    config: &Config,
    warnings: &mut Warnings,
    info: &Describe<DB>,
) -> crate::Result<Option<Vec<TokenStream>>> {
    match info.parameters() {
        None | Some(Either::Right(_)) => Ok(None),
        Some(Either::Left(_)) if !input.checked => Ok(None),
        Some(Either::Left(params)) => params
            .iter()
            .enumerate()
            .map(|(i, ty)| get_param_type::<DB>(ty, config, warnings, i))
            .collect::<crate::Result<_>>()
            .map(Some),
    }
}

// Per-binding setup: a `let __sqlx_argN = &(expr);` plus the type-check block.
fn bind_locals(
    bindings: &[Binding],
    names: &[Ident],
    param_types: &Option<Vec<TokenStream>>,
) -> TokenStream {
    let mut out = TokenStream::new();
    for (i, (binding, name)) in bindings.iter().zip(names).enumerate() {
        let param_ty = param_types.as_ref().and_then(|v| v.get(i).cloned());
        let (expr, ty_check) = match binding {
            Binding::Single(e) => (e, single_type_check(e, name, param_ty)),
            Binding::Spread(e) => (e, spread_type_check(e, name, param_ty)),
        };
        let stripped = strip_wildcard(expr.clone());
        out.extend(quote! { let #name = &(#stripped); });
        out.extend(ty_check);
    }
    out
}

fn single_type_check(expr: &Expr, name: &Ident, param_ty: Option<TokenStream>) -> TokenStream {
    let Some(param_ty) = param_ty else {
        return TokenStream::new();
    };
    if get_type_override(expr).is_some() {
        // The user wrote `expr as Ty`; let the cast itself enforce the type.
        return TokenStream::new();
    }
    quote_spanned!(expr.span() =>
        #[allow(clippy::missing_panics_doc, clippy::unreachable)]
        if false {
            use ::sqlx::ty_match::{WrapSameExt as _, MatchBorrowExt as _};
            let expr = ::sqlx::ty_match::dupe_value(#name);
            let ty_check = ::sqlx::ty_match::WrapSame::<#param_ty, _>::new(&expr).wrap_same();
            let (mut _ty_check, match_borrow) = ::sqlx::ty_match::MatchBorrow::new(ty_check, &expr);
            _ty_check = match_borrow.match_borrow();
            ::std::unreachable!();
        }
    )
}

fn spread_type_check(expr: &Expr, name: &Ident, elem_ty: Option<TokenStream>) -> TokenStream {
    let Some(elem_ty) = elem_ty else {
        return TokenStream::new();
    };
    quote_spanned!(expr.span() =>
        #[allow(clippy::missing_panics_doc, clippy::unreachable)]
        if false {
            use ::sqlx::ty_match::{WrapSameExt as _, MatchBorrowExt as _};
            for elem in (::sqlx::ty_match::dupe_value(#name)).into_iter() {
                let ty_check = ::sqlx::ty_match::WrapSame::<#elem_ty, _>::new(&elem).wrap_same();
                let (mut _ty_check, match_borrow) = ::sqlx::ty_match::MatchBorrow::new(ty_check, &elem);
                _ty_check = match_borrow.match_borrow();
                ::std::unreachable!();
            }
        }
    )
}

// Setup for the no-spread path: `$N` / `?` are baked into the SQL at macro time,
// so we just `add` each binding in declaration order.
fn static_setup(db_path: &syn::Path, names: &[Ident], binding_setup: TokenStream) -> TokenStream {
    let n = names.len();
    let size_hint = names
        .iter()
        .map(|n| quote! { + ::sqlx::encode::Encode::<#db_path>::size_hint(#n) });
    let bind_calls = names.iter().map(|name| {
        quote! {
            let query_args = query_args.and_then(move |mut query_args| {
                query_args.add(#name).map(move |()| query_args)
            });
        }
    });
    quote! {
        #binding_setup
        let mut query_args = <#db_path as ::sqlx::database::Database>::Arguments::default();
        query_args.reserve(#n, 0 #(#size_hint)*);
        let query_args = ::core::result::Result::<_, ::sqlx::error::BoxDynError>::Ok(query_args);
        #(#bind_calls)*
    }
}

// Setup for the spread path: build SQL and bind in lockstep at runtime so each
// spread element advances `Arguments::buffer.count` (which Postgres'
// `format_placeholder` reads to produce `$N`).
fn pieces_setup(
    db_path: &syn::Path,
    names: &[Ident],
    literals: &[String],
    piece_bindings: &[BindingRef],
    binding_setup: TokenStream,
) -> TokenStream {
    let cap_hint: usize = literals.iter().map(String::len).sum::<usize>() + 16;
    let mut body = TokenStream::new();

    for (idx, lit) in literals.iter().enumerate() {
        let lit_tok = Literal::string(lit);
        body.extend(quote! { __sqlx_sql.push_str(#lit_tok); });
        if let Some(bref) = piece_bindings.get(idx) {
            let name = &names[bref.binding_index];
            body.extend(match &bref.kind {
                BindingRefKind::Single => quote! {
                    __sqlx_args.add(#name)?;
                    __sqlx_args
                        .format_placeholder(&mut __sqlx_sql)
                        .expect("writing to String is infallible");
                },
                BindingRefKind::Spread(kleene) => {
                    let empty_filler = match kleene {
                        Kleene::Plus => quote! { __sqlx_sql.push_str("NULL"); },
                        Kleene::Star => quote! {},
                        _ => quote! {
                            compile_error!("unsupported Kleene variant");
                        },
                    };
                    quote! {
                        let mut __sqlx_count = 0usize;
                        for elem in (#name).into_iter() {
                            if __sqlx_count > 0 { __sqlx_sql.push_str(", "); }
                            __sqlx_args.add(elem)?;
                            __sqlx_args
                                .format_placeholder(&mut __sqlx_sql)
                                .expect("writing to String is infallible");
                            __sqlx_count += 1;
                        }
                        if __sqlx_count == 0 { #empty_filler }
                    }
                }
            });
        }
    }

    quote! {
        #binding_setup
        let mut __sqlx_sql = String::with_capacity(#cap_hint);
        let mut __sqlx_args = <#db_path as ::sqlx::database::Database>::Arguments::default();
        // IIFE so `?` from `Arguments::add` early-returns from this block,
        // not the surrounding macro expansion.
        let __sqlx_result: ::core::result::Result<(), ::sqlx::error::BoxDynError> = (|| {
            #body
            ::core::result::Result::Ok(())
        })();
        let query_args = __sqlx_result.map(|()| __sqlx_args);
    }
}

// SQL expression for the no-binding fast path. For `query_file!` we prefer
// `include_str!` so the file participates in the compiler's source-tracking;
// that's only possible when the rewriter's output is byte-identical to the
// file content. `{{` / `}}` collapse outside skip regions modifies the text,
// so for those queries we fall back to a string literal of the rewritten SQL.
fn static_sql_expr(input: &QueryMacroInput, rewrite_out: &RewriteOutput) -> TokenStream {
    let RuntimeTemplate::Static(sql) = &rewrite_out.runtime_template else {
        unreachable!("static_sql_expr called on a non-static template")
    };
    if let Some(path) = &input.file_path {
        if sql == &input.sql {
            return quote_spanned! { input.src_span => include_str!(#path) };
        }
    }
    let lit = Literal::string(sql);
    quote! { #lit }
}

fn get_param_type<DB: DatabaseExt>(
    param_ty: &DB::TypeInfo,
    config: &Config,
    warnings: &mut Warnings,
    i: usize,
) -> crate::Result<TokenStream> {
    if let Some(type_override) = config.macros.type_override(param_ty.name()) {
        return Ok(type_override.parse()?);
    }

    let err = match DB::param_type_for_id(param_ty, &config.macros.preferred_crates) {
        Ok(t) => return Ok(t.parse()?),
        Err(e) => e,
    };

    let param_num = i + 1;

    let message = match err {
        type_checking::Error::NoMappingFound => {
            if let Some(feature_gate) = DB::get_feature_gate(param_ty) {
                format!(
                    "optional sqlx feature `{feature_gate}` required for type {param_ty} of param #{param_num}",
                )
            } else {
                format!(
                    "no built-in mapping for type {param_ty} of param #{param_num}; \
                         a type override may be required, see documentation for details"
                )
            }
        }
        type_checking::Error::DateTimeCrateFeatureNotEnabled => {
            let feature_gate = config
                .macros
                .preferred_crates
                .date_time
                .crate_name()
                .expect("BUG: got feature-not-enabled error for DateTimeCrate::Inferred");

            format!(
                "SQLx feature `{feature_gate}` required for type {param_ty} of param #{param_num} \
                 (configured by `macros.preferred-crates.date-time` in sqlx.toml)",
            )
        }
        type_checking::Error::NumericCrateFeatureNotEnabled => {
            let feature_gate = config
                .macros
                .preferred_crates
                .numeric
                .crate_name()
                .expect("BUG: got feature-not-enabled error for NumericCrate::Inferred");

            format!(
                "SQLx feature `{feature_gate}` required for type {param_ty} of param #{param_num} \
                 (configured by `macros.preferred-crates.numeric` in sqlx.toml)",
            )
        }

        type_checking::Error::AmbiguousDateTimeType { fallback } => {
            warnings.ambiguous_datetime = true;
            return Ok(fallback.parse()?);
        }

        type_checking::Error::AmbiguousNumericType { fallback } => {
            warnings.ambiguous_numeric = true;
            return Ok(fallback.parse()?);
        }
    };

    Err(message.into())
}

fn get_type_override(expr: &Expr) -> Option<&Type> {
    match expr {
        Expr::Group(group) => get_type_override(&group.expr),
        Expr::Cast(cast) => Some(&cast.ty),
        _ => None,
    }
}

fn strip_wildcard(expr: Expr) -> Expr {
    match expr {
        Expr::Group(ExprGroup {
            attrs,
            group_token,
            expr,
        }) => Expr::Group(ExprGroup {
            attrs,
            group_token,
            expr: Box::new(strip_wildcard(*expr)),
        }),
        // we want to retain casts if they semantically matter
        Expr::Cast(ExprCast {
            attrs,
            expr,
            as_token,
            ty,
        }) => match *ty {
            // cast to wildcard `_` will produce weird errors; we interpret it as taking the value as-is
            Type::Infer(_) => *expr,
            _ => Expr::Cast(ExprCast {
                attrs,
                expr,
                as_token,
                ty,
            }),
        },
        _ => expr,
    }
}
