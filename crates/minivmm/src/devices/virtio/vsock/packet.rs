pub const HEADER_LEN: usize = 44;
pub const HOST_CID: u64 = 2;
pub const TYPE_STREAM: u16 = 1;
pub const SHUTDOWN_RECEIVE: u32 = 1;
pub const SHUTDOWN_SEND: u32 = 2;
pub const SHUTDOWN_BOTH: u32 = SHUTDOWN_RECEIVE | SHUTDOWN_SEND;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Request,
    Response,
    Rst,
    Shutdown,
    Rw,
    CreditUpdate,
    CreditRequest,
}

impl Op {
    pub fn from_raw(raw: u16) -> Option<Self> {
        Some(match raw {
            1 => Op::Request,
            2 => Op::Response,
            3 => Op::Rst,
            4 => Op::Shutdown,
            5 => Op::Rw,
            6 => Op::CreditUpdate,
            7 => Op::CreditRequest,
            _ => return None,
        })
    }

    pub fn raw(self) -> u16 {
        match self {
            Op::Request => 1,
            Op::Response => 2,
            Op::Rst => 3,
            Op::Shutdown => 4,
            Op::Rw => 5,
            Op::CreditUpdate => 6,
            Op::CreditRequest => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub kind: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

fn u32_at(bytes: &[u8; HEADER_LEN], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn u64_at(bytes: &[u8; HEADER_LEN], offset: usize) -> u64 {
    u64::from(u32_at(bytes, offset)) | (u64::from(u32_at(bytes, offset + 4)) << 32)
}

impl Header {
    pub fn parse(bytes: &[u8; HEADER_LEN]) -> Self {
        Self {
            src_cid: u64_at(bytes, 0),
            dst_cid: u64_at(bytes, 8),
            src_port: u32_at(bytes, 16),
            dst_port: u32_at(bytes, 20),
            len: u32_at(bytes, 24),
            kind: u16::from_le_bytes([bytes[28], bytes[29]]),
            op: u16::from_le_bytes([bytes[30], bytes[31]]),
            flags: u32_at(bytes, 32),
            buf_alloc: u32_at(bytes, 36),
            fwd_cnt: u32_at(bytes, 40),
        }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0u8; HEADER_LEN];
        bytes[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.len.to_le_bytes());
        bytes[28..30].copy_from_slice(&self.kind.to_le_bytes());
        bytes[30..32].copy_from_slice(&self.op.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.flags.to_le_bytes());
        bytes[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        bytes[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        bytes
    }

    pub fn op(&self) -> Option<Op> {
        Op::from_raw(self.op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip_matches_the_wire_layout() {
        let header = Header {
            src_cid: 3,
            dst_cid: HOST_CID,
            src_port: 0x1234,
            dst_port: 52,
            len: 7,
            kind: TYPE_STREAM,
            op: Op::Rw.raw(),
            flags: SHUTDOWN_SEND,
            buf_alloc: 0x40000,
            fwd_cnt: 99,
        };
        let bytes = header.encode();
        assert_eq!(&bytes[0..8], &3u64.to_le_bytes());
        assert_eq!(&bytes[24..28], &7u32.to_le_bytes());
        assert_eq!(&bytes[30..32], &5u16.to_le_bytes());
        assert_eq!(&bytes[40..44], &99u32.to_le_bytes());
        assert_eq!(Header::parse(&bytes), header);
        assert_eq!(header.op(), Some(Op::Rw));
    }

    #[test]
    fn op_codes_round_trip() {
        for raw in 0..10 {
            if let Some(op) = Op::from_raw(raw) {
                assert_eq!(op.raw(), raw);
            } else {
                assert!(raw == 0 || raw > 7);
            }
        }
    }
}
