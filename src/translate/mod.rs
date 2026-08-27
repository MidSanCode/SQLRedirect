//! SQL translation layer: parse with the source (front-end) dialect, rewrite
//! dialect-specific constructs, and emit SQL understood by the target backend.

pub mod rewrite;
pub mod types;

#[cfg(test)]
mod tests;

use std::fmt;

use sqlparser::ast::{VisitMut, Expr, Statement};
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;

use crate::error::{Error, Result};

/// Dialect spoken by the client application (the front-end we simulate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontDialect {
    Postgres,
    Mysql,
}

impl fmt::Display for FrontDialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrontDialect::Postgres => f.write_str("postgres"),
            FrontDialect::Mysql => f.write_str("mysql"),
        }
    }
}

/// Dialect of the real target database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDialect {
    Postgres,
    Mysql,
    Sqlite,
}

impl fmt::Display for TargetDialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetDialect::Postgres => f.write_str("postgres"),
            TargetDialect::Mysql => f.write_str("mysql"),
            TargetDialect::Sqlite => f.write_str("sqlite"),
        }
    }
}

impl TargetDialect {
    /// Detect the target dialect from a backend connection URL.
    pub fn from_backend_url(url: &str) -> Result<Self> {
        let scheme = url.split(':').next().unwrap_or("");
        match scheme {
            "postgres" | "postgresql" => Ok(TargetDialect::Postgres),
            "mysql" | "mariadb" => Ok(TargetDialect::Mysql),
            "sqlite" => Ok(TargetDialect::Sqlite),
            other => Err(Error::Config(format!(
                "unsupported backend URL scheme '{other}' (expected postgres://, mysql:// or sqlite://)"
            ))),
        }
    }
}

/// Translates SQL between a front-end dialect and a target dialect.
#[derive(Debug, Clone)]
pub struct Translator {
    front: FrontDialect,
    target: TargetDialect,
}

impl Translator {
    pub fn new(front: FrontDialect, target: TargetDialect) -> Self {
        Translator { front, target }
    }

    pub fn front(&self) -> FrontDialect {
        self.front
    }

    pub fn target(&self) -> TargetDialect {
        self.target
    }

    fn parse_dialect(&self) -> Box<dyn Dialect> {
        match self.front {
            FrontDialect::Postgres => Box::new(PostgreSqlDialect {}),
            FrontDialect::Mysql => Box::new(MySqlDialect {}),
        }
    }

    /// Translate a single already-parsed statement for the target dialect.
    pub fn translate_statement(&self, stmt: &Statement) -> Result<String> {
        let mut cloned = stmt.clone();
        rewrite::rewrite_statement(&mut cloned, self.target)?;
        let _ = cloned.visit(&mut rewrite::ExprRewriter {
            target: self.target,
        });
        Ok(rewrite::fix_quoting(&cloned.to_string(), self.target))
    }

    /// Translate a single SQL statement string (no trailing params).
    pub fn translate(&self, sql: &str) -> Result<String> {
        if self.front_dialect_matches_target() {
            return Ok(sql.to_string());
        }

        let dialect = self.parse_dialect();
        let mut stmts = Parser::parse_sql(&*dialect, sql).map_err(|e| {
            Error::Parse(format!("{e} (sql: {})", truncate(sql)))
        })?;

        for stmt in stmts.iter_mut() {
            rewrite::rewrite_statement(stmt, self.target)?;
        }

        let _ = stmts.visit(&mut rewrite::ExprRewriter {
            target: self.target,
        });

        let joined = stmts
            .iter()
            .map(|s: &Statement| s.to_string())
            .collect::<Vec<_>>()
            .join("; ");

        Ok(rewrite::fix_quoting(&joined, self.target))
    }

    /// True when no translation is needed (front == target dialect).
    pub(crate) fn front_dialect_matches_target(&self) -> bool {
        matches!(
            (self.front, self.target),
            (FrontDialect::Postgres, TargetDialect::Postgres)
                | (FrontDialect::Mysql, TargetDialect::Mysql)
        )
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() > 400 {
        format!("{}...", s.chars().take(400).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Replace SQL placeholders (`?` for MySQL front, `$n` for PG front) with
/// concrete literal expressions supplied by the caller.
///
/// Values are supplied in source-dialect SQL-literal form (already escaped by
/// the wire handler).
pub fn substitute_placeholders(sql: &str, literals: &[String]) -> Result<String> {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut positional = 0usize;

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                out.push('\'');
                // consume single-quoted string literal ('' escapes)
                while let Some(n) = chars.next() {
                    out.push(n);
                    if n == '\'' {
                        if chars.peek() == Some(&'\'') {
                            out.push(chars.next().unwrap());
                        } else {
                            break;
                        }
                    }
                }
            }
            '$' => {
                // PostgreSQL style $1..$n
                let mut num = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        num.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if num.is_empty() {
                    out.push('$');
                } else {
                    let idx = num.parse::<usize>().unwrap_or(1) - 1;
                    out.push_str(&literal_at(literals, idx)?);
                    positional += 1;
                }
            }
            '?' => {
                out.push_str(&literal_at(literals, positional)?);
                positional += 1;
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn literal_at(literals: &[String], idx: usize) -> Result<String> {
    match literals.get(idx) {
        Some(lit) => Ok(lit.clone()),
        None => Err(Error::Translate(format!(
            "not enough bind parameters: {idx} requested, {} provided",
            literals.len()
        ))),
    }
}

/// Render a literal value (supplied in source dialect) for the target dialect.
/// Escapes single quotes for string literals.
pub fn quote_string_literal(value: &str, target: TargetDialect) -> String {
    let _ = target;
    format!("'{}'", value.replace('\'', "''"))
}

#[allow(unused)]
fn _unused_import_guard(_e: Expr) {}
