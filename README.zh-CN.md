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

两种监听器的现成配置模板见 [`config.example.toml`](config.example.toml)。

## 用例演示

运行内置的端到端演示——它在进程内启动代理（后端为一个临时 SQLite 文件），
并用真实的 PostgreSQL 客户端库连接：

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

演示覆盖的场景：

| 客户端发送（PostgreSQL）                 | 代理的处理                             |
|------------------------------------------|----------------------------------------|
| `CREATE TABLE ... id SERIAL PRIMARY KEY` | 翻译为 SQLite 的 AUTOINCREMENT DDL     |
| `INSERT ... VALUES ($1, $2)`             | 占位符替换为带类型的字面量             |
| `SELECT id, name, age ...`               | 结果行以二进制线协议编码               |
| `ON CONFLICT (id) DO UPDATE`             | 以 SQLite 原生语法透传                 |
| `SELECT count(*)`                        | BigInt 映射为 INT8 线类型              |

同样的流程也适用于 `psql` 连接 `postgres` 监听器，或任意 MySQL 客户端
（`mysql`、`mariadb`、DBeaver 等）连接 `mysql` 监听器。

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
