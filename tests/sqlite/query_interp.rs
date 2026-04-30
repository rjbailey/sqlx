use sqlx::Sqlite;
use sqlx_test::new;

#[sqlx_macros::test]
async fn interp_positional() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let n: i32 = sqlx::query_interp::<Sqlite>("SELECT {0}")
        .bind(42i32)
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(n, 42);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_named() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let n: i32 = sqlx::query_interp::<Sqlite>("SELECT {id}")
        .bind_named("id", 42i32)
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(n, 42);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_no_placeholders_passthrough() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let n: i32 = sqlx::query_interp::<Sqlite>("SELECT 1")
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(n, 1);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_brace_inside_string_passes_through() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let s: String = sqlx::query_interp::<Sqlite>("SELECT '{not_a_placeholder}'")
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(s, "{not_a_placeholder}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_named_nonempty() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let n: i32 = sqlx::query_interp::<Sqlite>(
        "SELECT count(*) FROM (SELECT 1 AS x UNION SELECT 2 UNION SELECT 3) WHERE x IN ({ids*})",
    )
    .bind_iter_named("ids", vec![1i32, 2, 3])
    .build_scalar()?
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(n, 3);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_plus_empty_yields_null() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let n: i32 =
        sqlx::query_interp::<Sqlite>("SELECT count(*) FROM (SELECT 1 AS x) WHERE x IN ({ids+})")
            .bind_iter_named("ids", Vec::<i32>::new())
            .build_scalar()?
            .fetch_one(&mut conn)
            .await?;
    assert_eq!(n, 0);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_with_other_singles() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let n: i32 = sqlx::query_interp::<Sqlite>(
        "SELECT count(*) FROM (SELECT 1 AS x UNION SELECT 2) WHERE x IN ({ids*}) AND x <= {max}",
    )
    .bind_iter_named("ids", vec![1i32, 2])
    .bind_named("max", 1i32)
    .build_scalar()?
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(n, 1);
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct Account {
    id: i64,
    name: String,
}

#[sqlx_macros::test]
async fn interp_build_as() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let a: Account = sqlx::query_interp::<Sqlite>("SELECT id, name FROM accounts WHERE id = {id}")
        .bind_named("id", 1i32)
        .build_as()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(a.id, 1);
    assert_eq!(a.name, "Herp Derpinson");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_borrowed_str() -> anyhow::Result<()> {
    // Borrowed `&str` works without `.to_string()` thanks to the `'q` parameter.
    let mut conn = new::<Sqlite>().await?;
    let name = String::from("Herp Derpinson");
    let id: i64 = sqlx::query_interp::<Sqlite>("SELECT id FROM accounts WHERE name = {name}")
        .bind_named("name", name.as_str())
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(id, 1);
    Ok(())
}

fn err_msg(q: sqlx::InterpQuery<'_, Sqlite>) -> String {
    match q.build() {
        Ok(_) => panic!("expected build to fail"),
        Err(e) => e.to_string(),
    }
}

#[sqlx_macros::test]
async fn interp_named_missing_bind_errors() -> anyhow::Result<()> {
    let msg = err_msg(sqlx::query_interp::<Sqlite>("SELECT {missing}"));
    assert!(msg.contains("missing"), "{msg}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_positional_out_of_range_errors() -> anyhow::Result<()> {
    let msg = err_msg(sqlx::query_interp::<Sqlite>("SELECT {3}").bind(1i32));
    assert!(msg.contains("only"), "{msg}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_bindings_without_placeholders_errors() -> anyhow::Result<()> {
    let msg = err_msg(sqlx::query_interp::<Sqlite>("SELECT 1").bind_named("x", 5i32));
    assert!(msg.contains("no `{...}` placeholders"), "{msg}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_bound_to_single_placeholder_errors() -> anyhow::Result<()> {
    let msg =
        err_msg(sqlx::query_interp::<Sqlite>("SELECT {ids}").bind_iter_named("ids", vec![1i32, 2]));
    assert!(msg.contains("spread"), "{msg}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_single_bound_to_spread_placeholder_errors() -> anyhow::Result<()> {
    let msg = err_msg(
        sqlx::query_interp::<Sqlite>("SELECT count(*) FROM (SELECT 1 AS x) WHERE x IN ({ids*})")
            .bind_named("ids", 1i32),
    );
    assert!(msg.contains("spread"), "{msg}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_single_bound_to_plus_spread_uses_plus_marker() -> anyhow::Result<()> {
    // Spread/single mismatch errors include the SQL marker (`+` or `*`)
    // from the placeholder, not a hardcoded value.
    let msg = err_msg(
        sqlx::query_interp::<Sqlite>("SELECT count(*) FROM (SELECT 1 AS x) WHERE x IN ({ids+})")
            .bind_named("ids", 1i32),
    );
    assert!(msg.contains("{ids+}"), "{msg}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_complex_expression_rejected() -> anyhow::Result<()> {
    // `{(expr)}` is macro-only — the runtime cannot evaluate Rust expressions.
    let msg = err_msg(sqlx::query_interp::<Sqlite>("SELECT {(foo.bar)}"));
    assert!(msg.contains("complex-expression"), "{msg}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_positional() -> anyhow::Result<()> {
    // `bind_iter` binds the iterable for a positional spread placeholder
    // (`{N*}` / `{N+}`).
    let mut conn = new::<Sqlite>().await?;
    let n: i32 = sqlx::query_interp::<Sqlite>(
        "SELECT count(*) FROM (SELECT 1 AS x UNION SELECT 2) WHERE x IN ({0*})",
    )
    .bind_iter(vec![1i32, 2])
    .build_scalar()?
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(n, 2);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_repeated_named_placeholder_errors() -> anyhow::Result<()> {
    // Unlike the macro (which clones the bound expression and re-evaluates),
    // the runtime consumes each binding on first reference.
    let msg = err_msg(sqlx::query_interp::<Sqlite>("SELECT {x} + {x}").bind_named("x", 5i32));
    assert!(msg.contains("no matching"), "{msg}");
    Ok(())
}
