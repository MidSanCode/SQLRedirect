//! Shared helpers for wire-level integration tests.

use std::path::PathBuf;
use std::sync::Arc;

use sqlredirect::backend::Backend;
use sqlredirect::server::mysql::MySqlHandler;
use sqlredirect::server::pg::PgHandlers;
use sqlredirect::translate::FrontDialect;

pub const TEST_USER: &str = "sqr";
pub const TEST_PASS: &str = "secret";

/// A fresh SQLite database file under the OS temp dir.
pub fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    let unique = format!(
        "sqlredirect-it-{}-{}-{}.db",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    dir.join(unique)
}

/// Install sqlx drivers (idempotent).
pub fn init_drivers() {
    sqlx::any::install_default_drivers();
}

/// SQLite backend URL that creates the file if missing.
///
/// sqlx parses `sqlite://` URLs with the `//` as an authority separator, so an
/// absolute Windows path (`C:\...`) must be passed with forward slashes and a
/// leading slash (`sqlite:///C:/...`) to avoid being mistaken for a host.
pub fn sqlite_url(db: &PathBuf) -> String {
    let mut p = db.display().to_string().replace('\\', "/");
    if !p.starts_with('/') {
        p = format!("/{p}");
    }
    format!("sqlite://{p}?mode=rwc")
}

/// Start a PostgreSQL-protocol listener backed by `db`, returning its port.
pub async fn start_pg(db: &PathBuf) -> u16 {
    let backend = Backend::connect(&sqlite_url(db), 8)
        .await
        .expect("backend connect");
    let handler = Arc::new(
        PgHandlers::new(
            backend,
            FrontDialect::Postgres,
            Some(TEST_USER.to_string()),
            Some(TEST_PASS.to_string()),
        )
        .expect("pg handlers"),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = handler.serve_on(listener).await;
    });
    port
}

/// Start a MySQL-protocol listener backed by `db`, returning its port.
///
/// mysql-mimic binds internally, so we discover a free port by binding and
/// releasing a probe socket (small race window, fine for tests).
pub async fn start_mysql(db: &PathBuf) -> u16 {
    for _ in 0..5 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let backend = Backend::connect(&sqlite_url(db), 8)
            .await
            .expect("backend connect");
        let handler = Arc::new(MySqlHandler::new(
            backend,
            FrontDialect::Mysql,
            Some(TEST_USER.to_string()),
            Some(TEST_PASS.to_string()),
        ));
        tokio::spawn(async move {
            let _ = handler
                .serve(format!("127.0.0.1:{port}").parse().unwrap())
                .await;
        });

        // Wait until something accepts on that port.
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return port;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    panic!("could not start mysql listener");
}

/// Connect with the tokio-postgres client.
pub async fn pg_client(port: u16) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(
        &format!(
            "host=127.0.0.1 port={port} user={TEST_USER} password={TEST_PASS}"
        ),
        tokio_postgres::NoTls,
    )
    .await
    .expect("pg connect");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// Connect with mysql_async.
pub async fn my_conn(port: u16) -> mysql_async::Conn {
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some(TEST_USER))
        .pass(Some(TEST_PASS));
    mysql_async::Conn::new(opts).await.expect("mysql connect")
}
