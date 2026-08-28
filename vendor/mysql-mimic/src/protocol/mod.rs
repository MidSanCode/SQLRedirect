//! MySQL wire protocol implementation.
//!
//! This module contains the low-level protocol handling for MySQL client-server
//! communication, including packet framing, handshake, authentication, and
//! command processing.

pub mod auth;
pub mod constants;
pub mod packet;

use bytes::{BufMut, BytesMut};

use crate::error::MysqlError;

/// Reads a length-encoded integer from a buffer.
pub fn read_lenenc_int(buf: &mut &[u8]) -> Result<u64, MysqlError> {
    if buf.is_empty() {
        return Err(MysqlError::Protocol("unexpected end of buffer".into()));
    }
    let first = buf[0];
    *buf = &buf[1..];
    match first {
        0..=0xfa => Ok(first as u64),
        0xfc => {
            if buf.len() < 2 {
                return Err(MysqlError::Protocol("truncated lenenc int".into()));
            }
            let val = u16::from_le_bytes([buf[0], buf[1]]) as u64;
            *buf = &buf[2..];
            Ok(val)
        }
        0xfd => {
            if buf.len() < 3 {
                return Err(MysqlError::Protocol("truncated lenenc int".into()));
            }
            let val = (buf[0] as u64) | ((buf[1] as u64) << 8) | ((buf[2] as u64) << 16);
            *buf = &buf[3..];
            Ok(val)
        }
        0xfe => {
            if buf.len() < 8 {
                return Err(MysqlError::Protocol("truncated lenenc int".into()));
            }
            let val = u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]);
            *buf = &buf[8..];
            Ok(val)
        }
        0xfb => Ok(0), // NULL
        0xff => Err(MysqlError::Protocol("unexpected 0xff in lenenc int".into())),
    }
}

/// Writes a length-encoded integer to a buffer.
pub fn write_lenenc_int(buf: &mut BytesMut, val: u64) {
    if val < 251 {
        buf.put_u8(val as u8);
    } else if val < 65536 {
        buf.put_u8(0xfc);
        buf.put_u16_le(val as u16);
    } else if val < 16777216 {
        buf.put_u8(0xfd);
        buf.put_u8(val as u8);
        buf.put_u8((val >> 8) as u8);
        buf.put_u8((val >> 16) as u8);
    } else {
        buf.put_u8(0xfe);
        buf.put_u64_le(val);
    }
}

/// Writes a length-encoded string to a buffer.
pub fn write_lenenc_str(buf: &mut BytesMut, s: &[u8]) {
    write_lenenc_int(buf, s.len() as u64);
    buf.extend_from_slice(s);
}

/// Reads a null-terminated string from a buffer.
pub fn read_null_terminated_string(buf: &mut &[u8]) -> Result<Vec<u8>, MysqlError> {
    let pos = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| MysqlError::Protocol("missing null terminator".into()))?;
    let s = buf[..pos].to_vec();
    *buf = &buf[pos + 1..];
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lenenc_int_roundtrip() {
        let values = [
            0u64,
            1,
            250,
            251,
            65535,
            65536,
            16777215,
            16777216,
            u64::MAX,
        ];
        for &val in &values {
            let mut buf = BytesMut::new();
            write_lenenc_int(&mut buf, val);
            let mut slice: &[u8] = &buf;
            let decoded = read_lenenc_int(&mut slice).unwrap();
            assert_eq!(val, decoded, "roundtrip failed for {val}");
        }
    }

    #[test]
    fn test_null_terminated_string() {
        let data = b"hello\x00world";
        let mut slice: &[u8] = data;
        let s = read_null_terminated_string(&mut slice).unwrap();
        assert_eq!(s, b"hello");
        assert_eq!(slice, b"world");
    }
}
