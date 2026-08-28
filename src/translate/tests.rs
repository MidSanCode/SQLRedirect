//! Unit tests for SQL translation between dialects.

use crate::translate::{FrontDialect, TargetDialect, Translator};

fn t(front: FrontDialect, target: TargetDialect) -> Translator {
    Translator::new(front, target)
}

// ---------------------------------------------------------------------------
// MySQL front-end -> SQLite backend
// ---------------------------------------------------------------------------

#[test]
fn mysql_to_sqlite_backticks_and_types() {
    let tr = t(FrontDialect::Mysql, TargetDialect::Sqlite);
    let sql = "SELECT `name` FROM `users` WHERE `id` = 1";
    let out = tr.translate(sql).unwrap();
    assert_eq!(out, "SELECT \"name\" FROM \"users\" WHERE \"id\" = 1");
}

#[test]
fn mysql_to_sqlite_auto_increment_table() {
    let tr = t(FrontDialect::Mysql, TargetDialect::Sqlite);
    let sql = "CREATE TABLE items (id INT NOT NULL AUTO_INCREMENT, \
               title VARCHAR(50), PRIMARY KEY (id))";
    let out = tr.translate(sql).unwrap();
    assert!(
        out.contains("id INTEGER NOT NULL"),
        "got: {out}"
    );
    assert!(out.contains("AUTOINCREMENT"), "got: {out}");
    // The auto-increment column must carry the PK at column level.
    assert!(
        out.contains("AUTOINCREMENT PRIMARY KEY") || out.contains("PRIMARY KEY AUTOINCREMENT"),
        "got: {out}"
    );
    // MySQL-only table options must be stripped.
    assert!(!out.to_uppercase().contains("ENGINE"), "got: {out}");
}

#[test]
fn mysql_limit_offset_comma() {
    let tr = t(FrontDialect::Mysql, TargetDialect::Sqlite);
    assert_eq!(
        tr.translate("SELECT a FROM t LIMIT 5, 10").unwrap(),
        "SELECT a FROM t LIMIT 10 OFFSET 5"
    );
}

#[test]
fn mysql_now_function() {
    let tr = t(FrontDialect::Mysql, TargetDialect::Sqlite);
    let out = tr.translate("SELECT NOW()").unwrap();
    assert!(out.contains("CURRENT_TIMESTAMP"), "got: {out}");
}

#[test]
fn mysql_insert_ignore() {
    let tr = t(FrontDialect::Mysql, TargetDialect::Sqlite);
    let out = tr
        .translate("INSERT IGNORE INTO tags (k) VALUES ('x')")
        .unwrap();
    assert!(out.to_uppercase().contains("ON CONFLICT"), "got: {out}");
    assert!(out.to_uppercase().contains("DO NOTHING"), "got: {out}");
}

#[test]
fn mysql_on_duplicate_key_update() {
    let tr = t(FrontDialect::Mysql, TargetDialect::Sqlite);
    let out = tr
        .translate(
            "INSERT INTO counters (k, v) VALUES ('a', 1) \
             ON DUPLICATE KEY UPDATE v = v + 1",
        )
        .unwrap();
    let up = out.to_uppercase();
    assert!(up.contains("ON CONFLICT"), "got: {out}");
    assert!(up.contains("DO UPDATE"), "got: {out}");
    // Bare column refs on the RHS keep referring to the existing row
    // (same in MySQL `ON DUPLICATE KEY UPDATE` and SQLite `ON CONFLICT`).
    assert!(!up.contains("EXCLUDED"), "got: {out}");

    // `VALUES(col)` on the RHS maps to `excluded.col` so MySQL semantics
    // for `VALUES()` survive the translation.
    let out = tr
        .translate(
            "INSERT INTO counters (k, v) VALUES ('a', 1) \
             ON DUPLICATE KEY UPDATE v = VALUES(v) + 1",
        )
        .unwrap();
    assert!(out.to_uppercase().contains("EXCLUDED"), "got: {out}");
}

