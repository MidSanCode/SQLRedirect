//! Portable row/value types shared between the backend and wire front-ends.

use sqlx::any::AnyTypeInfoKind;

/// Column metadata for a result set.
#[derive(Debug, Clone)]
pub struct FieldMeta {
    pub name: String,
    pub kind: AnyTypeInfoKind,
}

/// A decoded cell value, portable across backends.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    SmallInt(i16),
    Integer(i32),
    BigInt(i64),
    Real(f32),
    Double(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    /// Render the value as text for the MySQL text protocol.
    pub fn as_mysql_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
            Value::SmallInt(i) => Some(i.to_string()),
            Value::Integer(i) => Some(i.to_string()),
            Value::BigInt(i) => Some(i.to_string()),
            Value::Real(f) => Some(format_float(*f as f64)),
            Value::Double(f) => Some(format_float(*f)),
            Value::Text(s) => Some(s.clone()),
            Value::Blob(b) => Some(String::from_utf8_lossy(b).to_string()),
        }
    }

    /// Render the value as text for the PostgreSQL text protocol by emitting a
    /// proper literal-like string.
    pub fn as_pg_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
            Value::SmallInt(i) => Some(i.to_string()),
            Value::Integer(i) => Some(i.to_string()),
            Value::BigInt(i) => Some(i.to_string()),
            Value::Real(f) => Some(format_float(*f as f64)),
            Value::Double(f) => Some(format_float(*f)),
            Value::Text(s) => Some(s.clone()),
            Value::Blob(b) => Some(escape_bytea(b)),
        }
    }

    /// Coerce the value to `bool` (for wire encoding at a bool column).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::SmallInt(i) => Some(*i != 0),
            Value::Integer(i) => Some(*i != 0),
            Value::BigInt(i) => Some(*i != 0),
            Value::Text(s) => Some(!s.is_empty()),
            _ => None,
        }
    }

    /// Coerce the value to `i16` (for wire encoding at an int2 column).
    pub fn as_i16(&self) -> Option<i16> {
        match self {
            Value::SmallInt(i) => Some(*i),
            Value::Integer(i) => i16::try_from(*i).ok(),
            Value::BigInt(i) => i16::try_from(*i).ok(),
            Value::Bool(b) => Some(if *b { 1 } else { 0 }),
            _ => None,
        }
    }

    /// Coerce the value to `i32` (for wire encoding at an int4 column).
    pub fn as_i32(&self) -> Option<i32> {
        match self {
            Value::SmallInt(i) => Some(*i as i32),
            Value::Integer(i) => Some(*i),
            Value::BigInt(i) => i32::try_from(*i).ok(),
            Value::Bool(b) => Some(if *b { 1 } else { 0 }),
            _ => None,
        }
    }

    /// Coerce the value to `i64` (for wire encoding at an int8 column).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::SmallInt(i) => Some(*i as i64),
            Value::Integer(i) => Some(*i as i64),
            Value::BigInt(i) => Some(*i),
            Value::Bool(b) => Some(if *b { 1 } else { 0 }),
            _ => None,
        }
    }

    /// Coerce the value to `f32` (for wire encoding at a float4 column).
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            Value::SmallInt(i) => Some(*i as f32),
            Value::Integer(i) => Some(*i as f32),
            Value::BigInt(i) => Some(*i as f32),
            Value::Real(f) => Some(*f),
            Value::Double(f) => Some(*f as f32),
            _ => None,
        }
    }

    /// Coerce the value to `f64` (for wire encoding at a float8 column).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::SmallInt(i) => Some(*i as f64),
            Value::Integer(i) => Some(*i as f64),
            Value::BigInt(i) => Some(*i as f64),
            Value::Real(f) => Some(*f as f64),
            Value::Double(f) => Some(*f),
            _ => None,
        }
    }
}

fn format_float(f: f64) -> String {
    if f == f.trunc() && f.is_finite() && f.abs() < 1e15 {
        // Render integral floats without trailing ".0" only when truly integral
        format!("{f:.1}")
    } else {
        format!("{}", f)
    }
}

fn escape_bytea(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            0x00 => s.push_str("\\\\000"),
            0x27 => s.push_str("''"),
            0x5c => s.push_str("\\\\\\\\"),
            0x0a => s.push_str("\\\\012"),
            0x0d => s.push_str("\\\\015"),
            0x08 => s.push_str("\\\\b"),
            0x09 => s.push_str("\\\\t"),
            _ if b < 0x20 || b >= 0x7f => s.push_str(&format!("\\\\{:03o}", b)),
            _ => s.push(b as char),
        }
    }
    s
}

/// Map a backend driver type kind to a stable name.
pub fn kind_to_pg_oid(kind: &AnyTypeInfoKind) -> u32 {
    use AnyTypeInfoKind::*;
    match kind {
        Bool => 16,     // boolean
        SmallInt => 21, // int2
        Integer => 23,  // int4
        BigInt => 20,   // int8
        Real => 700,    // float4
        Double => 701,  // float8
        Text => 25,     // text
        Blob => 17,     // bytea
        _ => 25,        // text (incl. Null / unknown)
    }
}

/// Map a backend driver type kind to a MySQL column type flag.
pub fn kind_to_mysql_type(kind: &AnyTypeInfoKind) -> u16 {
    use AnyTypeInfoKind::*;
    match kind {
        Bool => 1,      // MYSQL_TYPE_TINY
        SmallInt => 2,  // MYSQL_TYPE_SHORT
        Integer => 3,   // MYSQL_TYPE_LONG
        BigInt => 8,    // MYSQL_TYPE_LONGLONG
        Real => 4,      // MYSQL_TYPE_FLOAT
        Double => 5,    // MYSQL_TYPE_DOUBLE
        Blob => 252,    // MYSQL_TYPE_BLOB
        _ => 253,       // MYSQL_TYPE_VAR_STRING (incl. Text, Null)
    }
}