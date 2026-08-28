//! AST-level statement rewriting plus token-level quoting fixes.

use core::ops::ControlFlow;

use sqlparser::ast::{
    Assignment, AssignmentTarget, ConflictTarget, Expr, Function,
    FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments, Ident, Insert,
    LimitClause, ObjectName, OnConflict, OnConflictAction, OnInsert, Statement,
    Statement as AstStatement, Update, Delete, VisitorMut,
};

use super::TargetDialect;
use crate::error::{Error, Result};

/// Structural rewriting of a single statement for the given target dialect.
pub fn rewrite_statement(stmt: &mut AstStatement, target: TargetDialect) -> Result<()> {
    match stmt {
        Statement::Query(query) => {
            rewrite_query(query, target);
        }
        Statement::Insert(ins) => {
            rewrite_insert(ins, target)?;
        }
        Statement::Update(upd) => {
            rewrite_update(upd, target);
        }
        Statement::Delete(del) => {
            rewrite_delete(del, target);
        }
        Statement::CreateTable(ct) => {
            super::types::rewrite_create_table(ct, target)?;
        }
        _ => {}
    }
    Ok(())
}

fn rewrite_query(query: &mut sqlparser::ast::Query, target: TargetDialect) {
    if let Some(clause) = &mut query.limit_clause {
        *clause = normalize_limit(
            std::mem::replace(
                clause,
                LimitClause::LimitOffset {
                    limit: None,
                    limit_by: vec![],
                    offset: None,
                },
            ),
            target,
        );
    }
}

/// Normalize a `LIMIT`/`OFFSET` clause for the target dialect.
fn normalize_limit(clause: LimitClause, target: TargetDialect) -> LimitClause {
    match clause {
        // MySQL `LIMIT a, b` is equivalent to `LIMIT b OFFSET a` elsewhere.
        LimitClause::OffsetCommaLimit { offset, limit } => {
            if target == TargetDialect::Mysql {
                LimitClause::OffsetCommaLimit { offset, limit }
            } else {
                LimitClause::LimitOffset {
                    limit: Some(limit),
                    limit_by: vec![],
                    offset: Some(sqlparser::ast::Offset {
                        value: offset,
                        rows: sqlparser::ast::OffsetRows::None,
                    }),
                }
            }
        }
        // PostgreSQL `LIMIT ALL` is not valid in MySQL/SQLite; drop the limit.
        LimitClause::LimitOffset { limit, limit_by, offset } => {
            let limit = match limit {
                Some(Expr::Identifier(ident)) if ident.value == "ALL" && target != TargetDialect::Postgres => None,
                other => other,
            };
            LimitClause::LimitOffset { limit, limit_by, offset }
        }
    }
}

fn rewrite_insert(ins: &mut Insert, target: TargetDialect) -> Result<()> {
    if target == TargetDialect::Mysql {
        // PG/SQLite RETURNING is not supported by MySQL.
        if ins.returning.is_some() {
            return Err(Error::Unsupported(
                "RETURNING is not supported by the MySQL backend".to_string(),
            ));
        }
        // PG/SQLite SQLite-style ON CONFLICT -> ON DUPLICATE KEY UPDATE.
        if let Some(OnInsert::OnConflict(oc)) = &ins.on {
            let on_insert = match &oc.action {
                OnConflictAction::DoNothing => {
                    // Emit a no-op `col = col` per conflict column so the row
                    // is kept as-is. Fall back to `id = id` when unknown.
                    let assigns = match &oc.conflict_target {
                        Some(ConflictTarget::Columns(cols)) => cols
                            .iter()
                            .map(|c| Assignment {
                                target: AssignmentTarget::ColumnName(ObjectName::from(c.clone())),
                                value: Expr::Identifier(c.clone()),
                            })
                            .collect::<Vec<_>>(),
                        _ => vec![Assignment {
                            target: AssignmentTarget::ColumnName(ObjectName::from(Ident::new("id"))),
                            value: Expr::Identifier(Ident::new("id")),
                        }],
                    };
                    OnInsert::DuplicateKeyUpdate(assigns)
                }
                OnConflictAction::DoUpdate(du) => {
                    OnInsert::DuplicateKeyUpdate(du.assignments.clone())
                }
            };
            ins.on = Some(on_insert);
        }
        if ins.or.is_some() {
            // SQLite `INSERT OR REPLACE` -> MySQL has no equivalent; error.
            return Err(Error::Unsupported(
                "SQLite ON CONFLICT (`INSERT OR ...`) is not supported by the MySQL backend"
                    .to_string(),
            ));
        }
    } else {
        // Target is Postgres or SQLite.
        if ins.ignore {
            // MySQL INSERT IGNORE -> ON CONFLICT DO NOTHING.
            ins.ignore = false;
            ins.on = Some(OnInsert::OnConflict(OnConflict {
                conflict_target: None,
                action: OnConflictAction::DoNothing,
            }));
        }
        if matches!(ins.on, Some(OnInsert::DuplicateKeyUpdate(_))) {
            if let Some(OnInsert::DuplicateKeyUpdate(assignments)) = ins.on.take() {
                let mut assigns = assignments.clone();
                // Rewrite MySQL `VALUES(col)` references to SQLite `excluded.col`.
                for a in assigns.iter_mut() {
                    a.value = rewrite_values_ref(a.value.clone());
                }
                // MySQL's `ON DUPLICATE KEY UPDATE` fires on *any* unique/PK
                // violation, so we map it to a bare `ON CONFLICT DO UPDATE`
                // without an explicit conflict target: SQLite allows the
                // DO UPDATE action without a target and will use the unique
                // key that triggered the violation automatically.
                ins.on = Some(OnInsert::OnConflict(OnConflict {
                    conflict_target: None,
                    action: OnConflictAction::DoUpdate(sqlparser::ast::DoUpdate {
                        assignments: assigns,
                        selection: None,
                    }),
                }));
            }
        }
        if ins.or.is_some() {
            // `INSERT OR ...` is SQLite-specific; keep for SQLite target, drop for PG.
            if target == TargetDialect::Postgres {
                ins.or = None;
            }
        }
    }
    Ok(())
}

