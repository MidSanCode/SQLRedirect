//! Prepared statement support.
//!
//! Handles COM_STMT_PREPARE, COM_STMT_EXECUTE, COM_STMT_CLOSE,
//! COM_STMT_RESET, COM_STMT_SEND_LONG_DATA, and COM_STMT_FETCH.

use std::collections::HashMap;

use bytes::{BufMut, BytesMut};

use crate::error::MysqlError;

/// Find the byte positions of unquoted `?` parameter placeholders in SQL.
fn find_param_positions(sql: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_backtick = false;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' if !in_double_quote && !in_backtick => {
                // Skip escaped quotes
                if i > 0 && bytes[i - 1] == b'\\' {
                    // escaped, do nothing
                } else {
                    in_single_quote = !in_single_quote;
                }
            }
            b'"' if !in_single_quote && !in_backtick => {
                if i > 0 && bytes[i - 1] == b'\\' {
                    // escaped
                } else {
                    in_double_quote = !in_double_quote;
                }
            }
            b'`' if !in_single_quote && !in_double_quote => {
                in_backtick = !in_backtick;
            }
            b'?' if !in_single_quote && !in_double_quote && !in_backtick => {
                positions.push(i);
            }
            _ => {}
        }
        i += 1;
    }
    positions
}

/// A prepared statement tracked by the connection.
#[derive(Debug, Clone)]
pub struct PreparedStatement {
    /// Server-assigned statement ID.
    pub stmt_id: u32,
    /// Original SQL with `?` placeholders.
    pub sql: String,
    /// Number of `?` parameter placeholders.
    pub num_params: usize,
    /// Byte positions of `?` in the original SQL.
    param_positions: Vec<usize>,
    /// Buffers for COM_STMT_SEND_LONG_DATA.
    pub param_buffers: Option<HashMap<u16, Vec<u8>>>,
}

impl PreparedStatement {
    /// Create a new prepared statement from SQL.
    pub fn new(stmt_id: u32, sql: String) -> Self {
        let positions = find_param_positions(&sql);
        let num_params = positions.len();
        PreparedStatement {
            stmt_id,
            sql,
            num_params,
            param_positions: positions,
            param_buffers: None,
        }
    }

    /// Interpolate parameters into the SQL string, replacing `?` with values.
    pub fn interpolate(&self, params: &[Option<String>]) -> Result<String, MysqlError> {
        if params.len() < self.num_params {
            return Err(MysqlError::Protocol("not enough parameters".into()));
        }

        let mut result = String::with_capacity(self.sql.len());
        let mut last_end = 0;

        for (idx, &pos) in self.param_positions.iter().enumerate() {
            result.push_str(&self.sql[last_end..pos]);
            match &params[idx] {
                Some(val) => {
                    let escaped = val.replace('\'', "''");
                    result.push('\'');
                    result.push_str(&escaped);
                    result.push('\'');
                }
                None => {
                    result.push_str("NULL");
                }
            }
            last_end = pos + 1; // skip the `?`
        }
        result.push_str(&self.sql[last_end..]);
        Ok(result)
    }
}

/// Build a COM_STMT_PREPARE_OK response.
pub fn build_stmt_prepare_ok(stmt: &PreparedStatement) -> Vec<u8> {
    let mut buf = BytesMut::new();
    buf.put_u8(0x00); // OK marker
    buf.put_u32_le(stmt.stmt_id);
    buf.put_u16_le(0); // number of columns (we don't know yet)
    buf.put_u16_le(stmt.num_params as u16);
    buf.put_u8(0); // filler
    buf.put_u16_le(0); // warnings
    buf.to_vec()
}

