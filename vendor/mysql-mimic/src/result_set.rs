//! MySQL result set types.
//!
//! Provides types to construct query results that will be sent back to clients
//! in the MySQL wire protocol format.

use bytes::{BufMut, BytesMut};

/// MySQL column (field) type constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ColumnType {
    /// DECIMAL
    Decimal = 0x00,
    /// TINY (TINYINT)
    Tiny = 0x01,
    /// SHORT (SMALLINT)
    Short = 0x02,
    /// LONG (INT)
    Long = 0x03,
    /// FLOAT
    Float = 0x04,
    /// DOUBLE
    Double = 0x05,
    /// NULL
    Null = 0x06,
    /// TIMESTAMP
    Timestamp = 0x07,
    /// BIGINT
    LongLong = 0x08,
    /// INT24 (MEDIUMINT)
    Int24 = 0x09,
    /// DATE
    Date = 0x0a,
    /// TIME
    Time = 0x0b,
    /// DATETIME
    DateTime = 0x0c,
    /// YEAR
    Year = 0x0d,
    /// VARCHAR
    VarChar = 0x0f,
    /// BIT
    Bit = 0x10,
    /// JSON
    Json = 0xf5,
    /// BLOB
    Blob = 0xfc,
    /// VAR_STRING
    VarString = 0xfd,
    /// STRING
    String = 0xfe,
}

/// Describes a column in a result set.
#[derive(Debug, Clone)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Column type.
    pub column_type: ColumnType,
}

impl Column {
    /// Create a new column definition.
    pub fn new(name: impl Into<String>, column_type: ColumnType) -> Self {
        Column {
            name: name.into(),
            column_type,
        }
    }
}

/// A MySQL result set containing columns and rows.
#[derive(Debug, Clone)]
pub struct ResultSet {
    /// Column definitions.
    pub columns: Vec<Column>,
    /// Row data. Each row is a vector of optional string values (NULL represented as `None`).
    pub rows: Vec<Vec<Option<String>>>,
    /// Last insert ID to report in the OK packet for this statement.
    pub last_insert_id: Option<u64>,
}

impl ResultSet {
    /// Create an empty result set (no columns, no rows).
    pub fn empty() -> Self {
        ResultSet {
            columns: Vec::new(),
            rows: Vec::new(),
            last_insert_id: None,
        }
    }

    /// Create a new result set with the given columns.
    pub fn new(columns: Vec<Column>) -> Self {
        ResultSet {
            columns,
            rows: Vec::new(),
            last_insert_id: None,
        }
    }

    /// Set the last insert ID for the OK packet.
    pub fn with_last_insert_id(mut self, id: u64) -> Self {
        self.last_insert_id = Some(id);
        self
    }

    /// Add a row to the result set.
    pub fn add_row(&mut self, row: Vec<Option<String>>) {
        self.rows.push(row);
    }

    /// Returns true if this result set has no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Serialize a column definition to MySQL wire format.
    pub fn serialize_column(col: &Column) -> Vec<u8> {
        let mut buf = BytesMut::new();
        let catalog = b"def";
        crate::protocol::write_lenenc_str(&mut buf, catalog);
        // schema, table, org_table — empty
        crate::protocol::write_lenenc_str(&mut buf, b"");
        crate::protocol::write_lenenc_str(&mut buf, b"");
        crate::protocol::write_lenenc_str(&mut buf, b"");
        // column name
        crate::protocol::write_lenenc_str(&mut buf, col.name.as_bytes());
        // org_name
        crate::protocol::write_lenenc_str(&mut buf, col.name.as_bytes());
        // filler
        buf.put_u8(0x0c);
        // character set: utf8mb4 (0x2d, 0x00)
        buf.put_u16_le(0x002d);
        // column length
        buf.put_u32_le(255);
        // column type
        buf.put_u8(col.column_type as u8);
        // flags
        buf.put_u16_le(0);
        // decimals
        buf.put_u8(0);
        // filler
        buf.put_u16_le(0);
        buf.to_vec()
    }

    /// Serialize a row to MySQL wire format (text protocol).
    pub fn serialize_row(row: &[Option<String>]) -> Vec<u8> {
        let mut buf = BytesMut::new();
        for val in row {
            match val {
                Some(s) => {
                    crate::protocol::write_lenenc_str(&mut buf, s.as_bytes());
                }
                None => {
                    buf.put_u8(0xfb); // NULL marker
                }
            }
        }
        buf.to_vec()
    }

    /// Serialize a row to MySQL binary protocol format (COM_STMT_EXECUTE results).
    ///
    /// Binary row layout:
    /// 1. one header byte `0x00`
    /// 2. NULL bitmap of `(num_columns + 7 + 2) / 8` bytes (bit `i+2` = column `i`)
    /// 3. each non-NULL value encoded according to its column type
    pub fn serialize_row_binary(row: &[Option<String>], columns: &[Column]) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.put_u8(0x00); // binary row header

        let ncols = row.len();
        let bitmap_len = (ncols + 7 + 2) / 8;
        let mut null_bitmap = vec![0u8; bitmap_len];
        for (i, val) in row.iter().enumerate() {
            if val.is_none() {
                null_bitmap[(i + 2) / 8] |= 1 << ((i + 2) % 8);
            }
        }
        buf.extend_from_slice(&null_bitmap);

        for (i, val) in row.iter().enumerate() {
            let Some(s) = val else { continue };
            let col_type = columns
                .get(i)
                .map(|c| c.column_type)
                .unwrap_or(ColumnType::VarString);
            match col_type {
                ColumnType::Tiny => buf.put_i8(s.parse::<i8>().unwrap_or(0)),
                ColumnType::Short => buf.put_i16_le(s.parse::<i16>().unwrap_or(0)),
                ColumnType::Long | ColumnType::Int24 => {
                    buf.put_i32_le(s.parse::<i32>().unwrap_or(0))
                }
                ColumnType::LongLong => buf.put_i64_le(s.parse::<i64>().unwrap_or(0)),
                ColumnType::Float => buf.put_f32_le(s.parse::<f32>().unwrap_or(0.0)),
                ColumnType::Double => buf.put_f64_le(s.parse::<f64>().unwrap_or(0.0)),
                _ => crate::protocol::write_lenenc_str(&mut buf, s.as_bytes()),
            }
        }
        buf.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_result_set() {
        let rs = ResultSet::empty();
        assert!(rs.is_empty());
        assert!(rs.rows.is_empty());
    }

    #[test]
    fn test_result_set_with_data() {
        let mut rs = ResultSet::new(vec![
            Column::new("id", ColumnType::Long),
            Column::new("name", ColumnType::VarString),
        ]);
        rs.add_row(vec![Some("1".into()), Some("Alice".into())]);
        rs.add_row(vec![Some("2".into()), None]);
        assert_eq!(rs.columns.len(), 2);
        assert_eq!(rs.rows.len(), 2);
    }

    #[test]
    fn test_serialize_row_with_null() {
        let row = vec![Some("hello".into()), None, Some("world".into())];
        let data = ResultSet::serialize_row(&row);
        // NULL should be a single 0xfb byte
        assert!(data.contains(&0xfb));
    }
}
