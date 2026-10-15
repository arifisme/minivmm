const ZERO_OP: u8 = 0x00;
const ONE_OP: u8 = 0x01;
const NAME_OP: u8 = 0x08;
const BYTE_PREFIX: u8 = 0x0a;
const WORD_PREFIX: u8 = 0x0b;
const DWORD_PREFIX: u8 = 0x0c;
const QWORD_PREFIX: u8 = 0x0e;
const BUFFER_OP: u8 = 0x11;
const PACKAGE_OP: u8 = 0x12;
const EXT_OP_PREFIX: u8 = 0x5b;
const DEVICE_OP: u8 = 0x82;
const ROOT_CHAR: u8 = b'\\';
const DUAL_NAME_PREFIX: u8 = 0x2e;
const MULTI_NAME_PREFIX: u8 = 0x2f;

pub fn integer(value: u64) -> Vec<u8> {
    match value {
        0 => vec![ZERO_OP],
        1 => vec![ONE_OP],
        v if v <= u8::MAX.into() => vec![BYTE_PREFIX, v as u8],
        v if v <= u16::MAX.into() => [&[WORD_PREFIX][..], &(v as u16).to_le_bytes()].concat(),
        v if v <= u32::MAX.into() => [&[DWORD_PREFIX][..], &(v as u32).to_le_bytes()].concat(),
        v => [&[QWORD_PREFIX][..], &v.to_le_bytes()].concat(),
    }
}

pub fn eisa_id(id: &str) -> Vec<u8> {
    let bytes = id.as_bytes();
    assert!(bytes.len() == 7, "EISA ids have seven characters");
    let letter = |c: u8| u32::from(c - b'@') & 0x1f;
    let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0);
    let compressed = (letter(bytes[0]) << 10) | (letter(bytes[1]) << 5) | letter(bytes[2]);
    let product = bytes[3..].iter().fold(0u32, |acc, &c| (acc << 4) | hex(c));
    let value = (compressed << 16) | product;
    [&[DWORD_PREFIX][..], &value.to_be_bytes()].concat()
}

fn name_segment(segment: &str) -> [u8; 4] {
    let mut out = [b'_'; 4];
    for (dst, src) in out.iter_mut().zip(segment.bytes()) {
        *dst = src;
    }
    out
}

pub fn name_string(path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let relative = match path.strip_prefix('\\') {
        Some(rest) => {
            out.push(ROOT_CHAR);
            rest
        }
        None => path,
    };
    let segments: Vec<&str> = relative.split('.').filter(|s| !s.is_empty()).collect();
    match segments.len() {
        0 => out.push(ZERO_OP),
        1 => {}
        2 => out.push(DUAL_NAME_PREFIX),
        n => {
            out.push(MULTI_NAME_PREFIX);
            out.push(n as u8);
        }
    }
    for segment in segments {
        out.extend_from_slice(&name_segment(segment));
    }
    out
}

pub fn pkg_length(payload_len: usize) -> Vec<u8> {
    if payload_len + 1 < 0x40 {
        return vec![(payload_len + 1) as u8];
    }
    for extra in 1..=3usize {
        let total = payload_len + 1 + extra;
        if total < 1 << (4 + 8 * extra) {
            let mut out = vec![((extra as u8) << 6) | (total & 0xf) as u8];
            for i in 0..extra {
                out.push((total >> (4 + 8 * i)) as u8);
            }
            return out;
        }
    }
    panic!("AML package too large");
}

pub fn name(path: &str, value: Vec<u8>) -> Vec<u8> {
    [vec![NAME_OP], name_string(path), value].concat()
}

pub fn package(elements: &[Vec<u8>]) -> Vec<u8> {
    let mut body = vec![elements.len() as u8];
    for element in elements {
        body.extend_from_slice(element);
    }
    [vec![PACKAGE_OP], pkg_length(body.len()), body].concat()
}

pub fn buffer(bytes: &[u8]) -> Vec<u8> {
    let body = [integer(bytes.len() as u64), bytes.to_vec()].concat();
    [vec![BUFFER_OP], pkg_length(body.len()), body].concat()
}

pub fn device(path: &str, children: &[Vec<u8>]) -> Vec<u8> {
    let mut body = name_string(path);
    for child in children {
        body.extend_from_slice(child);
    }
    [vec![EXT_OP_PREFIX, DEVICE_OP], pkg_length(body.len()), body].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_integers() {
        assert_eq!(integer(0), [0x00]);
        assert_eq!(integer(1), [0x01]);
        assert_eq!(integer(5), [0x0a, 0x05]);
        assert_eq!(integer(0x1234), [0x0b, 0x34, 0x12]);
        assert_eq!(integer(0x1234_5678), [0x0c, 0x78, 0x56, 0x34, 0x12]);
        assert_eq!(integer(1 << 40)[0], 0x0e);
    }

    #[test]
    fn encodes_eisa_id() {
        assert_eq!(eisa_id("PNP0A03"), [0x0c, 0x41, 0xd0, 0x0a, 0x03]);
        assert_eq!(eisa_id("PNP0501"), [0x0c, 0x41, 0xd0, 0x05, 0x01]);
    }

    #[test]
    fn encodes_names() {
        assert_eq!(name_string("_S5"), b"_S5_");
        assert_eq!(name_string("\\_SB.PCI0"), b"\\\x2e_SB_PCI0");
        assert_eq!(name_string("\\A.B.C"), b"\\\x2f\x03A___B___C___");
    }

    #[test]
    fn encodes_package_lengths() {
        assert_eq!(pkg_length(7), [0x08]);
        assert_eq!(pkg_length(62), [0x3f]);
        assert_eq!(pkg_length(63), [0x41, 0x04]);
        assert_eq!(pkg_length(0x1000), [0x83, 0x00, 0x01]);
    }

    #[test]
    fn encodes_sleep_package() {
        let s5 = name(
            "_S5",
            package(&[integer(5), integer(5), integer(0), integer(0)]),
        );
        assert_eq!(
            s5,
            [
                0x08, b'_', b'S', b'5', b'_', 0x12, 0x08, 0x04, 0x0a, 0x05, 0x0a, 0x05, 0x00, 0x00
            ]
        );
    }

    #[test]
    fn encodes_buffer() {
        assert_eq!(buffer(&[0x79, 0]), [0x11, 0x04, 0x0a, 0x02, 0x79, 0x00]);
    }

    #[test]
    fn encodes_device() {
        let dev = device("\\_SB.PCI0", &[name("_HID", eisa_id("PNP0A03"))]);
        assert_eq!(&dev[..3], &[0x5b, 0x82, 21]);
        assert_eq!(dev.len(), 3 + 20);
    }
}