/// Parse COM_STMT_EXECUTE data to extract the statement ID and parameters.
///
/// Returns (stmt_id, cursor_flags, params).
pub fn parse_stmt_execute_data(
    data: &[u8],
    stmt: &PreparedStatement,
) -> Result<Vec<Option<String>>, MysqlError> {
    if data.len() < 9 {
        return Err(MysqlError::Protocol("COM_STMT_EXECUTE too short".into()));
    }

    // Skip: stmt_id(4) + flags(1) + iteration_count(4) = 9 bytes
    let mut pos = 9;

    if stmt.num_params == 0 {
        return Ok(Vec::new());
    }

    // Null bitmap
    let null_bitmap_len = stmt.num_params.div_ceil(8);
    if pos + null_bitmap_len > data.len() {
        return Err(MysqlError::Protocol(
            "COM_STMT_EXECUTE: truncated null bitmap".into(),
        ));
    }
    let null_bitmap = &data[pos..pos + null_bitmap_len];
    pos += null_bitmap_len;

    // new-params-bound-flag
    if pos >= data.len() {
        return Err(MysqlError::Protocol(
            "COM_STMT_EXECUTE: missing new-params-bound-flag".into(),
        ));
    }
    let new_params_bound = data[pos];
    pos += 1;

    let mut params: Vec<Option<String>> = vec![None; stmt.num_params];

    if new_params_bound != 0 {
        // Read parameter types (2 bytes each)
        let types_len = stmt.num_params * 2;
        if pos + types_len > data.len() {
            return Err(MysqlError::Protocol(
                "COM_STMT_EXECUTE: truncated param types".into(),
            ));
        }
        let param_types: Vec<(u8, bool)> = (0..stmt.num_params)
            .map(|i| {
                let type_byte = data[pos + i * 2];
                let unsigned = data[pos + i * 2 + 1] & 0x80 != 0;
                (type_byte, unsigned)
            })
            .collect();
        pos += types_len;

        // Read parameter values
        for (i, &(type_byte, _unsigned)) in param_types.iter().enumerate() {
            // Check null bitmap
            if null_bitmap[i / 8] & (1 << (i % 8)) != 0 {
                params[i] = None;
                continue;
            }

            // Check for long data buffers
            if let Some(ref buffers) = stmt.param_buffers {
                if let Some(buf) = buffers.get(&(i as u16)) {
                    params[i] = Some(String::from_utf8_lossy(buf).to_string());
                    continue;
                }
            }

            let val = read_binary_value(type_byte, &data[pos..], &mut pos)?;
            params[i] = Some(val);
        }
    }

    Ok(params)
}

/// Read a single binary value per MySQL type from the data buffer.
fn read_binary_value(
    type_byte: u8,
    data: &[u8],
    global_pos: &mut usize,
) -> Result<String, MysqlError> {
    let remaining = data;

    match type_byte {
        // TINY (1 byte)
        0x01 => {
            if remaining.is_empty() {
                return Err(MysqlError::Protocol("truncated TINY value".into()));
            }
            *global_pos += 1;
            Ok((remaining[0] as i8).to_string())
        }
        // SHORT (2 bytes)
        0x02 => {
            if remaining.len() < 2 {
                return Err(MysqlError::Protocol("truncated SHORT value".into()));
            }
            let val = i16::from_le_bytes([remaining[0], remaining[1]]);
            *global_pos += 2;
            Ok(val.to_string())
        }
        // LONG (4 bytes)
        0x03 => {
            if remaining.len() < 4 {
                return Err(MysqlError::Protocol("truncated LONG value".into()));
            }
            let val = i32::from_le_bytes([remaining[0], remaining[1], remaining[2], remaining[3]]);
            *global_pos += 4;
            Ok(val.to_string())
        }
        // FLOAT (4 bytes)
        0x04 => {
            if remaining.len() < 4 {
                return Err(MysqlError::Protocol("truncated FLOAT value".into()));
            }
            let val = f32::from_le_bytes([remaining[0], remaining[1], remaining[2], remaining[3]]);
            *global_pos += 4;
            Ok(val.to_string())
        }
        // DOUBLE (8 bytes)
        0x05 => {
            if remaining.len() < 8 {
                return Err(MysqlError::Protocol("truncated DOUBLE value".into()));
            }
            let val = f64::from_le_bytes([
                remaining[0],
                remaining[1],
                remaining[2],
                remaining[3],
                remaining[4],
                remaining[5],
                remaining[6],
                remaining[7],
            ]);
            *global_pos += 8;
            Ok(val.to_string())
        }
        // LONGLONG (8 bytes)
        0x08 => {
            if remaining.len() < 8 {
                return Err(MysqlError::Protocol("truncated LONGLONG value".into()));
            }
            let val = i64::from_le_bytes([
                remaining[0],
                remaining[1],
                remaining[2],
                remaining[3],
                remaining[4],
                remaining[5],
                remaining[6],
                remaining[7],
            ]);
            *global_pos += 8;
            Ok(val.to_string())
        }
        // NULL type
        0x06 => Ok("NULL".into()),
        // VARCHAR, VAR_STRING, STRING, BLOB, etc. — length-encoded string
        0x0f | 0xf5 | 0xfc | 0xfd | 0xfe | 0x00 => {
            let (s, bytes_read) = read_lenenc_string_raw(remaining)?;
            *global_pos += bytes_read;
            Ok(s)
        }
        _ => {
            // Fall back: try to read as length-encoded string
            let (s, bytes_read) = read_lenenc_string_raw(remaining)?;
            *global_pos += bytes_read;
            Ok(s)
        }
    }
}

