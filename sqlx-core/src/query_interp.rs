//! Runtime SQL builder with `{...}` placeholder syntax.
//!
//! [`query_interp`] is the runtime counterpart of `sqlx::query!` for
//! dynamically-built SQL. It accepts the same `{...}` placeholder syntax
//! (positional `{0}`, named `{name}`, spread `{ids*}` / `{ids+}`); inline
//! capture, `..rest`, and `{(expr)}` are macro-only because the runtime
//! cannot evaluate Rust expressions or capture scope-locals.
//!
//! Compared to the macro: there is no compile-time describe step (type errors
//! surface at execution), and each placeholder may be referenced at most
//! once (bindings are consumed on first reference).

use crate::arguments::{Arguments, ImmutableArguments};
use crate::database::Database;
use crate::encode::Encode;
use crate::error::{BoxDynError, Error};
use crate::from_row::FromRow;
use crate::placeholders::{parse_query, Fragment, Ident, Kleene};
use crate::query::{query_with_result, Query};
use crate::query_as::{query_as_with, QueryAs};
use crate::query_scalar::{query_scalar_with, QueryScalar};
use crate::sql_str::AssertSqlSafe;
use crate::types::Type;
use crate::HashMap;

/// Begin building a runtime-interpolated query.
///
/// Bound values may be borrowed (`&str`, `&[u8]`, etc.), as with
/// [`query()`][crate::query::query]; the returned [`InterpQuery<'q, DB>`] is
/// parameterized by `'q`, the longest borrow.
///
/// ```rust,no_run
/// # use sqlx::{Pool, Sqlite};
/// # async fn example(pool: Pool<Sqlite>) -> sqlx::Result<()> {
/// sqlx::query_interp::<Sqlite>("SELECT id FROM users WHERE name = {name}")
///     .bind_named("name", "alice")
///     .build()?
///     .fetch_optional(&pool)
///     .await?;
///
/// let ids: Vec<i32> = vec![1, 2, 3];
/// sqlx::query_interp::<Sqlite>("SELECT * FROM users WHERE id IN ({ids*})")
///     .bind_iter_named("ids", ids)
///     .build()?
///     .fetch_all(&pool)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub fn query_interp<'q, DB: Database>(sql: impl Into<String>) -> InterpQuery<'q, DB> {
    InterpQuery {
        sql: sql.into(),
        positional: Vec::new(),
        named: HashMap::new(),
    }
}

/// A query under construction, accepting `bind` / `bind_named` calls.
#[must_use = "query must be executed to affect database"]
pub struct InterpQuery<'q, DB: Database> {
    sql: String,
    positional: Vec<Bound<'q, DB>>,
    named: HashMap<String, Bound<'q, DB>>,
}

impl<'q, DB: Database> InterpQuery<'q, DB> {
    /// Bind a positional argument. Positional bindings are tracked separately
    /// from named bindings: the first `bind` call binds `{0}`, the second
    /// `{1}`, regardless of any interleaved `bind_named` calls.
    ///
    /// Each placeholder may be referenced at most once: a `{0}` that appears
    /// twice in the SQL fails at [`build`](Self::build) with a "referenced
    /// more than once" error. (The `query!` macro is more permissive — it
    /// clones the bound expression and re-evaluates it per reference.)
    pub fn bind<T>(mut self, value: T) -> Self
    where
        T: 'q + Send + Type<DB> + Encode<'q, DB>,
    {
        self.positional.push(Bound::Single(erase(value)));
        self
    }

    /// Bind a named argument matched against `{name}` in the SQL.
    ///
    /// See [`bind`](Self::bind) for the single-reference restriction.
    pub fn bind_named<T>(mut self, name: impl Into<String>, value: T) -> Self
    where
        T: 'q + Send + Type<DB> + Encode<'q, DB>,
    {
        self.named.insert(name.into(), Bound::Single(erase(value)));
        self
    }

