use kvm_bindings::kvm_segment;

pub const CODE64_FLAGS: u16 = 0xa09b;
pub const DATA_FLAGS: u16 = 0xc093;
pub const TSS_FLAGS: u16 = 0x808b;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentDescriptor(pub u64);

impl SegmentDescriptor {
    pub const NULL: Self = Self(0);

    pub const fn new(flags: u16, base: u32, limit: u32) -> Self {
        let base = base as u64;
        let limit = limit as u64;
        let flags = flags as u64;
        Self(
            ((base & 0xff00_0000) << 32)
                | ((flags & 0xf0ff) << 40)
                | ((limit & 0x000f_0000) << 32)
                | ((base & 0x00ff_ffff) << 16)
                | (limit & 0x0000_ffff),
        )
    }

    fn base(self) -> u64 {
        ((self.0 >> 16) & 0x00ff_ffff) | ((self.0 >> 32) & 0xff00_0000)
    }

    fn limit(self) -> u32 {
        let raw = ((self.0 & 0xffff) | ((self.0 >> 32) & 0x000f_0000)) as u32;
        if self.bit(55) == 1 {
            (raw << 12) | 0xfff
        } else {
            raw
        }
    }

    fn bit(self, n: u32) -> u8 {
        ((self.0 >> n) & 1) as u8
    }

    pub fn to_kvm_segment(self, index: u16) -> kvm_segment {
        let present = self.bit(47);
        kvm_segment {
            base: self.base(),
            limit: self.limit(),
            selector: index * 8,
            type_: ((self.0 >> 40) & 0xf) as u8,
            present,
            dpl: ((self.0 >> 45) & 0x3) as u8,
            db: self.bit(54),
            s: self.bit(44),
            l: self.bit(53),
            g: self.bit(55),
            avl: self.bit(52),
            unusable: u8::from(present == 0),
            padding: 0,
        }
    }
}

pub const BOOT_CODE_INDEX: u16 = 2;
pub const BOOT_DATA_INDEX: u16 = 3;
pub const BOOT_TSS_INDEX: u16 = 4;

pub fn boot_gdt() -> [SegmentDescriptor; 5] {
    [
        SegmentDescriptor::NULL,
        SegmentDescriptor::NULL,
        SegmentDescriptor::new(CODE64_FLAGS, 0, 0xfffff),
        SegmentDescriptor::new(DATA_FLAGS, 0, 0xfffff),
        SegmentDescriptor::new(TSS_FLAGS, 0, 0xfffff),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_flat_segments() {
        assert_eq!(
            SegmentDescriptor::new(CODE64_FLAGS, 0, 0xfffff).0,
            0x00af_9b00_0000_ffff
        );
        assert_eq!(
            SegmentDescriptor::new(DATA_FLAGS, 0, 0xfffff).0,
            0x00cf_9300_0000_ffff
        );
        assert_eq!(
            SegmentDescriptor::new(TSS_FLAGS, 0, 0xfffff).0,
            0x008f_8b00_0000_ffff
        );
    }

    #[test]
    fn encodes_base_and_limit() {
        let desc = SegmentDescriptor::new(0x4093, 0x1234_5678, 0xabcde);
        let seg = desc.to_kvm_segment(3);
        assert_eq!(seg.base, 0x1234_5678);
        assert_eq!(seg.limit, 0xabcde);
        assert_eq!(seg.selector, 0x18);
        assert_eq!(seg.db, 1);
        assert_eq!(seg.g, 0);
    }

    #[test]
    fn converts_code_segment() {
        let seg = boot_gdt()[BOOT_CODE_INDEX as usize].to_kvm_segment(BOOT_CODE_INDEX);
        assert_eq!(seg.selector, 0x10);
        assert_eq!(seg.limit, 0xffff_ffff);
        assert_eq!(
            (seg.type_, seg.present, seg.s, seg.l, seg.g),
            (0xb, 1, 1, 1, 1)
        );
        assert_eq!((seg.db, seg.dpl, seg.unusable), (0, 0, 0));
        let null = SegmentDescriptor::NULL.to_kvm_segment(0);
        assert_eq!(null.unusable, 1);
    }
}
