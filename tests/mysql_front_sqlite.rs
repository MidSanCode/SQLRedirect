//! Integration test: MySQL wire clients talking to a SQLite backend through
//! the proxy (no changes to the client application code).

mod common;

use common::*;
use mysql_async::prelude::*;

#[tokio::test]
async fn mysql_client_over_sqlite_backend() {
    init_drivers();
    let db = temp_db("my");
    let port = start_mysql(&db).await;
    let mut conn = my_conn(port).await;

    // DDL: MySQL syntax with AUTO_INCREMENT.
    conn.query_drop(
        "CREATE TABLE items (
            id INT NOT NULL AUTO_INCREMENT,
            title VARCHAR(50),
            price DOUBLE,
            PRIMARY KEY (id)
        )",
    )
    .await
    .expect("create table");

    // INSERT with `?` placeholders (binary protocol, interpolated upstream).
    for title in ["pen", "book", "lamp"] {
        conn.exec_drop("INSERT INTO items (title, price) VALUES (?, ?)", (title, 9.99f64))
            .await
            .expect("insert");
    }

    // SELECT round-trip.
    let selected: Vec<(i64, String, f64)> = conn
        .exec("SELECT id, title, price FROM items ORDER BY id", ())
        .await
        .expect("select");
    assert_eq!(selected.len(), 3);
    assert_eq!(selected[0], (1, "pen".into(), 9.99));
    assert_eq!(selected[2].1, "lamp");

    // Proxy-tracked LAST_INSERT_ID().
    let last: Option<(i64,)> = conn
        .exec_first("SELECT LAST_INSERT_ID()", ())
        .await
        .expect("last_insert_id");
    assert_eq!(last.map(|r| r.0), Some(3));

    // UPDATE with backtick identifiers.
    conn.exec_drop("UPDATE `items` SET `price` = ? WHERE `title` = ?", (12.5f64, "book"))
        .await
        .unwrap();
    let rows: Vec<(f64,)> = conn
        .exec("SELECT price FROM items WHERE title = 'book'", ())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, 12.5);

    // MySQL LIMIT a,b form.
    let rows: Vec<(String,)> = conn
        .exec("SELECT title FROM items ORDER BY id LIMIT 1, 2", ())
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "book");

    // CONCAT function.
    let rows: Vec<(String,)> = conn.exec("SELECT CONCAT(title, '!') FROM items WHERE id = 1", ()).await.unwrap();
    assert_eq!(rows[0].0, "pen!");

    // DELETE.
    conn.exec_drop("DELETE FROM items WHERE id = ?", (3u32,))
        .await
        .unwrap();
    let cnt: Option<(i64,)> = conn.exec_first("SELECT COUNT(*) FROM items", ()).await.unwrap();
    assert_eq!(cnt.map(|r| r.0), Some(2));
}

#[tokio::test]
async fn mysql_show_and_describe() {
    init_drivers();
    let db = temp_db("myshow");
    let port = start_mysql(&db).await;
    let mut conn = my_conn(port).await;

    conn.query_drop("CREATE TABLE pets (id INT PRIMARY KEY, name VARCHAR(20))")
        .await
        .unwrap();

    // SHOW TABLES against the sqlite catalog.
    let tables: Vec<(String,)> = match conn.query("SHOW TABLES").await {
        Ok(rows) => rows,
        Err(e) => panic!("SHOW TABLES failed: {e}"),
    };
    assert!(
        tables.iter().any(|(n,)| n == "pets"),
        "pets not listed in: {tables:?}"
    );

    // DESCRIBE pets returns SHOW COLUMNS shape: (Field, Type, Null, Key, Default, Extra).
    let cols: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        String,
    )> = conn.query("DESCRIBE pets").await.expect("describe");
    let names: Vec<&str> = cols.iter().map(|(f, ..)| f.as_str()).collect();
    assert!(names.contains(&"id"), "{names:?}");
    assert!(names.contains(&"name"), "{names:?}");

    // SHOW CREATE TABLE returns some DDL text containing the table name.
    let ddl: Vec<(Option<String>, Option<String>)> =
        conn.query("SHOW CREATE TABLE pets").await.expect("sct");
    let body = ddl[0].1.clone().unwrap_or_default().to_uppercase();
    assert!(body.contains("PETS"), "{body}");

    // Session commands are accepted.
    conn.query_drop("SET NAMES utf8mb4").await.unwrap_or_default();
    let dbs: Vec<(String,)> = conn.query("SHOW DATABASES").await.expect("show databases");
    assert!(!dbs.is_empty());
}

#[tokio::test]
async fn mysql_upsert_maps_to_on_conflict() {
    init_drivers();
    let db = temp_db("myupsert");
    let port = start_mysql(&db).await;
    let mut conn = my_conn(port).await;

    conn.query_drop("CREATE TABLE cfg (k VARCHAR(20) PRIMARY KEY, v INT)")
        .await
        .unwrap();

    conn.query_drop("INSERT INTO cfg (k, v) VALUES ('a', 1)").await.unwrap();
    // ON DUPLICATE KEY UPDATE arithmetic must survive translation to SQLite
    // as excluded.v references.
    conn.query_drop("INSERT INTO cfg (k, v) VALUES ('a', 5) ON DUPLICATE KEY UPDATE v = v + 10")
        .await
        .unwrap();
    let rows: Vec<(i64,)> = conn.exec("SELECT v FROM cfg WHERE k = 'a'", ()).await.unwrap();
    assert_eq!(rows, vec![(11,)]);

    // INSERT IGNORE becomes ON CONFLICT DO NOTHING.
    conn.query_drop("INSERT IGNORE INTO cfg (k, v) VALUES ('a', 99)").await.unwrap();
    let rows: Vec<(i64,)> = conn.exec("SELECT v FROM cfg WHERE k = 'a'", ()).await.unwrap();
    assert_eq!(rows, vec![(11,)], "ignored insert must not overwrite");
}
