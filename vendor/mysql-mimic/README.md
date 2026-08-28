# mysql-mimic-rust

A Rust library that implements the MySQL wire protocol server-side, allowing applications to act as a MySQL server. Inspired by the Python [mysql-mimic](https://github.com/barakalon/mysql-mimic) library.

## Features

- **Full MySQL wire protocol handshake** — clients connect using standard MySQL tools
- **Query handling via trait** — implement the `Session` trait to define custom query logic
- **Result set construction** — build typed column definitions and row data
- **Prepared statements** — full COM_STMT_PREPARE / EXECUTE / CLOSE / RESET / SEND_LONG_DATA lifecycle
- **Session variables** — MySQL-compatible `@@variable` system with defaults (version, charset, sql_mode, etc.)
- **Built-in middleware** — auto-handles SET, USE, SHOW VARIABLES, SELECT @@, BEGIN/COMMIT/ROLLBACK, static SELECTs
- **Multi-statement support** — semicolon-separated queries with proper quote handling
- **Pluggable authentication** — `IdentityProvider` trait with `mysql_native_password` support
- **Connection management** — unique connection IDs, per-connection state, connect attributes
- **Async/await** — built on Tokio for high-performance async I/O
- **Full COM_* command coverage** — QUERY, PING, INIT_DB, FIELD_LIST, CHANGE_USER, RESET_CONNECTION, SET_OPTION, DEBUG, and all STMT_* commands

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
mysql-mimic = { path = "." }
tokio = { version = "1", features = ["full"] }
```

### Example Server

```rust
use mysql_mimic::{MysqlServer, Session, SessionFactory, ResultSet, Column, ColumnType};
use mysql_mimic::error::MysqlError;

struct MySession;

impl Session for MySession {
    async fn handle_query(&mut self, query: &str) -> Result<ResultSet, MysqlError> {
        let mut rs = ResultSet::new(vec![
            Column::new("greeting", ColumnType::VarString),
        ]);
        rs.add_row(vec![Some("Hello from mysql-mimic!".into())]);
        Ok(rs)
    }
}

struct MyFactory;

impl SessionFactory for MyFactory {
    type S = MySession;
    async fn create_session(&self) -> Result<MySession, MysqlError> {
        Ok(MySession)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = MysqlServer::new(MyFactory);
    server.listen("127.0.0.1:3307").await?;
    Ok(())
}
```

### With Authentication

```rust
use mysql_mimic::identity::{StaticIdentityProvider, User};
use mysql_mimic::server::MysqlServer;

let users = vec![User::with_password("admin", "secret")];
let identity = StaticIdentityProvider::new(users);
let server = MysqlServer::with_identity_provider(MyFactory, identity);
```

### Custom Global Variables

```rust
use mysql_mimic::variables::{GlobalVariables, Value};

let mut globals = GlobalVariables::new();
globals.set("version", Value::String("8.0.0-myapp".into()));
let server = MysqlServer::new(MyFactory).set_global_variables(globals);
```

Connect with any MySQL client:

```bash
mysql -h 127.0.0.1 -P 3307 -u root
```

## Running the Example

```bash
cargo run --example simple_server
```

## Architecture

| Module | Description |
|--------|-------------|
| `connection.rs` | Per-client connection state, command dispatch, middleware |
| `server.rs` | TCP server accepting client connections |
| `session.rs` | `Session` and `SessionFactory` traits |
| `identity.rs` | Pluggable authentication (`IdentityProvider`, `User`) |
| `variables.rs` | Session/global variables system |
| `prepared.rs` | Prepared statement lifecycle |
| `result_set.rs` | Result set and column type definitions |
| `protocol/` | MySQL wire protocol (packets, handshake, auth, constants) |
| `error.rs` | Error types using `thiserror` |

## Session Trait

The `Session` trait provides lifecycle hooks:

| Method | When called |
|--------|-------------|
| `init(&mut self, info: &ConnectionInfo)` | After handshake, before command phase |
| `handle_query(&mut self, query: &str)` | On COM_QUERY / COM_STMT_EXECUTE (after middleware) |
| `handle_init_db(&mut self, database: &str)` | On COM_INIT_DB / USE statement |
| `on_reset(&mut self)` | On COM_RESET_CONNECTION / COM_CHANGE_USER |
| `on_close(&mut self)` | On client disconnect |

## Development

```bash
make build           # Build the library
make test            # Run tests
make lint            # Lint (clippy with warnings as errors)
make fmt-check       # Check formatting
make ci              # Run the same checks used by GitHub CI
cargo run --example simple_server  # Run example server
```

## License

MIT
