use sqlx::MySql;
use sqlx_test::new;

#[sqlx_macros::test]
async fn interp_positional() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let n: i32 = sqlx::query_interp::<MySql>("SELECT CAST({0} AS SIGNED)")
        .bind(42i32)
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(n, 42);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_named() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let n: i32 = sqlx::query_interp::<MySql>("SELECT CAST({id} AS SIGNED)")
        .bind_named("id", 42i32)
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(n, 42);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_brace_inside_string_passes_through() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let s: String = sqlx::query_interp::<MySql>("SELECT '{not_a_placeholder}'")
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(s, "{not_a_placeholder}");
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_in_clause() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let n: i64 = sqlx::query_interp::<MySql>(
        "SELECT count(*) FROM (SELECT 1 AS x UNION SELECT 2 UNION SELECT 3) AS v WHERE x IN ({ids*})",
    )
    .bind_iter_named("ids", vec![1i32, 2])
    .build_scalar()?
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(n, 2);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_with_singles() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let n: i64 = sqlx::query_interp::<MySql>(
        "SELECT count(*) FROM (SELECT 1 AS x UNION SELECT 2) AS v \
         WHERE x IN ({ids*}) AND x <= CAST({max} AS SIGNED)",
    )
    .bind_iter_named("ids", vec![1i32, 2])
    .bind_named("max", 1i32)
    .build_scalar()?
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(n, 1);
    Ok(())
}

#[sqlx_macros::test]
async fn interp_spread_plus_empty_yields_null() -> anyhow::Result<()> {
    let mut conn = new::<MySql>().await?;
    let n: i64 = sqlx::query_interp::<MySql>(
        "SELECT count(*) FROM (SELECT 1 AS x) AS v WHERE x IN ({ids+})",
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
    let mut conn = new::<MySql>().await?;
    let name = String::from("alice");
    let s: String = sqlx::query_interp::<MySql>("SELECT {n}")
        .bind_named("n", name.as_str())
        .build_scalar()?
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(s, "alice");
    Ok(())
}