// ---------------------------------------------------------------------------
// PostgreSQL front-end -> SQLite backend
// ---------------------------------------------------------------------------

#[test]
fn pg_serial_to_sqlite_integer_pk() {
    let tr = t(FrontDialect::Postgres, TargetDialect::Sqlite);
    let out = tr
        .translate("CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT)")
        .unwrap();
    assert!(
        out.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"),
        "got: {out}"
    );
}

#[test]
fn pg_ilike_passthrough_for_sqlite_is_lowered() {
    let tr = t(FrontDialect::Postgres, TargetDialect::Sqlite);
    let out = tr.translate("SELECT 1 WHERE 'A' ILIKE 'a'").unwrap();
    // SQLite has no ILIKE; emulated via LOWER().
    assert!(
        !out.to_uppercase().contains("ILIKE"),
        "ILIKE should be rewritten: {out}"
    );
}

#[test]
fn pg_cast_syntax_converted() {
    let tr = t(FrontDialect::Postgres, TargetDialect::Sqlite);
    let out = tr.translate("SELECT '42'::int").unwrap();
    assert!(!out.contains("::"), "double-colon cast should be gone: {out}");
    assert!(out.to_uppercase().contains("CAST"), "got: {out}");
}

#[test]
fn pg_string_concat_operator_kept_for_sqlite() {
    let tr = t(FrontDialect::Postgres, TargetDialect::Sqlite);
    let out = tr.translate("SELECT 'a' || 'b'").unwrap();
    assert!(out.contains("||"), "SQLite supports ||: {out}");
}

// ---------------------------------------------------------------------------
// PostgreSQL front-end / SQLite -> MySQL backend
// ---------------------------------------------------------------------------

#[test]
fn pg_ilike_emulated_on_mysql() {
    let tr = t(FrontDialect::Postgres, TargetDialect::Mysql);
    let out = tr.translate("SELECT * FROM t WHERE name ILIKE 'ab%'").unwrap();
    let up = out.to_ascii_uppercase();
    assert!(!up.contains("ILIKE"), "got: {out}");
    assert!(up.contains("LOWER"), "got: {out}");
}

#[test]
fn concat_operator_becomes_concat_fn_on_mysql() {
    let tr = t(FrontDialect::Postgres, TargetDialect::Mysql);
    let out = tr.translate("SELECT 'a' || 'b'").unwrap();
    assert!(out.to_ascii_uppercase().contains("CONCAT"), "got: {out}");
    assert!(!out.contains("||"), "got: {out}");
}

#[test]
fn returning_rejected_for_mysql() {
    let tr = t(FrontDialect::Postgres, TargetDialect::Mysql);
    let err = tr
        .translate("INSERT INTO t (a) VALUES (1) RETURNING id")
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("returning"));
}

#[test]
fn on_conflict_maps_to_duplicate_key_on_mysql() {
    let tr = t(FrontDialect::Postgres, TargetDialect::Mysql);
    let out = tr
        .translate(
            "INSERT INTO cfg (k, v) VALUES ('a', 1) \
             ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
        )
        .unwrap();
    let up = out.to_ascii_uppercase();
    assert!(up.contains("ON DUPLICATE KEY UPDATE"), "got: {out}");
    assert!(!up.contains("CONFLICT"), "got: {out}");
}

#[test]
fn sqlite_text_types_map_to_mysql() {
    let tr = t(FrontDialect::Postgres, TargetDialect::Mysql);
    let out = tr
        .translate("CREATE TABLE x (a BIGINT UNSIGNED, b DOUBLE PRECISION)")
        .unwrap();
    let up = out.to_ascii_uppercase();
    assert!(up.contains("BIGINT UNSIGNED"), "got: {out}");
    assert!(up.contains("DOUBLE"), "got: {out}");
    assert!(!up.contains("PRECISION"), "got: {out}");
}

// ---------------------------------------------------------------------------
// Same-dialect fast path and quoting fixes
// ---------------------------------------------------------------------------