/// Read a length-encoded string from raw bytes.
/// Returns (string_value, total_bytes_consumed).
fn read_lenenc_string_raw(data: &[u8]) -> Result<(String, usize), MysqlError> {
    if data.is_empty() {
        return Err(MysqlError::Protocol("empty data for lenenc string".into()));
    }
    let (len, header_size) = match data[0] {
        0..=0xfa => (data[0] as usize, 1),
        0xfc => {
            if data.len() < 3 {
                return Err(MysqlError::Protocol("truncated lenenc string".into()));
            }
            (u16::from_le_bytes([data[1], data[2]]) as usize, 3)
        }
        0xfd => {
            if data.len() < 4 {
                return Err(MysqlError::Protocol("truncated lenenc string".into()));
            }
            (
                (data[1] as usize) | ((data[2] as usize) << 8) | ((data[3] as usize) << 16),
                4,
            )
        }
        0xfe => {
            if data.len() < 9 {
                return Err(MysqlError::Protocol("truncated lenenc string".into()));
            }
            let val = u64::from_le_bytes([
                data[1], data[2], data[3], data[4], data[5], data[6], data[7], data[8],
            ]);
            (val as usize, 9)
        }
        _ => return Err(MysqlError::Protocol("invalid lenenc prefix".into())),
    };

    let total = header_size + len;
    if data.len() < total {
        return Err(MysqlError::Protocol("truncated lenenc string data".into()));
    }
    let s = String::from_utf8_lossy(&data[header_size..total]).to_string();
    Ok((s, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepared_statement_num_params() {
        let stmt = PreparedStatement::new(1, "SELECT ? FROM t WHERE id = ?".into());
        assert_eq!(stmt.num_params, 2);
    }

    #[test]
    fn test_prepared_statement_no_params() {
        let stmt = PreparedStatement::new(1, "SELECT 1".into());
        assert_eq!(stmt.num_params, 0);
    }

    #[test]
    fn test_interpolate() {
        let stmt = PreparedStatement::new(1, "SELECT ? FROM t WHERE id = ?".into());
        let result = stmt
            .interpolate(&[Some("hello".into()), Some("42".into())])
            .unwrap();
        assert_eq!(result, "SELECT 'hello' FROM t WHERE id = '42'");
    }

    #[test]
    fn test_interpolate_null() {
        let stmt = PreparedStatement::new(1, "SELECT ? FROM t".into());
        let result = stmt.interpolate(&[None]).unwrap();
        assert_eq!(result, "SELECT NULL FROM t");
    }

    #[test]
    fn test_quoted_question_mark() {
        let stmt = PreparedStatement::new(1, "SELECT '?' FROM t WHERE id = ?".into());
        assert_eq!(stmt.num_params, 1);
    }
}
