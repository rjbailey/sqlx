use sqlx::Postgres;
use sqlx_test::new;

#[sqlx_macros::test]
async fn interp_positional() -> anyhow::Result<()> {
    let mut conn = new::<Postgres>().await?;
    let n: i32 = sqlx::query_interp::<Postgres>("SELECT {0}::int4")
        .bind(42i32)
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(n, 42);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_named() -> anyhow::Result<()> {
    let mut conn = new::<Postgres>().await?;
    let n: i32 = sqlx::query_interp::<Postgres>("SELECT {id}::int4")
        .bind_named("id", 42i32)
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(n, 42);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_dollar_quote_left_alone() -> anyhow::Result<()> {
    // `$tag$ ... $tag$` is a Postgres dollar-quoted string. Braces inside
    // must NOT be interpreted as placeholders.
    let mut conn = new::<Postgres>().await?;
    let s: String = sqlx::query_interp::<Postgres>("SELECT $tag${not_a_placeholder}$tag$::text")
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(s, "{not_a_placeholder}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_in_clause() -> anyhow::Result<()> {
    let mut conn = new::<Postgres>().await?;
    let n: i64 = sqlx::query_interp::<Postgres>(
        "SELECT count(*)::int8 FROM (VALUES (1::int4), (2), (3)) v(x) WHERE x IN ({ids*})",
    )
    .bind_iter_named("ids", vec![1i32, 2])
    .build_scalar()?
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(n, 2);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_with_single_after_keeps_numbering() -> anyhow::Result<()> {
    // Verifies that `$N` numbering stays correct across a runtime spread.
    // Bound `ids` expands to `$1, $2`, and `{max}` must come out as `$3`.
    let mut conn = new::<Postgres>().await?;
    let n: i64 = sqlx::query_interp::<Postgres>(
        "SELECT count(*)::int8 FROM (VALUES (10::int4), (20), (30), (40)) v(x) \
         WHERE x = ANY(ARRAY[{ids*}]::int4[]) AND x <= {max}::int4",
    )
    .bind_iter_named("ids", vec![10i32, 20, 30])
    .bind_named("max", 25i32)
    .build_scalar()?
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(n, 2); // 10 and 20 satisfy both clauses.
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_plus_empty_yields_null() -> anyhow::Result<()> {
    let mut conn = new::<Postgres>().await?;
    let n: i64 = sqlx::query_interp::<Postgres>(
        "SELECT count(*)::int8 FROM (VALUES (1::int4)) v(x) WHERE x IN ({ids+})",
    )
    .bind_iter_named("ids", Vec::<i32>::new())
    .build_scalar()?
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(n, 0);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_borrowed_str() -> anyhow::Result<()> {
    let mut conn = new::<Postgres>().await?;
    let name = String::from("alice");
    let s: String = sqlx::query_interp::<Postgres>("SELECT {n}::text")
        .bind_named("n", name.as_str())
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(s, "alice");
    Ok(())
}
