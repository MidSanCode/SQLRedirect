//! CREATE TABLE type and column-option mapping between SQL dialects.

use sqlparser::ast::{
    ColumnDef, ColumnOption, ColumnOptionDef, CreateTable, DataType, Ident, ObjectName,
    PrimaryKeyConstraint, TableConstraint,
};
use sqlparser::tokenizer::Token;

use super::TargetDialect;
use crate::error::Result;

/// Integer width of an auto-incrementing column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoInc {
    Small,
    Int,
    Big,
}

/// Rewrite a `CREATE TABLE` statement for the target dialect: data types,
/// auto-increment handling and dialect-specific column/table options.
pub fn rewrite_create_table(ct: &mut CreateTable, target: TargetDialect) -> Result<()> {
    // Drop MySQL/SQLite-only table options for PG; drop PG WITH () options for MySQL/SQLite.
    if target != TargetDialect::Mysql {
        ct.table_options = sqlparser::ast::CreateTableOptions::None;
    }
    if target != TargetDialect::Postgres && matches!(ct.or_replace, true) {
        // CREATE OR REPLACE TABLE is not portable; drop the modifier.
        ct.or_replace = false;
    }

    let mut autoincr_col_name: Option<String> = None;

    for col in ct.columns.iter_mut() {
        col.data_type = map_type(&col.data_type, target)?;
        let auto = detect_auto_increment(col);
        rewrite_column_options(col, target, auto)?;
        if auto.is_some() {
            autoincr_col_name = Some(col.name.value.clone());
        }
    }

    // SQLite requires the auto-increment column to be the primary key and only
    // one PRIMARY KEY is allowed per table; drop a table-level PK in that case.
    if target == TargetDialect::Sqlite {
        if let Some(pk_name) = &autoincr_col_name {
            ct.constraints.retain(|c| {
                if let TableConstraint::PrimaryKey(pk) = c {
                    let names = pk
                        .columns
                        .iter()
                        .map(|c| c.column.to_string().to_ascii_lowercase())
                        .collect::<Vec<_>>();
                    !names.iter().any(|n| n == &pk_name.to_ascii_lowercase())
                } else {
                    true
                }
            });
        }
    }

    Ok(())
}

/// Detect whether a column is an auto-increment column and its integer width.
fn detect_auto_increment(col: &ColumnDef) -> Option<AutoInc> {
    // PostgreSQL `SERIAL`/`BIGSERIAL`/`SMALLSERIAL` pseudo-types.
    if let DataType::Custom(name, _) = &col.data_type {
        let n = name.to_string().to_ascii_lowercase();
        match n.as_str() {
            "serial" | "serial4" => return Some(AutoInc::Int),
            "bigserial" | "serial8" => return Some(AutoInc::Big),
            "smallserial" | "serial2" => return Some(AutoInc::Small),
            _ => {}
            // Other custom types may still carry AUTO_INCREMENT/AUTOINCREMENT
            // column options; fall through to check them.
        }
    }
    // MySQL `AUTO_INCREMENT` / SQLite `AUTOINCREMENT` column options.
    for opt in &col.options {
        if let ColumnOption::DialectSpecific(tokens) = &opt.option {
            let mut found = false;
            for t in tokens {
                if let Token::Word(w) = t {
                    let v = w.value.to_ascii_uppercase();
                    if v == "AUTO_INCREMENT" || v == "AUTOINCREMENT" {
                        found = true;
                        break;
                    }
                }
            }
            if found {
                let width = match &col.data_type {
                    DataType::SmallInt(_) | DataType::Int2(_) | DataType::TinyInt(_) => {
                        AutoInc::Small
                    }
                    DataType::BigInt(_) | DataType::Int8(_) => AutoInc::Big,
                    _ => AutoInc::Int,
                };
                return Some(width);
            }
        }
    }
    None
}

