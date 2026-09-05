# SQLRedirect

A database compatibility proxy that lets a PostgreSQL or MySQL client talk to a
different backend dialect (PostgreSQL, MySQL, or SQLite) without changing the
client.

SQLRedirect accepts the wire protocol of one database engine, parses the SQL
with [`sqlparser`](https://crates.io/crates/sqlparser), rewrites it for the
target dialect, and executes it against a real backend over
[`sqlx`](https://crates.io/crates/sqlx).

## Features

- PostgreSQL front-end → PostgreSQL / MySQL / SQLite backends.
- MySQL front-end → PostgreSQL / MySQL / SQLite backends.
- Single binary, one config file, multiple listeners.
- Tracks `LAST_INSERT_ID()` across dialects (including SQLite, where the
  rowid is otherwise dropped by the sqlx `Any` layer).
- Bridges common type and syntax differences (`SERIAL` → `INTEGER PRIMARY KEY
  AUTOINCREMENT`, `ILIKE` → `LOWER(...) LIKE LOWER(...)`, MySQL
  `ON DUPLICATE KEY UPDATE` → SQLite `ON CONFLICT`, etc.).

## Quick start

```toml
# config.toml
[[listeners]]
protocol = "postgres"
addr     = "127.0.0.1:5439"
backend  = "sqlite://./data.db?mode=rwc"
username = "demo"
password = "demo"

[[listeners]]
protocol = "mysql"
addr     = "127.0.0.1:3307"
backend  = "mysql://app:app@127.0.0.1:3306/myapp"
```

```bash
cargo run --release -- -c config.toml
```

Point your client at the listener address. SQLRedirect parses, rewrites, and
forwards each query to the configured backend.

A ready-to-edit template with both listener kinds is provided in
[`config.example.toml`](config.example.toml).

## Demo

Run the bundled end-to-end demo — it starts the proxy in-process against a
throwaway SQLite file and connects with a real PostgreSQL client library:

```bash
cargo run --example demo
```

```text
backend   : sqlite:///.../sqlredirect-demo.db?mode=rwc
dialect   : Sqlite
listening : postgres://demo:demo@127.0.0.1:56663

connected : tokio-postgres
[ok] CREATE TABLE users (id SERIAL PRIMARY KEY, ...)
     -> translated to SQLite AUTOINCREMENT DDL
[ok] INSERT 3 rows via $n placeholders (rewritten to literals)
[ok] SELECT id, name, age FROM users ORDER BY id
      1 | Alice  | 30
      2 | Bob    | 25
      3 | Carol  | 41
[ok] upsert id=1 -> Some(("Alicia", 31))
[ok] count(*) = 3

demo complete.
```

What the demo exercises:

| Client sends (PostgreSQL)                       | Proxy does                                     |
|-------------------------------------------------|------------------------------------------------|
| `CREATE TABLE ... id SERIAL PRIMARY KEY`        | `INTEGER PRIMARY KEY AUTOINCREMENT` on SQLite  |
| `INSERT ... VALUES ($1, $2)`                    | placeholders become typed literals             |
| `SELECT id, name, age ...`                      | rows encoded in the binary wire format         |
| `ON CONFLICT (id) DO UPDATE`                    | passed through in SQLite-native form           |
| `SELECT count(*)`                               | BigInt mapped to the INT8 wire type            |

The same flow works from `psql` against a `postgres` listener, or from any
MySQL client (`mysql`, `mariadb`, DBeaver, ...) against a `mysql` listener.

## Configuration

`Config` is a TOML file containing one or more `[[listeners]]` entries:

| Field                    | Description                                                   |
|--------------------------|---------------------------------------------------------------|
| `protocol`               | Front-end wire protocol: `postgres` or `mysql`.               |
| `addr`                   | TCP address to bind, e.g. `127.0.0.1:5439`.                   |
| `backend`                | Target backend URL: `postgres://`, `mysql://`, or `sqlite://`. |
| `username` / `password`  | Optional credentials required of clients.                     |
| `sqlite_busy_timeout_ms` | Optional SQLite busy timeout in milliseconds.                 |
| `max_connections`        | Max backend connections per pool (default `128`).             |

## Backend URL examples

- `postgres://user:pass@host:5432/db`
- `mysql://user:pass@host:3306/db`
- `sqlite://./local.db?mode=rwc` (relative path)
- `sqlite:///C:/data/app.db?mode=rwc` (Windows absolute path)

## Building

Requires a recent stable Rust toolchain (the project pins `sqlx 0.8`, which is
compatible with Rust 1.80+).

```bash
cargo build --release
```

The resulting binary lives at `target/release/sqlredirect`.

## License

MPL-2.0 — see [`LICENSE`](LICENSE).
