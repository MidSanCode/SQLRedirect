use std::sync::Arc;
use sqlredirect::backend::Backend;
use sqlredirect::server::pg::PgHandlers;
use sqlredirect::translate::FrontDialect;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("sqlredirect=debug").init();
    sqlx::any::install_default_drivers();
    let db = "/tmp/opencode/wire.db";
    let _ = std::fs::remove_file(db);
    let backend = Backend::connect(&format!("sqlite://{db}?mode=rwc"), 4).await?;
    let dialect = backend.dialect();
    let handler = Arc::new(PgHandlers::new(
        backend,
        FrontDialect::Postgres,
        Some("u".into()),
        Some("p".into()),
    )?);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:15439").await?;
    println!("listening");
    handler.serve_on(listener).await?;
    Ok(())
}
