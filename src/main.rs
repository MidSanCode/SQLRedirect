use std::sync::Arc;

use clap::Parser;
use sqlredirect::backend::Backend;
use sqlredirect::config::Config;
use sqlredirect::error::{Error, Result};
use sqlredirect::server::mysql::MySqlHandler;
use sqlredirect::server::pg::PgHandlers;
use sqlredirect::translate::FrontDialect;

#[derive(Parser, Debug)]
#[command(name = "sqlredirect", about = "database compatibility proxy")]
struct Args {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sqlredirect=info".into()),
        )
        .init();

    let args = Args::parse();
    let cfg = Config::load(&args.config)?;

    sqlx::any::install_default_drivers();

    let mut tasks = Vec::new();
    for l in &cfg.listeners {
        let addr = l
            .addr
            .parse()
            .map_err(|e| Error::Config(format!("invalid addr '{}': {e}", l.addr)))?;
        let backend = Backend::connect(&l.backend, l.max_connections).await?;
        let front = match l.protocol.as_str() {
            "postgres" => FrontDialect::Postgres,
            "mysql" => FrontDialect::Mysql,
            other => {
                return Err(Error::Config(format!(
                    "unsupported protocol '{other}'"
                )))
            }
        };

        match l.protocol.as_str() {
            "postgres" => {
                let handler = Arc::new(PgHandlers::new(
                    backend,
                    front,
                    l.username.clone(),
                    l.password.clone(),
                )?);
                tasks.push(tokio::spawn(async move { handler.serve(addr).await }));
            }
            "mysql" => {
                let handler = Arc::new(MySqlHandler::new(
                    backend,
                    front,
                    l.username.clone(),
                    l.password.clone(),
                ));
                tasks.push(tokio::spawn(async move { handler.serve(addr).await }));
            }
            _ => unreachable!(),
        }
    }

    for task in tasks {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(Error::Server(format!("listener task failed: {e}"))),
        }
    }
    Ok(())
}