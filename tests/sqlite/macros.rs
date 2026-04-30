use sqlx::Sqlite;
use sqlx_test::new;

#[sqlx_macros::test]
async fn macro_select() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let account = sqlx::query!("select id, name, is_active from accounts where id = 1")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(1, account.id);
    assert_eq!("Herp Derpinson", account.name);
    assert_eq!(account.is_active, Some(true));

    Ok(())
}

macro_rules! gen_macro_select_concats {
    ($param:literal) => {
        #[sqlx_macros::test]
        async fn macro_select_concat_single() -> anyhow::Result<()> {
            let mut conn = new::<Sqlite>().await?;

            let account = sqlx::query!("select " + $param + " from accounts where id = 1")
                .fetch_one(&mut conn)
                .await?;

            assert_eq!(1, account.id);
            assert_eq!("Herp Derpinson", account.name);
            assert_eq!(account.is_active, Some(true));

            Ok(())
        }
    };
}

gen_macro_select_concats!("id, name, is_active");

#[sqlx_macros::test]
async fn macro_select_expression() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let row = sqlx::query!("select 10 as _1, 'Hello' as _2")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(10, row._1);
    assert_eq!("Hello", &*row._2);

    Ok(())
}

#[sqlx_macros::test]
async fn macro_select_partial_expression() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let row = sqlx::query!(
        "select 10 as _1, 'Hello' as _2, is_active, name, id + 5 as id_p from accounts where id = 1"
    )
    .fetch_one(&mut conn)
    .await?;

    assert_eq!(10, row._1);
    assert_eq!("Hello", &*row._2);
    assert_eq!(6, row.id_p);
    assert_eq!("Herp Derpinson", row.name);
    assert_eq!(row.is_active, Some(true));

    Ok(())
}

#[sqlx_macros::test]
async fn macro_select_bind() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let account = sqlx::query!(
        "select id, name, is_active from accounts where id = ?",
        1i32
    )
    .fetch_one(&mut conn)
    .await?;

    assert_eq!(1, account.id);
    assert_eq!("Herp Derpinson", account.name);
    assert_eq!(account.is_active, Some(true));

    Ok(())
}

#[derive(Debug)]
struct RawAccount {
    id: i64,
    name: String,
    is_active: Option<bool>,
}

#[sqlx_macros::test]
async fn test_query_as_raw() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let account = sqlx::query_as!(RawAccount, "SELECT id, name, is_active from accounts")
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(account.id, 1);
    assert_eq!(account.name, "Herp Derpinson");
    assert_eq!(account.is_active, Some(true));

    Ok(())
}