/// Rewrite a column's options (auto increment, dialect-specific tokens) for the
/// target dialect.
fn rewrite_column_options(
    col: &mut ColumnDef,
    target: TargetDialect,
    auto: Option<AutoInc>,
) -> Result<()> {
    let mut options = std::mem::take(&mut col.options);
    let mut kept = Vec::with_capacity(options.len() + 2);

    for opt in options.drain(..) {
        match &opt.option {
            ColumnOption::DialectSpecific(tokens) => {
                let mut is_auto = false;
                let mut strip = false;
                for t in tokens {
                    if let Token::Word(w) = t {
                        let v = w.value.to_ascii_uppercase();
                        if v == "AUTO_INCREMENT" || v == "AUTOINCREMENT" {
                            is_auto = true;
                        }
                        // Non-portable MySQL tokens.
                        if matches!(
                            w.value.to_ascii_uppercase().as_str(),
                            "ENGINE" | "COMMENT" | "CHARSET" | "COLLATE"
                        ) && target != TargetDialect::Mysql
                        {
                            strip = true;
                        }
                    }
                }
                if is_auto {
                    // Handled below via the auto-increment block.
                    continue;
                }
                if strip {
                    continue;
                }
                kept.push(opt);
            }
            ColumnOption::Comment(_) | ColumnOption::CharacterSet(_)
            | ColumnOption::Collation(_) | ColumnOption::OnUpdate(_)
                if target != TargetDialect::Mysql =>
            {
                // MySQL-only column options.
                continue;
            }
            ColumnOption::Check(_) | ColumnOption::Generated { .. } | ColumnOption::Default(_)
            | ColumnOption::PrimaryKey(_) | ColumnOption::NotNull | ColumnOption::Null
            | ColumnOption::Unique(_) | ColumnOption::ForeignKey(_) => {
                kept.push(opt);
            }
            _ => {
                // Unknown/unhandled options: keep them (round-trip).
                kept.push(opt);
            }
        }
    }

    if let Some(width) = auto {
        match target {
            TargetDialect::Mysql => {
                col.data_type = int_type(width, "MYSQL");
                kept.push(column_option(ColumnOption::DialectSpecific(vec![
                    Token::make_keyword("AUTO_INCREMENT"),
                ])));
            }
            TargetDialect::Sqlite => {
                col.data_type = int_type(width, "SQLITE");
                kept.push(column_option(ColumnOption::DialectSpecific(vec![
                    Token::make_keyword("AUTOINCREMENT"),
                ])));
                // SQLite needs `INTEGER PRIMARY KEY AUTOINCREMENT`.
                if !kept.iter().any(|o| matches!(o.option, ColumnOption::PrimaryKey(_))) {
                    kept.push(ColumnOptionDef {
                        name: None,
                        option: ColumnOption::PrimaryKey(PrimaryKeyConstraint {
                            name: None,
                            index_name: None,
                            index_type: None,
                            columns: vec![],
                            index_options: vec![],
                            characteristics: None,
                        }),
                    });
                }
            }
            TargetDialect::Postgres => {
                // Convert to `SERIAL`/`BIGSERIAL`/`SMALLSERIAL`.
                let kind = match width {
                    AutoInc::Small => "SMALLSERIAL",
                    AutoInc::Int => "SERIAL",
                    AutoInc::Big => "BIGSERIAL",
                };
                col.data_type = DataType::Custom(
                    ObjectName::from(Ident::new(kind)),
                    vec![],
                );
            }
        }
    }

    col.options = kept;
    Ok(())
}

fn column_option(option: ColumnOption) -> ColumnOptionDef {
    ColumnOptionDef { name: None, option }
}

fn int_type(width: AutoInc, dialect: &str) -> DataType {
    let name = match width {
        AutoInc::Small => "SMALLINT",
        AutoInc::Int => "INTEGER",
        AutoInc::Big => "BIGINT",
    };
    match dialect {
        "MYSQL" => DataType::Custom(ObjectName::from(Ident::new(name)), vec![]),
        _ => DataType::Custom(ObjectName::from(Ident::new(name)), vec![]),
    }
}

