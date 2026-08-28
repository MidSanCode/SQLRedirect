//! A simple example MySQL server that responds to queries with hardcoded data.
//!
//! Connect with: `mysql -h 127.0.0.1 -P 3307 -u root`

use mysql_mimic::error::MysqlError;
use mysql_mimic::result_set::{Column, ColumnType, ResultSet};
use mysql_mimic::server::MysqlServer;
use mysql_mimic::session::{Session, SessionFactory};

/// A simple session that returns hardcoded results.
struct SimpleSession;

impl Session for SimpleSession {
    async fn handle_query(&mut self, query: &str) -> Result<ResultSet, MysqlError> {
        let query_lower = query.trim().to_lowercase();

        if query_lower.starts_with("select") {
            // Return a simple result set
            let mut rs = ResultSet::new(vec![
                Column::new("id", ColumnType::Long),
                Column::new("message", ColumnType::VarString),
            ]);
            rs.add_row(vec![
                Some("1".into()),
                Some("Hello from mysql-mimic!".into()),
            ]);
            rs.add_row(vec![Some("2".into()), Some("It works!".into())]);
            Ok(rs)
        } else {
            // For non-SELECT queries, return an empty result
            Ok(ResultSet::empty())
        }
    }
}

/// Factory that creates `SimpleSession` instances.
struct SimpleFactory;

impl SessionFactory for SimpleFactory {
    type S = SimpleSession;

    async fn create_session(&self) -> Result<SimpleSession, MysqlError> {
        Ok(SimpleSession)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let addr = "127.0.0.1:3307";
    println!("Starting MySQL mimic server on {addr}");
    println!("Connect with: mysql -h 127.0.0.1 -P 3307 -u root");

    let server = MysqlServer::new(SimpleFactory);
    server.listen(addr).await?;

    Ok(())
}