#[sqlx_macros::test]
async fn test_query_scalar() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let id = sqlx::query_scalar!("select 1").fetch_one(&mut conn).await?;
    assert_eq!(id, 1i64);

    // invalid column names are ignored
    let id = sqlx::query_scalar!(r#"select 1 as "&foo""#)
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(id, 1i64);

    let id = sqlx::query_scalar!(r#"select 1 as "foo!""#)
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(id, 1i64);

    let id = sqlx::query_scalar!(r#"select 1 as "foo?""#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(id, Some(1i64));

    let id = sqlx::query_scalar!(r#"select 1 as "foo: MyInt""#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(id, MyInt(1i64));

    let id = sqlx::query_scalar!(r#"select 1 as "foo?: MyInt""#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(id, Some(MyInt(1i64)));

    let id = sqlx::query_scalar!(r#"select 1 as "foo!: MyInt""#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(id, MyInt(1i64));

    let id: MyInt = sqlx::query_scalar!(r#"select 1 as "foo: _""#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(id, MyInt(1i64));

    let id: MyInt = sqlx::query_scalar!(r#"select 1 as "foo?: _""#)
        .fetch_one(&mut conn)
        .await?
        // don't hint that it should be `Option<MyInt>`
        .unwrap();

    assert_eq!(id, MyInt(1i64));

    let id: MyInt = sqlx::query_scalar!(r#"select 1 as "foo!: _""#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(id, MyInt(1i64));

    Ok(())
}

#[sqlx_macros::test]
async fn query_by_string() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let string = "Hello, world!".to_string();
    let ref tuple = ("Hello, world!".to_string(),);

    let result = sqlx::query!(
        "SELECT 'Hello, world!' as string where 'Hello, world!' in (?, ?, ?, ?, ?, ?, ?)",
        string, // make sure we don't actually take ownership here
        &string[..],
        Some(&string),
        Some(&string[..]),
        Option::<String>::None,
        string.clone(),
        tuple.0 // make sure we're not trying to move out of a field expression
    )
    .fetch_one(&mut conn)
    .await?;

    assert_eq!(result.string, string);

    Ok(())
}

#[sqlx_macros::test]
async fn macro_select_from_view() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let account = sqlx::query!("SELECT id, name, is_active from accounts_view")
        .fetch_one(&mut conn)
        .await?;

    // SQLite tells us the true origin of these columns even through the view
    assert_eq!(account.id, 1);
    assert_eq!(account.name, "Herp Derpinson");
    assert_eq!(account.is_active, Some(true));

    Ok(())
}

#[sqlx_macros::test]
async fn test_column_override_not_null() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let record = sqlx::query!(r#"select owner_id as `owner_id!` from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.owner_id, 1);

    Ok(())
}

#[sqlx_macros::test]
async fn test_column_override_nullable() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let record = sqlx::query!(r#"select text as `text?` from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.text.as_deref(), Some("#sqlx is pretty cool!"));

    Ok(())
}

#[derive(PartialEq, Eq, Debug, sqlx::Type)]
#[sqlx(transparent)]
struct MyInt(i64);

struct Record {
    id: MyInt,
}

struct OptionalRecord {
    id: Option<MyInt>,
}

#[sqlx_macros::test]
async fn test_column_override_wildcard() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let record = sqlx::query_as!(Record, r#"select id as "id: _" from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, MyInt(1));

    // this syntax is also useful for expressions
    let record = sqlx::query_as!(Record, r#"select 1 as "id: _""#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, MyInt(1));

    let record = sqlx::query_as!(OptionalRecord, r#"select owner_id as "id: _" from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, Some(MyInt(1)));

    Ok(())
}

#[sqlx_macros::test]
async fn test_column_override_wildcard_not_null() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let record = sqlx::query_as!(Record, r#"select owner_id as "id!: _" from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, MyInt(1));

    Ok(())
}

#[sqlx_macros::test]
async fn test_column_override_wildcard_nullable() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let record = sqlx::query_as!(OptionalRecord, r#"select id as "id?: _" from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, Some(MyInt(1)));

    Ok(())
}

#[sqlx_macros::test]
async fn test_column_override_exact() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let record = sqlx::query!(r#"select id as "id: MyInt" from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, MyInt(1));

    // we can also support this syntax for expressions
    let record = sqlx::query!(r#"select 1 as "id: MyInt""#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, MyInt(1));

    let record = sqlx::query!(r#"select owner_id as "id: MyInt" from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, Some(MyInt(1)));

    Ok(())
}

#[sqlx_macros::test]
async fn test_column_override_exact_not_null() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let record = sqlx::query!(r#"select owner_id as "id!: MyInt" from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, MyInt(1));

    Ok(())
}

#[sqlx_macros::test]
async fn test_column_override_exact_nullable() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;

    let record = sqlx::query!(r#"select id as "id?: MyInt" from tweet"#)
        .fetch_one(&mut conn)
        .await?;

    assert_eq!(record.id, Some(MyInt(1)));

    Ok(())
}

// we don't emit bind parameter typechecks for SQLite so testing the overrides is redundant

// `{...}` placeholder syntax (issue #875)

#[sqlx_macros::test]
async fn placeholder_positional() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let id = sqlx::query_scalar!("select id from accounts where id = {0}", 1i32)
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(id, 1);
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_named() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let id = sqlx::query_scalar!("select id from accounts where id = {id}", id = 1i32)
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(id, 1);
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_inline_capture_of_local() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let id = 1i32;
    let row_id = sqlx::query_scalar!("select id from accounts where id = {id}")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(row_id, 1);
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_brace_escape_in_string() -> anyhow::Result<()> {
    // `{not_a_placeholder}` inside a SQL string is left alone.
    let mut conn = new::<Sqlite>().await?;
    let row = sqlx::query!("SELECT '{not_a_placeholder}' as got")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(&*row.got, "{not_a_placeholder}");
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_spread_star_nonempty() -> anyhow::Result<()> {
    // Self-contained VALUES clause so the test doesn't depend on `accounts`
    // table state (other tests in the same suite mutate it).
    let mut conn = new::<Sqlite>().await?;
    let ids = vec![1i32, 2, 3];
    let row = sqlx::query!(
        r#"SELECT count(*) AS `n!: i32` FROM
              (SELECT 1 AS x UNION SELECT 2 UNION SELECT 3 UNION SELECT 4) v
           WHERE x IN ({ids*})"#,
        ids = &ids
    )
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(row.n, 3);
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_spread_plus_empty_yields_null() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let ids: Vec<i32> = Vec::new();
    let rows = sqlx::query!("SELECT id FROM accounts WHERE id IN ({ids+})", ids = &ids)
        .fetch_all(&mut conn)
        .await?;
    assert_eq!(rows.len(), 0);
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_mixed_positional_and_named() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let row = sqlx::query!(
        r#"SELECT {0} as `a!: i32`, {b} as `b!: i32`"#,
        1i32,
        b = 2i32
    )
    .fetch_one(&mut conn)
    .await?;
    assert_eq!((row.a, row.b), (1, 2));
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_complex_expression() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    struct Filter {
        id: i32,
    }
    let f = Filter { id: 1 };
    let id = sqlx::query_scalar!("select id from accounts where id = {(f.id)}")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(id, 1);
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_rest_spread_and_override() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    #[allow(dead_code)] // `id` is unread because it's overridden by the named arg.
    struct Filter {
        id: i32,
        name: String,
    }
    let f = Filter {
        id: 99, // overridden by `id = 1i32` below
        name: "Herp Derpinson".to_string(),
    };
    let id = sqlx::query_scalar!(
        "select id from accounts where id = {id} AND name = {name}",
        id = 1i32,
        ..f
    )
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(id, 1);
    Ok(())
}

// All `query_*!` variants forward through the same `expand_query!` pipeline,
// so one test per variant is enough to pin variant-level dispatch.

#[sqlx_macros::test]
async fn placeholder_with_query_as() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let id = 1i32;
    let account = sqlx::query_as!(
        RawAccount,
        "select id, name, is_active from accounts where id = {id}"
    )
    .fetch_one(&mut conn)
    .await?;
    assert_eq!(account.id, 1);
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_with_query_unchecked() -> anyhow::Result<()> {
    let mut conn = new::<Sqlite>().await?;
    let id = 1i32;
    let account =
        sqlx::query_unchecked!("select id, name, is_active from accounts where id = {id}")
            .fetch_one(&mut conn)
            .await?;
    assert_eq!(account.id, 1);
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_question_mark_in_string_literal() -> anyhow::Result<()> {
    // `?` inside a SQL string literal is not a native placeholder, and the
    // rewriter must accept the query when it appears alongside `{...}`.
    let mut conn = new::<Sqlite>().await?;
    let row = sqlx::query!("SELECT 'a?b' AS got WHERE 1 = {x}", x = 1i32)
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(&*row.got, "a?b");
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_reserved_keyword_inline_capture() -> anyhow::Result<()> {
    // Reserved Rust keywords (`type`, `match`, etc.) can appear as placeholder
    // names; the macro emits raw idents which resolve to user-scoped locals
    // declared with the corresponding `r#`-prefix.
    let mut conn = new::<Sqlite>().await?;
    let r#type = 1i32;
    let id = sqlx::query_scalar!("select id from accounts where id = {type}")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(id, 1);
    Ok(())
}

#[sqlx_macros::test]
async fn placeholder_spread_star_empty_yields_in_empty() -> anyhow::Result<()> {
    // Documented contract: `{ids*}` with an empty iterable expands to nothing.
    // SQLite tolerates `IN ()` (always FALSE); other databases may reject it
    // as a syntax error. Users who may pass an empty collection should use
    // `{ids+}` instead.
    let mut conn = new::<Sqlite>().await?;
    let ids: Vec<i32> = Vec::new();
    let rows = sqlx::query!("SELECT id FROM accounts WHERE id IN ({ids*})", ids = &ids)
        .fetch_all(&mut conn)
        .await?;
    assert_eq!(rows.len(), 0);
    Ok(())
}