/// Map a data type variant to a portable form for the target dialect.
fn map_type(dt: &DataType, target: TargetDialect) -> Result<DataType> {
    use DataType::*;
    Ok(match target {
        TargetDialect::Mysql => match dt {
            Bool | Boolean => Custom(obj("BOOLEAN"), vec![]),
            Int(_) | Int4(_) | Integer(_) | Int2(_) | SmallInt(_) | TinyInt(_) | MediumInt(_)
            | Int8(_) | BigInt(_) | IntUnsigned(_) | Int4Unsigned(_) | IntegerUnsigned(_)
            | Int2Unsigned(_) | SmallIntUnsigned(_) | Int8Unsigned(_) | BigIntUnsigned(_) => {
                // Preserve the canonical variants so MySQL keeps its exact types.
                dt.clone()
            }
            Float(_) | Float4 | Float32 => Custom(obj("FLOAT"), vec![]),
            Double(_) | DoubleUnsigned(_) | Float64 | DoublePrecision
            | DoublePrecisionUnsigned | Float8 => Custom(obj("DOUBLE"), vec![]),
            Real | RealUnsigned => Custom(obj("FLOAT"), vec![]),
            Varchar(_) | Char(_) | Character(_) | CharacterVarying(_) | CharVarying(_)
            | String(_) | Text | TinyText | MediumText | LongText | Clob(_) => match dt {
                TinyText => Custom(obj("TINYTEXT"), vec![]),
                MediumText => Custom(obj("MEDIUMTEXT"), vec![]),
                LongText => Custom(obj("LONGTEXT"), vec![]),
                TinyBlob => Custom(obj("TINYBLOB"), vec![]),
                MediumBlob => Custom(obj("MEDIUMBLOB"), vec![]),
                LongBlob => Custom(obj("LONGBLOB"), vec![]),
                _ => dt.clone(),
            },
            JSON | JSONB => Custom(obj("JSON"), vec![]),
            Bytea | Binary(_) | Varbinary(_) | Blob(_) | Bytes(_) | Bit(_) | BitVarying(_)
            | VarBit(_) => Custom(obj("BLOB"), vec![]),
            Numeric(_) | Decimal(_) | Dec(_) | BigDecimal(_) => dt.clone(),
            Date => Custom(obj("DATE"), vec![]),
            Time(_, _) => Custom(obj("TIME"), vec![]),
            Datetime(_) => Custom(obj("DATETIME"), vec![]),
            Timestamp(_, _) | TimestampNtz(_) => Custom(obj("TIMESTAMP"), vec![]),
            Uuid => Custom(obj("CHAR(36)"), vec![]),
            Interval { .. } => Custom(obj("TEXT"), vec![]),
            other => other.clone(),
        },
        TargetDialect::Sqlite => match dt {
            Bool | Boolean => Custom(obj("BOOLEAN"), vec![]),
            Int(_) | Int4(_) | Integer(_) | Int2(_) | SmallInt(_) | TinyInt(_) | MediumInt(_)
            | Int8(_) | BigInt(_) | IntUnsigned(_) | Int4Unsigned(_) | IntegerUnsigned(_)
            | Int2Unsigned(_) | SmallIntUnsigned(_) | Int8Unsigned(_) | BigIntUnsigned(_) => {
                Custom(obj("INTEGER"), vec![])
            }
            Float(_) | Float4 | Float32 | Float8 | Float64 | Double(_) | DoubleUnsigned(_)
            | DoublePrecision | DoublePrecisionUnsigned | Real | RealUnsigned => {
                Custom(obj("REAL"), vec![])
            }
            Varchar(_) | Char(_) | Character(_) | CharacterVarying(_) | CharVarying(_)
            | String(_) | TinyText | MediumText | LongText | Text | Clob(_) => {
                Custom(obj("TEXT"), vec![])
            }
            JSON | JSONB => Custom(obj("TEXT"), vec![]),
            Bytea | Binary(_) | Varbinary(_) | Blob(_) | Bytes(_) | Bit(_) | BitVarying(_)
            | VarBit(_) => Custom(obj("BLOB"), vec![]),
            Numeric(_) | Decimal(_) | Dec(_) | BigDecimal(_) => Custom(obj("NUMERIC"), vec![]),
            Date | Time(_, _) | Datetime(_) | Timestamp(_, _) | TimestampNtz(_) => {
                Custom(obj("TEXT"), vec![])
            }
            Uuid => Custom(obj("TEXT"), vec![]),
            Interval { .. } => Custom(obj("TEXT"), vec![]),
            other => other.clone(),
        },
        TargetDialect::Postgres => match dt {
            Bool | Boolean => Custom(obj("BOOLEAN"), vec![]),
            Int(_) | Int4(_) | Integer(_) | MediumInt(_) => Custom(obj("INTEGER"), vec![]),
            TinyInt(_) | SmallInt(_) | Int2(_) | SmallIntUnsigned(_) | Int2Unsigned(_) => {
                Custom(obj("SMALLINT"), vec![])
            }
            Int8(_) | BigInt(_) | BigIntUnsigned(_) | Int8Unsigned(_) => {
                Custom(obj("BIGINT"), vec![])
            }
            IntUnsigned(_) | Int4Unsigned(_) | IntegerUnsigned(_) => {
                Custom(obj("INTEGER"), vec![])
            }
            Float(_) | Float4 | Float32 | Real | RealUnsigned => Custom(obj("REAL"), vec![]),
            Double(_) | DoubleUnsigned(_) | Float64 | DoublePrecision
            | DoublePrecisionUnsigned | Float8 => Custom(obj("DOUBLE PRECISION"), vec![]),
            Varchar(_) | Char(_) | Character(_) | CharacterVarying(_) | CharVarying(_)
            | String(_) | Text | TinyText | MediumText | LongText | Clob(_) => {
                Custom(obj("TEXT"), vec![])
            }
            JSON => Custom(obj("JSON"), vec![]),
            JSONB => Custom(obj("JSONB"), vec![]),
            Bytea | Binary(_) | Varbinary(_) | Blob(_) | Bytes(_) | Bit(_) | BitVarying(_)
            | VarBit(_) | TinyBlob | MediumBlob | LongBlob => Custom(obj("BYTEA"), vec![]),
            Numeric(_) | Decimal(_) | Dec(_) | BigDecimal(_) => dt.clone(),
            Date => Custom(obj("DATE"), vec![]),
            Time(_, _) => Custom(obj("TIME"), vec![]),
            Datetime(_) | Timestamp(_, _) | TimestampNtz(_) => Custom(obj("TIMESTAMP"), vec![]),
            Uuid => Custom(obj("UUID"), vec![]),
            Interval { .. } => dt.clone(),
            other => other.clone(),
        },
    })
}

fn obj(name: &str) -> ObjectName {
    ObjectName::from(Ident::new(name))
}

#[allow(dead_code)]
fn _silence_unused(ct: &CreateTable) {
    let _ = ct;
}