/// Rewrite MySQL `VALUES(col)` function calls inside an `ON DUPLICATE KEY
/// UPDATE` RHS to SQLite `excluded.col`. Bare column references are kept as-is
/// (they refer to the existing row in both MySQL and SQLite `ON CONFLICT`).
fn rewrite_values_ref(expr: Expr) -> Expr {
    match expr {
        Expr::Function(mut f) => {
            let name = f.name.to_string().to_ascii_uppercase();
            if name == "VALUES" {
                if let FunctionArguments::List(list) = &f.args {
                    if list.args.len() == 1 {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(inner)) = &list.args[0] {
                            if let Expr::Identifier(col) = inner {
                                return Expr::CompoundIdentifier(vec![
                                    Ident::new("excluded"),
                                    col.clone(),
                                ]);
                            }
                        }
                    }
                }
            }
            if let FunctionArguments::List(list) = &mut f.args {
                for arg in list.args.iter_mut() {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                        *e = rewrite_values_ref(std::mem::replace(
                            e,
                            Expr::Value(sqlparser::ast::Value::Null.into()),
                        ));
                    }
                }
            }
            Expr::Function(f)
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(rewrite_values_ref(*left)),
            op,
            right: Box::new(rewrite_values_ref(*right)),
        },
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op,
            expr: Box::new(rewrite_values_ref(*expr)),
        },
        Expr::Nested(e) => Expr::Nested(Box::new(rewrite_values_ref(*e))),
        Expr::Cast { kind, expr, data_type, format, array } => Expr::Cast {
            kind,
            expr: Box::new(rewrite_values_ref(*expr)),
            data_type,
            format,
            array,
        },
        other => other,
    }
}

fn rewrite_update(upd: &mut Update, target: TargetDialect) {
    if target == TargetDialect::Mysql {
        upd.returning = None;
    }
    if target == TargetDialect::Postgres && upd.or.is_some() {
        upd.or = None;
    }
    if target == TargetDialect::Mysql && upd.or.is_some() {
        // `UPDATE OR ROLLBACK` etc. are SQLite-only.
        upd.or = None;
    }
}

fn rewrite_delete(del: &mut Delete, target: TargetDialect) {
    if target == TargetDialect::Mysql {
        del.returning = None;
    }
}

/// Visitor that applies expression-level rewrites for the target dialect.
pub struct ExprRewriter {
    pub target: TargetDialect,
}

fn lower_expr(expr: Expr) -> Expr {
    fn_call("LOWER", vec![expr])
}

