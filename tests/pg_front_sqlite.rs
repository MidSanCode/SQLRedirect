//! Integration test: PostgreSQL wire clients talking to a SQLite backend
//! through the proxy (no changes to the client application code).

mod common;

use common::*;
use sqlredirect::translate::FrontDialect;
use sqlredirect::translate::TargetDialect;
use sqlredirect::translate::Translator;

#[tokio::test]
async fn pg_client_over_sqlite_backend() {
    init_drivers();
    let db = temp_db("pg");
    let port = start_pg(&db).await;
    let c = pg_client(port).await;

    // DDL: PG syntax with SERIAL pseudo-type.
    c.execute(
        "CREATE TABLE users (
            id SERIAL PRIMARY KEY,
            name VARCHAR(100) NOT NULL,
            age INT DEFAULT 0
        )",
        &[],
    )
    .await
    .expect("create table");

    // DML with parameters (extended protocol, $1 placeholders).
    c.execute(
        "INSERT INTO users (name, age) VALUES ($1, $2)",
        &[&"Alice", &30i32],
    )
    .await
    .expect("insert alice");
    c.execute(
        "INSERT INTO users (name, age) VALUES ($1, $2)",
        &[&"Bob", &25i32],
    )
    .await
    .expect("insert bob");

    // SELECT round-trip.
    let rows = c
        .query("SELECT id, name, age FROM users ORDER BY id", &[])
        .await
        .expect("select");
    assert_eq!(rows.len(), 2);
    let id: i32 = rows[0].get(0);
    let name: &str = rows[0].get(1);
    let age: i32 = rows[0].get(2);
    assert_eq!((id, name, age), (1, "Alice", 30));

    // UPDATE with affected-row count.
    let n = c
        .execute("UPDATE users SET age = age + 1 WHERE name = $1", &[&"Bob"])
        .await
        .unwrap();
    assert_eq!(n, 1);

    // LIMIT / OFFSET.
    let rows = c
        .query("SELECT name FROM users ORDER BY id LIMIT 1 OFFSET 1", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let name: &str = rows[0].get(0);
    assert_eq!(name, "Bob");

    // String concat operator.
    let rows = c.query("SELECT 'a' || 'b' || name FROM users WHERE id = 1", &[]).await.unwrap();
    let full: &str = rows[0].get(0);
    assert_eq!(full, "abAlice");

    // Transactions across the proxy.
    c.execute("BEGIN", &[]).await.unwrap();
    c.execute("INSERT INTO users (name) VALUES ($1)", &[&"Carol"])
        .await
        .unwrap();
    c.execute("ROLLBACK", &[]).await.unwrap();
    let rows = c.query("SELECT count(*) FROM users", &[]).await.unwrap();
    let cnt: i64 = rows[0].get(0);
    assert_eq!(cnt, 2, "rolled-back insert must not persist");

    c.execute("BEGIN", &[]).await.unwrap();
    c.execute("INSERT INTO users (name) VALUES ($1)", &[&"Dave"])
        .await
        .unwrap();
    c.execute("COMMIT", &[]).await.unwrap();
    let rows = c.query("SELECT count(*) FROM users", &[]).await.unwrap();
    let cnt: i64 = rows[0].get(0);
    assert_eq!(cnt, 3, "committed insert must persist");
}

#[tokio::test]
async fn pg_prepared_statements_and_types() {
    init_drivers();
    let db = temp_db("pgprep");
    let port = start_pg(&db).await;
    let c = pg_client(port).await;

    c.execute("CREATE TABLE t (a INTEGER, b BIGINT, f DOUBLE PRECISION, s TEXT)", &[])
        .await
        .unwrap();

    let stmt = c
        .prepare("INSERT INTO t (a, b, f, s) VALUES ($1, $2, $3, $4)")
        .await
        .unwrap();
    for i in 0..3i32 {
        c.execute(&stmt, &[&i, &(i as i64 * 1_000_000_000), &(i as f64 / 2.0), &format!("row{i}")])
            .await
            .unwrap();
    }

    let sel = c.prepare("SELECT a, b, f, s FROM t WHERE a >= $1 ORDER BY a").await.unwrap();
    let rows = c.query(&sel, &[&1i32]).await.unwrap();
    assert_eq!(rows.len(), 2);
    let a: i32 = rows[0].get(0);
    let b: i64 = rows[0].get(1);
    let f: f64 = rows[0].get(2);
    let s: &str = rows[0].get(3);
    assert_eq!(a, 1);
    assert_eq!(b, 1_000_000_000);
    assert!((f - 0.5).abs() < 1e-9);
    assert_eq!(s, "row1");
}

/// The translated SQL that reaches the backend must be SQLite-valid; verify by
/// running the translator directly on statements a PG app would send.
#[test]
fn pg_to_sqlite_translation_samples() {
    let tr = Translator::new(FrontDialect::Postgres, TargetDialect::Sqlite);
    let out = tr.translate("SELECT * FROM t WHERE name ILIKE 'ab%' LIMIT 10").unwrap();
    assert!(!out.to_uppercase().contains("ILIKE"));
}
