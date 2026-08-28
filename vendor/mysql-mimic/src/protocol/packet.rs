//! MySQL packet reading and writing.
//!
//! MySQL packets consist of a 3-byte length, 1-byte sequence number, and payload.

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::MysqlError;
use crate::protocol::constants::*;

/// Maximum payload length for a single MySQL packet (16 MB - 1).
pub const MAX_PACKET_SIZE: usize = 0xFF_FF_FF;

/// A raw MySQL packet.
#[derive(Debug, Clone)]
pub struct Packet {
    /// Sequence number for this packet.
    pub sequence_id: u8,
    /// Packet payload.
    pub payload: Vec<u8>,
}

/// Reads a single MySQL packet from the stream.
pub async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Packet, MysqlError> {
    // Read 4-byte header
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await?;

    let payload_length =
        (header[0] as usize) | ((header[1] as usize) << 8) | ((header[2] as usize) << 16);
    let sequence_id = header[3];

    // Read payload
    let mut payload = vec![0u8; payload_length];
    reader.read_exact(&mut payload).await?;

    Ok(Packet {
        sequence_id,
        payload,
    })
}

/// Writes a MySQL packet to the stream.
///
/// If the payload is larger than [`MAX_PACKET_SIZE`], it will be split
/// into multiple packets automatically.
pub async fn write_packet<W: AsyncWrite + Unpin>(
    writer: &mut W,
    sequence_id: &mut u8,
    payload: &[u8],
) -> Result<(), MysqlError> {
    for chunk in payload.chunks(MAX_PACKET_SIZE).chain(
        // If the payload is an exact multiple of MAX_PACKET_SIZE, send an empty terminator
        if payload.len().is_multiple_of(MAX_PACKET_SIZE) && !payload.is_empty() {
            Some(&[][..])
        } else {
            None
        },
    ) {
        let len = chunk.len();
        let mut header = [0u8; 4];
        header[0] = (len & 0xff) as u8;
        header[1] = ((len >> 8) & 0xff) as u8;
        header[2] = ((len >> 16) & 0xff) as u8;
        header[3] = *sequence_id;

        writer.write_all(&header).await?;
        writer.write_all(chunk).await?;

        *sequence_id = sequence_id.wrapping_add(1);
    }

    writer.flush().await?;
    Ok(())
}

/// Builds an OK packet payload.
pub fn build_ok_packet(affected_rows: u64, last_insert_id: u64) -> Vec<u8> {
    let mut buf = BytesMut::new();
    buf.put_u8(OK_MARKER);
    crate::protocol::write_lenenc_int(&mut buf, affected_rows);
    crate::protocol::write_lenenc_int(&mut buf, last_insert_id);
    // Status flags: SERVER_STATUS_AUTOCOMMIT
    buf.put_u16_le(SERVER_STATUS_AUTOCOMMIT);
    // Warnings
    buf.put_u16_le(0);
    buf.to_vec()
}

/// Builds an ERR packet payload.
pub fn build_err_packet(code: u16, sql_state: &str, message: &str) -> Vec<u8> {
    let mut buf = BytesMut::new();
    buf.put_u8(ERR_MARKER);
    buf.put_u16_le(code);
    buf.put_u8(b'#');
    // SQL state: exactly 5 bytes
    let state_bytes = sql_state.as_bytes();
    if state_bytes.len() >= 5 {
        buf.extend_from_slice(&state_bytes[..5]);
    } else {
        buf.extend_from_slice(state_bytes);
        for _ in 0..(5 - state_bytes.len()) {
            buf.put_u8(b' ');
        }
    }
    buf.extend_from_slice(message.as_bytes());
    buf.to_vec()
}

/// Builds an EOF packet payload.
pub fn build_eof_packet() -> Vec<u8> {
    let mut buf = BytesMut::new();
    buf.put_u8(EOF_MARKER);
    buf.put_u16_le(0); // warnings
    buf.put_u16_le(SERVER_STATUS_AUTOCOMMIT); // status flags
    buf.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::io::Builder;

    #[tokio::test]
    async fn test_read_packet() {
        // A packet with payload "ABC" and sequence id 0
        let data: Vec<u8> = vec![
            3, 0, 0, 0, // header: length=3, seq=0
            b'A', b'B', b'C', // payload
        ];
        let mut reader = Builder::new().read(&data).build();
        let pkt = read_packet(&mut reader).await.unwrap();
        assert_eq!(pkt.sequence_id, 0);
        assert_eq!(pkt.payload, b"ABC");
    }

    #[test]
    fn test_build_ok_packet() {
        let pkt = build_ok_packet(1, 0);
        assert_eq!(pkt[0], OK_MARKER);
    }

    #[test]
    fn test_build_err_packet() {
        let pkt = build_err_packet(1064, "42000", "Bad query");
        assert_eq!(pkt[0], ERR_MARKER);
        assert_eq!(u16::from_le_bytes([pkt[1], pkt[2]]), 1064);
    }
}