fn fn_call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Function(Function {
        name: ObjectName::from(Ident::new(name)),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: args
                .into_iter()
                .map(|e| FunctionArg::Unnamed(FunctionArgExpr::Expr(e)))
                .collect(),
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

impl VisitorMut for ExprRewriter {
    type Break = ();

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        match expr {
            Expr::ILike {
                negated,
                any,
                expr: lhs,
                pattern,
                escape_char,
            } if self.target == TargetDialect::Mysql || self.target == TargetDialect::Sqlite => {
                // Neither MySQL nor SQLite support ILIKE; emulate with
                // `LOWER(lhs) LIKE LOWER(pattern)`.
                let lhs = lhs.as_ref().clone();
                let pattern = pattern.as_ref().clone();
                let like = Expr::Like {
                    negated: *negated,
                    any: *any,
                    expr: Box::new(lower_expr(lhs)),
                    pattern: Box::new(lower_expr(pattern)),
                    escape_char: escape_char.clone(),
                };
                *expr = like;
            }
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::StringConcat,
                right,
            } if self.target == TargetDialect::Mysql => {
                let l = left.as_ref().clone();
                let r = right.as_ref().clone();
                *expr = fn_call("CONCAT", vec![l, r]);
            }
            Expr::Cast {
                kind: sqlparser::ast::CastKind::DoubleColon,
                ..
            } if self.target != TargetDialect::Postgres => {
                // PostgreSQL `::type` syntax is not valid in MySQL/SQLite; emit
                // standard `CAST(... AS ...)` instead.
                if let Expr::Cast { kind, .. } = expr {
                    *kind = sqlparser::ast::CastKind::Cast;
                }
            }
            Expr::Function(f) if self.target == TargetDialect::Sqlite => {
                let name = f.name.to_string().to_ascii_lowercase();
                match name.as_str() {
                    "now" | "current_timestamp" => {
                        // SQLite has no now(); CURRENT_TIMESTAMP is a keyword.
                        *expr = Expr::Identifier(Ident::new("CURRENT_TIMESTAMP"));
                    }
                    "uuid_generate_v4" | "gen_random_uuid" => {
                        let sixteen = Expr::Value(
                            sqlparser::ast::Value::Number("16".to_string(), false).into(),
                        );
                        let blob = fn_call("randomblob", vec![sixteen]);
                        let hex = fn_call("hex", vec![blob]);
                        *expr = fn_call("lower", vec![hex]);
                    }
                    "rand" => {
                        *expr = Expr::Value(
                            sqlparser::ast::Value::Number(
                                "((random() / 18446744073709551616) + 1) / 2 * -1".to_string(),
                                false,
                            )
                            .into(),
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

/// Convert identifier quote style for the target dialect, without touching
/// string literals or comments.
///
/// - MySQL uses backticks for identifiers; PG/SQLite use double quotes.
/// - Only quotes that can only be identifiers (emitted by the parser) are
///   converted; strings are always single-quoted.
pub fn fix_quoting(sql: &str, target: TargetDialect) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;

    while i < n {
        let c = chars[i];
        match c {
            '\'' => {
                let start = i;
                i += 1;
                while i < n {
                    if chars[i] == '\'' {
                        i += 1;
                        if i < n && chars[i] == '\'' {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                out.extend(chars[start..i].iter());
            }
            '$' => {
                let next = if i + 1 < n { chars[i + 1] } else { '\0' };
                if next == '$' || next.is_alphanumeric() {
                    let end = skip_dollar_quoted(&chars, i);
                    if end > i + 1 {
                        out.extend(chars[i..end].iter());
                        i = end;
                        continue;
                    }
                }
                out.push('$');
                i += 1;
            }
            '"' | '`' => {
                let q = c;
                let target_quote = if target == TargetDialect::Mysql { '`' } else { '"' };
                let start = i;
                i += 1;
                while i < n {
                    if chars[i] == q {
                        i += 1;
                        if i < n && chars[i] == q {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                out.push(target_quote);
                // Escape any occurrence of the *target* quote inside the
                // identifier body by doubling it.
                for ch in &chars[start + 1..i.saturating_sub(1)] {
                    if *ch == target_quote {
                        out.push(target_quote);
                    }
                    out.push(*ch);
                }
                out.push(target_quote);
            }
            '-' if i + 1 < n && chars[i + 1] == '-' => {
                let start = i;
                i += 2;
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
                out.extend(chars[start..i].iter());
            }
            '/' if i + 1 < n && chars[i + 1] == '*' => {
                let start = i;
                i += 2;
                while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(n);
                out.extend(chars[start..i].iter());
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    out
}

/// Skip a `$tag$...$tag$` dollar-quoted string starting at a `$`.
fn skip_dollar_quoted(chars: &[char], start: usize) -> usize {
    let n = chars.len();
    let mut j = start + 1;
    while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if j >= n || chars[j] != '$' {
        return start + 1;
    }
    let tag: Vec<char> = chars[start + 1..j].to_vec();
    let mut k = j + 1;
    while k < n {
        if chars[k] == '$' {
            let mut m = k + 1;
            let mut t = 0usize;
            while t < tag.len() && m < n && chars[m] == tag[t] {
                t += 1;
                m += 1;
            }
            if t == tag.len() && m < n && chars[m] == '$' {
                return m + 1;
            }
        }
        k += 1;
    }
    start + 1
}