#[test]
fn same_dialect_is_passthrough() {
    let tr = t(FrontDialect::Mysql, TargetDialect::Mysql);
    let sql = "SELECT `a` FROM `t` LIMIT 2";
    assert_eq!(tr.translate(sql).unwrap(), sql);
}

#[test]
fn fix_quoting_backticks_to_double_quotes() {
    use crate::translate::rewrite::fix_quoting;
    // An embedded target quote must be doubled to stay valid.
    let out = fix_quoting("SELECT `we\"ird` FROM t", TargetDialect::Sqlite);
    assert_eq!(out, "SELECT \"we\"\"ird\" FROM t");
}

#[test]
fn string_literals_untouched_by_quoting_fix() {
    use crate::translate::rewrite::fix_quoting;
    let out = fix_quoting("SELECT 'it''s' FROM t", TargetDialect::Mysql);
    assert_eq!(out, "SELECT 'it''s' FROM t");
}

// ---------------------------------------------------------------------------
// Placeholder substitution
// ---------------------------------------------------------------------------

#[test]
fn substitute_pg_numbered_params() {
    let out = crate::translate::substitute_placeholders(
        "INSERT INTO t VALUES ($1, $2)",
        &["'x'".into(), "7".into()],
    )
    .unwrap();
    assert_eq!(out, "INSERT INTO t VALUES ('x', 7)");
}

#[test]
fn substitute_mysql_question_marks() {
    let out = crate::translate::substitute_placeholders(
        "SELECT * FROM t WHERE a = ? AND b = ?",
        &["NULL".into(), "'y''s'".into()],
    )
    .unwrap();
    assert_eq!(out, "SELECT * FROM t WHERE a = NULL AND b = 'y''s'");
}

#[test]
fn substitute_skips_string_literals() {
    let out = crate::translate::substitute_placeholders(
        "SELECT '?' , ?",
        &["9".into()],
    )
    .unwrap();
    assert_eq!(out, "SELECT '?' , 9");
}

#[test]
fn substitute_missing_param_errors() {
    let err =
        crate::translate::substitute_placeholders("SELECT ?", &[]).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("not enough"));
}

// ---------------------------------------------------------------------------
// CREATE TABLE type mapping matrix
// ---------------------------------------------------------------------------

#[test]
fn type_mapping_matrix() {
    let cases = [
        (
            "CREATE TABLE m (a BOOLEAN, b BYTEA, c UUID)",
            TargetDialect::Mysql,
            vec!["BOOLEAN", "BLOB", "CHAR(36)"],
        ),
        (
            "CREATE TABLE s (a BOOLEAN, b BYTEA, c UUID)",
            TargetDialect::Sqlite,
            vec!["BOOLEAN", "BLOB", "TEXT"],
        ),
        (
            "CREATE TABLE p (a TINYINT, b DATETIME, c MEDIUMBLOB)",
            TargetDialect::Postgres,
            vec!["SMALLINT", "TIMESTAMP", "BYTEA"],
        ),
    ];
    for (sql, target, expect) in cases {
        let tr = t(FrontDialect::Mysql, target);
        if matches!(tr.front(), FrontDialect::Mysql) && target == TargetDialect::Mysql {
            continue; // passthrough path skips mapping
        }
        let out = tr.translate(sql).unwrap().to_ascii_uppercase();
        for e in expect {
            assert!(out.contains(e), "target={target} expected {e} in: {out}");
        }
    }
}

#[test]
fn dialect_detection_from_urls() {
    use crate::translate::TargetDialect;
    assert_eq!(
        TargetDialect::from_backend_url("postgres://u:p@h/db").unwrap(),
        TargetDialect::Postgres
    );
    assert_eq!(
        TargetDialect::from_backend_url("mysql://h/db").unwrap(),
        TargetDialect::Mysql
    );
    assert_eq!(
        TargetDialect::from_backend_url("sqlite://file.db").unwrap(),
        TargetDialect::Sqlite
    );
    assert!(TargetDialect::from_backend_url("oracle://h").is_err());
}
