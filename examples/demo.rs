//! End-to-end demo: start SQLRedirect in-process, connect with a real
//! PostgreSQL client library (tokio-postgres), and run a typical workload.
//!
//! The listener fronts a SQLite file database, so the demo needs no external
//! database server:
//!
//! ```text
//! tokio-postgres ──pgwire──▶ SQLRedirect ──translate──▶ SQLite file
//! ```
//!
//! Run with: cargo run --example demo

use std::sync::Arc;

use sqlredirect::backend::Backend;
use sqlredirect::server::pg::PgHandlers;
use sqlredirect::translate::FrontDialect;
use tokio_postgres::NoTls;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("sqlredirect=info")
        .init();
    sqlx::any::install_default_drivers();

    // 1. Bind an ephemeral port first so the client URL is known up front.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    // 2. Point the proxy at a throwaway SQLite file.
    let db = std::env::temp_dir().join("sqlredirect-demo.db");
    let _ = std::fs::remove_file(&db);
    let db_url = format!("sqlite:///{}?mode=rwc", db.display().to_string().replace('\\', "/"));

    let backend = Backend::connect(&db_url, 4).await?;
    println!("backend   : {db_url}");
    println!("dialect   : {:?}", backend.dialect());

    // 3. Serve the PostgreSQL wire protocol with demo credentials.
    let handler = Arc::new(PgHandlers::new(
        backend,
        FrontDialect::Postgres,
        Some("demo".into()),
        Some("demo".into()),
    )?);
    tokio::spawn(async move { let _ = handler.serve_on(listener).await; });
    println!("listening : postgres://demo:demo@127.0.0.1:{port}");
    println!();

    // 4. Connect a real PostgreSQL client through the proxy.
    let (client, conn) = tokio_postgres::connect(
        &format!("host=127.0.0.1 port={port} user=demo password=demo"),
        NoTls,
    )
    .await?;
    tokio::spawn(async move { let _ = conn.await; });
    println!("connected : tokio-postgres");

    // 5. DDL — PostgreSQL `SERIAL` becomes SQLite `INTEGER PRIMARY KEY
    //    AUTOINCREMENT` behind the scenes.
    client
        .execute("CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT, age INTEGER)", &[])
        .await?;
    println!("[ok] CREATE TABLE users (id SERIAL PRIMARY KEY, ...)");
    println!("     -> translated to SQLite AUTOINCREMENT DDL");

    // 6. DML — one INSERT per row; the proxy tracks the generated ids.
    for (name, age) in [("Alice", 30), ("Bob", 25), ("Carol", 41)] {
        client
            .execute("INSERT INTO users (name, age) VALUES ($1, $2)", &[&name, &age])
            .await?;
    }
    println!("[ok] INSERT 3 rows via $n placeholders (rewritten to literals)");

    // 7. Read back through the wire in binary format.
    let rows = client
        .query("SELECT id, name, age FROM users ORDER BY id", &[])
        .await?;
    println!("[ok] SELECT id, name, age FROM users ORDER BY id");
    for row in &rows {
        let id: i32 = row.get(0);
        let name: &str = row.get(1);
        let age: i32 = row.get(2);
        println!("     {id:>2} | {name:<6} | {age}");
    }

    // 8. Cross-dialect upsert: PostgreSQL `ON CONFLICT` rewrites to the
    //    SQLite-native same statement.
    client
        .execute(
            "INSERT INTO users (id, name, age) VALUES (1, 'Alicia', 31)
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, age = EXCLUDED.age",
            &[],
        )
        .await?;
    let updated: Option<(String, i32)> = client
        .query_opt("SELECT name, age FROM users WHERE id = 1", &[])
        .await?
        .map(|r| (r.get(0), r.get(1)));
    println!("[ok] upsert id=1 -> {updated:?}");

    // 9. Aggregate: SQLite reports BigInt for count(*); the proxy maps it to
    //    the INT8 wire type so the client can read it as i64.
    let cnt: i64 = client
        .query_one("SELECT count(*) FROM users", &[])
        .await?
        .get(0);
    println!("[ok] count(*) = {cnt}");

    let _ = std::fs::remove_file(&db);
    println!("\ndemo complete.");
    Ok(())
}
