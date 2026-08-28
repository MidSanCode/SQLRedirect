# SQLRedirect

一个数据库兼容代理，让 PostgreSQL 或 MySQL 客户端无需修改即可连接到一个
不同方言的后端（PostgreSQL、MySQL 或 SQLite）。

SQLRedirect 接受一种数据库引擎的线协议，使用
[`sqlparser`](https://crates.io/crates/sqlparser) 解析 SQL，重写为目标方言，
然后通过 [`sqlx`](https://crates.io/crates/sqlx) 在真实后端上执行。

## 特性

- PostgreSQL 前端 → PostgreSQL / MySQL / SQLite 后端。
- MySQL 前端 → PostgreSQL / MySQL / SQLite 后端。
- 单二进制，一个配置文件，支持多个监听器。
- 跨方言追踪 `LAST_INSERT_ID()`（包括 SQLite，sqlx 的 `Any` 层会丢弃
  rowid，这里通过直接查询恢复）。
- 桥接常见的类型和语法差异（`SERIAL` → `INTEGER PRIMARY KEY AUTOINCREMENT`、
  `ILIKE` → `LOWER(...) LIKE LOWER(...)`、MySQL
  `ON DUPLICATE KEY UPDATE` → SQLite `ON CONFLICT` 等）。

## 快速上手

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

将客户端指向监听地址，SQLRedirect 就会解析、重写并转发每条查询到配置的后端。

## 配置

`Config` 是一个 TOML 文件，包含一个或多个 `[[listeners]]` 条目：

| 字段                     | 描述                                                   |
|--------------------------|--------------------------------------------------------|
| `protocol`               | 前端线协议：`postgres` 或 `mysql`。                    |
| `addr`                   | TCP 绑定地址，如 `127.0.0.1:5439`。                    |
| `backend`                | 目标后端 URL：`postgres://`、`mysql://`、`sqlite://`。 |
| `username` / `password`  | 客户端连接所需的可选凭据。                            |
| `sqlite_busy_timeout_ms` | 可选的 SQLite busy timeout（毫秒）。                   |
| `max_connections`        | 连接池最大后端连接数（默认 `128`）。                   |

## 后端 URL 示例

- `postgres://user:pass@host:5432/db`
- `mysql://user:pass@host:3306/db`
- `sqlite://./local.db?mode=rwc`（相对路径）
- `sqlite:///C:/data/app.db?mode=rwc`（Windows 绝对路径）

## 构建

需要较新的 stable Rust 工具链（项目使用 `sqlx 0.8`，兼容 Rust 1.80+）。

```bash
cargo build --release
```

构建产物位于 `target/release/sqlredirect`。

## 许可证

MPL-2.0 — 详见 [`LICENSE`](LICENSE)。
