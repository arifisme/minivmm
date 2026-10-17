use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

pub const PREFIX_LEN: usize = 8;
pub const MAX_HEADER_LEN: usize = 1 << 20;
pub const MAX_PAYLOAD_LEN: usize = 64 << 20;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame header of {0} bytes exceeds the limit")]
    HeaderTooLarge(usize),
    #[error("frame payload of {0} bytes exceeds the limit")]
    PayloadTooLarge(usize),
    #[error("malformed frame header: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("connection closed")]
    Closed,
    #[error("frame I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLengths {
    pub header: usize,
    pub payload: usize,
}

impl FrameLengths {
    pub fn total(&self) -> usize {
        self.header + self.payload
    }
}

/// Every frame is two little-endian `u32` lengths, a JSON header and an opaque payload.
pub fn encode<T: Serialize>(header: &T, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let json = serde_json::to_vec(header)?;
    if json.len() > MAX_HEADER_LEN {
        return Err(FrameError::HeaderTooLarge(json.len()));
    }
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge(payload.len()));
    }
    let mut frame = Vec::with_capacity(PREFIX_LEN + json.len() + payload.len());
    frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&json);
    frame.extend_from_slice(payload);
    Ok(frame)
}

pub fn decode_prefix(prefix: &[u8; PREFIX_LEN]) -> Result<FrameLengths, FrameError> {
    let header = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    let payload = u32::from_le_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]) as usize;
    if header > MAX_HEADER_LEN {
        return Err(FrameError::HeaderTooLarge(header));
    }
    if payload > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge(payload));
    }
    Ok(FrameLengths { header, payload })
}

pub fn decode_body<T: DeserializeOwned>(
    lengths: FrameLengths,
    mut body: Vec<u8>,
) -> Result<(T, Vec<u8>), FrameError> {
    let payload = body.split_off(lengths.header);
    Ok((serde_json::from_slice(&body)?, payload))
}

pub fn write_frame<W: Write, T: Serialize>(
    writer: &mut W,
    header: &T,
    payload: &[u8],
) -> Result<(), FrameError> {
    writer.write_all(&encode(header, payload)?)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<(T, Vec<u8>), FrameError> {
    let mut prefix = [0u8; PREFIX_LEN];
    if let Err(err) = reader.read_exact(&mut prefix) {
        return Err(match err.kind() {
            io::ErrorKind::UnexpectedEof => FrameError::Closed,
            _ => FrameError::Io(err),
        });
    }
    let lengths = decode_prefix(&prefix)?;
    let mut body = vec![0u8; lengths.total()];
    reader.read_exact(&mut body)?;
    decode_body(lengths, body)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Hello {
        name: String,
    }

    #[test]
    fn frames_round_trip() {
        let hello = Hello {
            name: "agent".into(),
        };
        let mut wire = Vec::new();
        write_frame(&mut wire, &hello, b"\x00binary\xff").unwrap();
        write_frame(&mut wire, &hello, b"").unwrap();
        let mut reader = wire.as_slice();
        let (first, payload): (Hello, _) = read_frame(&mut reader).unwrap();
        assert_eq!(first, hello);
        assert_eq!(payload, b"\x00binary\xff");
        let (_, payload): (Hello, _) = read_frame(&mut reader).unwrap();
        assert!(payload.is_empty());
        assert!(matches!(
            read_frame::<_, Hello>(&mut reader),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn oversized_and_malformed_frames_are_rejected() {
        let mut prefix = [0u8; PREFIX_LEN];
        prefix[..4].copy_from_slice(&(MAX_HEADER_LEN as u32 + 1).to_le_bytes());
        assert!(matches!(
            decode_prefix(&prefix),
            Err(FrameError::HeaderTooLarge(_))
        ));
        prefix[..4].copy_from_slice(&0u32.to_le_bytes());
        prefix[4..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode_prefix(&prefix),
            Err(FrameError::PayloadTooLarge(_))
        ));
        let mut garbage = Vec::new();
        garbage.extend_from_slice(&3u32.to_le_bytes());
        garbage.extend_from_slice(&0u32.to_le_bytes());
        garbage.extend_from_slice(b"{{{");
        assert!(matches!(
            read_frame::<_, Hello>(&mut garbage.as_slice()),
            Err(FrameError::Malformed(_))
        ));
        let truncated = encode(&Hello { name: "x".into() }, b"abc").unwrap();
        assert!(matches!(
            read_frame::<_, Hello>(&mut &truncated[..truncated.len() - 1]),
            Err(FrameError::Io(_))
        ));
        assert!(matches!(
            encode(&Hello { name: "x".into() }, &vec![0; MAX_PAYLOAD_LEN + 1]),
            Err(FrameError::PayloadTooLarge(_))
        ));
    }
}