    /// Bind a positional spread argument matched against `{N*}` / `{N+}`.
    /// The empty-iterable behavior is governed by the SQL marker: `*` expands
    /// to nothing, `+` expands to `NULL`.
    ///
    /// See [`bind`](Self::bind) for the single-reference restriction.
    pub fn bind_iter<I, T>(mut self, iter: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: 'q + Send + Type<DB> + Encode<'q, DB>,
    {
        self.positional
            .push(Bound::Spread(iter.into_iter().map(erase).collect()));
        self
    }

    /// Bind a named spread argument matched against `{name*}` / `{name+}`.
    /// The empty-iterable behavior is governed by the SQL marker: `*` expands
    /// to nothing, `+` expands to `NULL`.
    ///
    /// See [`bind`](Self::bind) for the single-reference restriction.
    pub fn bind_iter_named<I, T>(mut self, name: impl Into<String>, iter: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: 'q + Send + Type<DB> + Encode<'q, DB>,
    {
        self.named.insert(
            name.into(),
            Bound::Spread(iter.into_iter().map(erase).collect()),
        );
        self
    }

    /// Finalize the SQL rewrite and produce a [`Query`] ready for execution.
    pub fn build(self) -> Result<Query<'q, DB, ImmutableArguments<DB>>, Error> {
        let (sql, args) = self.assemble()?;
        Ok(query_with_result(
            AssertSqlSafe(sql),
            Ok(ImmutableArguments(args)),
        ))
    }

    /// Like [`build`](Self::build), but maps each row into `O` via [`FromRow`].
    pub fn build_as<O>(self) -> Result<QueryAs<'q, DB, O, ImmutableArguments<DB>>, Error>
    where
        O: for<'r> FromRow<'r, DB::Row>,
    {
        let (sql, args) = self.assemble()?;
        Ok(query_as_with(AssertSqlSafe(sql), ImmutableArguments(args)))
    }

    /// Like [`build`](Self::build), but decodes a single column per row as `O`.
    pub fn build_scalar<O>(self) -> Result<QueryScalar<'q, DB, O, ImmutableArguments<DB>>, Error>
    where
        (O,): for<'r> FromRow<'r, DB::Row>,
    {
        let (sql, args) = self.assemble()?;
        Ok(query_scalar_with(
            AssertSqlSafe(sql),
            ImmutableArguments(args),
        ))
    }

    fn assemble(self) -> Result<(String, DB::Arguments), Error> {
        let parsed = parse_query(&self.sql).map_err(|e| Error::Configuration(e.into()))?;
        let mut positional: Vec<Option<Bound<'q, DB>>> =
            self.positional.into_iter().map(Some).collect();
        let mut named = self.named;

        if !parsed.had_placeholder {
            if positional.is_empty() && named.is_empty() {
                return Ok((self.sql, DB::Arguments::default()));
            }
            return Err(Error::Configuration(
                "bindings provided but the SQL contains no `{...}` placeholders; \
                 use `sqlx::query()` for SQL with native `?` / `$N` placeholders"
                    .into(),
            ));
        }

        let mut args = DB::Arguments::default();
        args.reserve(
            positional.iter().flatten().map(Bound::len).sum::<usize>()
                + named.values().map(Bound::len).sum::<usize>(),
            positional
                .iter()
                .flatten()
                .map(Bound::size_hint)
                .sum::<usize>()
                + named.values().map(Bound::size_hint).sum::<usize>(),
        );

        let mut out = String::with_capacity(self.sql.len());
        for frag in parsed.fragments {
            match frag {
                Fragment::Literal(s) => out.push_str(&s),
                Fragment::Placeholder(p) => {
                    let (bound, name) = take_bound(&mut positional, &mut named, &p.ident)?;
                    emit(&mut out, &mut args, bound, p.kleene, &name)?;
                }
            }
        }
        Ok((out, args))
    }
}

/// One slot in the bindings table.
enum Bound<'q, DB: Database> {
    Single(Erased<'q, DB>),
    Spread(Vec<Erased<'q, DB>>),
}

type Erased<'q, DB> = Box<dyn ErasedBind<DB> + 'q>;

/// Type-erased bind value with deferred encoding. Held until the placeholder
/// position is known, then consumed by [`Arguments::add`].
trait ErasedBind<DB: Database>: Send {
    fn size_hint(&self) -> usize;
    fn add_to(self: Box<Self>, args: &mut DB::Arguments) -> Result<(), BoxDynError>;
}

struct Single<T>(T);

impl<'q, DB, T> ErasedBind<DB> for Single<T>
where
    DB: Database,
    T: 'q + Send + Type<DB> + Encode<'q, DB>,
{
    fn size_hint(&self) -> usize {
        Encode::<'q, DB>::size_hint(&self.0)
    }

    fn add_to(self: Box<Self>, args: &mut DB::Arguments) -> Result<(), BoxDynError> {
        args.add(self.0)
    }
}

fn erase<'q, DB, T>(value: T) -> Erased<'q, DB>
where
    DB: Database,
    T: 'q + Send + Type<DB> + Encode<'q, DB>,
{
    Box::new(Single(value))
}

impl<DB: Database> Bound<'_, DB> {
    fn len(&self) -> usize {
        match self {
            Bound::Single(_) => 1,
            Bound::Spread(v) => v.len(),
        }
    }
    fn size_hint(&self) -> usize {
        match self {
            Bound::Single(b) => b.size_hint(),
            Bound::Spread(v) => v.iter().map(|b| b.size_hint()).sum(),
        }
    }
}

fn take_bound<'q, DB: Database>(
    positional: &mut [Option<Bound<'q, DB>>],
    named: &mut HashMap<String, Bound<'q, DB>>,
    ident: &Ident<'_>,
) -> Result<(Bound<'q, DB>, String), Error> {
    match ident {
        Ident::Positional(i) => {
            let n = positional.len();
            let i = usize::from(*i);
            let slot = positional.get_mut(i).ok_or_else(|| {
                Error::Configuration(
                    format!(
                        "positional placeholder `{{{i}}}` referenced but only {n} positional \
                         argument(s) were bound"
                    )
                    .into(),
                )
            })?;
            let bound = slot.take().ok_or_else(|| {
                Error::Configuration(
                    format!("positional placeholder `{{{i}}}` referenced more than once").into(),
                )
            })?;
            Ok((bound, i.to_string()))
        }
        Ident::Named(name) => {
            let bound = named.remove(&**name).ok_or_else(|| {
                Error::Configuration(
                    format!(
                        "named placeholder `{{{name}}}` referenced but no matching \
                         bind_named/bind_iter_named was called"
                    )
                    .into(),
                )
            })?;
            Ok((bound, name.to_string()))
        }
        Ident::Expr(_) => Err(Error::Configuration(
            "complex-expression placeholders `{(expr)}` are not supported at runtime".into(),
        )),
    }
}

fn emit<DB: Database>(
    out: &mut String,
    args: &mut DB::Arguments,
    bound: Bound<'_, DB>,
    kleene: Option<Kleene>,
    name: &str,
) -> Result<(), Error> {
    match (bound, kleene) {
        (Bound::Single(value), None) => bind_one(out, args, value),
        (Bound::Spread(values), Some(k)) => {
            if values.is_empty() {
                if matches!(k, Kleene::Plus) {
                    out.push_str("NULL");
                }
                Ok(())
            } else {
                for (count, value) in values.into_iter().enumerate() {
                    if count > 0 {
                        out.push_str(", ");
                    }
                    bind_one(out, args, value)?;
                }
                Ok(())
            }
        }
        (Bound::Single(_), Some(k)) => {
            let marker = match k {
                Kleene::Star => '*',
                Kleene::Plus => '+',
            };
            Err(Error::Configuration(
                format!(
                    "placeholder `{{{name}{marker}}}` requires a spread bind \
                     (use `bind_iter` / `bind_iter_named`)"
                )
                .into(),
            ))
        }
        (Bound::Spread(_), None) => Err(Error::Configuration(
            format!(
                "placeholder `{{{name}}}` was bound as a spread iterable but the SQL \
                 marker is not `*` or `+`"
            )
            .into(),
        )),
    }
}

fn bind_one<DB: Database>(
    out: &mut String,
    args: &mut DB::Arguments,
    value: Erased<'_, DB>,
) -> Result<(), Error> {
    // Order matters: `Arguments::format_placeholder` for numbered formats reads
    // `Arguments::len()`, which `add` increments.
    value.add_to(args).map_err(Error::Configuration)?;
    args.format_placeholder(out)
        .expect("writing to String is infallible");
    Ok(())
}
