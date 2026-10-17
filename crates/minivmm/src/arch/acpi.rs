use super::aml;
use super::layout::{
    ACPI_PM_CNT_LEN, ACPI_PM_EVT_LEN, ACPI_PM_PORT, ACPI_TABLES_END, ACPI_TABLES_START,
    IOAPIC_ADDRESS, LAPIC_ADDRESS, PCI_CONFIG_PORT, PCI_CONFIG_PORT_LEN, PCI_MMIO_END,
    PCI_MMIO_START, SCI_IRQ,
};
use crate::memory::{GuestAddress, GuestMemory, MemoryError};

pub const SLEEP_TYPE_S5: u8 = 5;

const OEM_ID: &[u8; 6] = b"MINVMM";
const OEM_TABLE_ID: &[u8; 8] = b"MINIVMM ";
const CREATOR_ID: &[u8; 4] = b"MVMM";
const SDT_HEADER_LEN: usize = 36;
const RSDP_LEN: usize = 36;
const FADT_LEN: usize = 276;
const FACS_LEN: usize = 64;
const FADT_REVISION: u8 = 6;
const FADT_MINOR_REVISION: u8 = 5;
const MADT_REVISION: u8 = 5;

const FADT_FLAG_WBINVD: u32 = 1 << 0;
const FADT_FLAG_POWER_BUTTON_IS_CONTROL_METHOD: u32 = 1 << 4;
const FADT_FLAG_SLEEP_BUTTON_IS_CONTROL_METHOD: u32 = 1 << 5;
const BOOT_ARCH_LEGACY_DEVICES: u16 = 1 << 0;
const C2_DISABLED_LATENCY: u16 = 101;
const C3_DISABLED_LATENCY: u16 = 1001;
const GAS_SYSTEM_IO: u8 = 1;

const MADT_PCAT_COMPAT: u32 = 1;
const MADT_LOCAL_APIC: u8 = 0;
const MADT_IO_APIC: u8 = 1;
const MADT_LOCAL_APIC_NMI: u8 = 4;
const LOCAL_APIC_ENABLED: u32 = 1;
const ALL_PROCESSORS: u8 = 0xff;

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, b| sum.wrapping_sub(*b))
}

struct Sdt {
    bytes: Vec<u8>,
}

impl Sdt {
    fn new(signature: &[u8; 4], revision: u8) -> Self {
        let mut bytes = vec![0u8; SDT_HEADER_LEN];
        bytes[0..4].copy_from_slice(signature);
        bytes[8] = revision;
        bytes[10..16].copy_from_slice(OEM_ID);
        bytes[16..24].copy_from_slice(OEM_TABLE_ID);
        bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
        bytes[28..32].copy_from_slice(CREATOR_ID);
        bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
        Self { bytes }
    }

    fn with_len(signature: &[u8; 4], revision: u8, len: usize) -> Self {
        let mut sdt = Self::new(signature, revision);
        sdt.bytes.resize(len, 0);
        sdt
    }

    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
    }

    fn put(&mut self, offset: usize, data: &[u8]) {
        self.bytes[offset..offset + data.len()].copy_from_slice(data);
    }

    fn finish(mut self) -> Vec<u8> {
        let len = self.bytes.len() as u32;
        self.bytes[4..8].copy_from_slice(&len.to_le_bytes());
        self.bytes[9] = 0;
        self.bytes[9] = checksum(&self.bytes);
        self.bytes
    }
}

fn io_register(port: u16, bit_width: u8) -> [u8; 12] {
    let mut gas = [0u8; 12];
    gas[0] = GAS_SYSTEM_IO;
    gas[1] = bit_width;
    gas[4..12].copy_from_slice(&u64::from(port).to_le_bytes());
    gas
}

const WORD_ADDRESS_DESCRIPTOR: u8 = 0x88;
const DWORD_ADDRESS_DESCRIPTOR: u8 = 0x87;
const END_TAG: [u8; 2] = [0x79, 0x00];
const RESOURCE_MEMORY: u8 = 0;
const RESOURCE_IO: u8 = 1;
const RESOURCE_BUS_NUMBER: u8 = 2;
const PRODUCER_FIXED_RANGE: u8 = 0x0c;
const IO_ENTIRE_RANGE: u8 = 0x03;
const MEMORY_READ_WRITE: u8 = 0x01;

fn word_address(resource: u8, type_flags: u8, min: u16, max: u16) -> Vec<u8> {
    let mut descriptor = vec![
        WORD_ADDRESS_DESCRIPTOR,
        0x0d,
        0x00,
        resource,
        PRODUCER_FIXED_RANGE,
        type_flags,
    ];
    for field in [0, min, max, 0, max - min + 1] {
        descriptor.extend_from_slice(&field.to_le_bytes());
    }
    descriptor
}

fn dword_memory(min: u32, max: u32) -> Vec<u8> {
    let mut descriptor = vec![
        DWORD_ADDRESS_DESCRIPTOR,
        0x17,
        0x00,
        RESOURCE_MEMORY,
        PRODUCER_FIXED_RANGE,
        MEMORY_READ_WRITE,
    ];
    for field in [0, min, max, 0, max - min + 1] {
        descriptor.extend_from_slice(&field.to_le_bytes());
    }
    descriptor
}

fn pci_host_bridge_resources() -> Vec<u8> {
    let config_start = PCI_CONFIG_PORT as u16;
    let config_end = config_start + PCI_CONFIG_PORT_LEN as u16;
    [
        word_address(RESOURCE_BUS_NUMBER, 0, 0, 0xff),
        word_address(RESOURCE_IO, IO_ENTIRE_RANGE, 0, config_start - 1),
        word_address(RESOURCE_IO, IO_ENTIRE_RANGE, config_end, 0xffff),
        dword_memory(PCI_MMIO_START as u32, (PCI_MMIO_END - 1) as u32),
        END_TAG.to_vec(),
    ]
    .concat()
}

fn dsdt() -> Vec<u8> {
    let mut sdt = Sdt::new(b"DSDT", 2);
    let sleep_type = aml::integer(SLEEP_TYPE_S5.into());
    sdt.push(&aml::name(
        "_S5",
        aml::package(&[
            sleep_type.clone(),
            sleep_type,
            aml::integer(0),
            aml::integer(0),
        ]),
    ));
    sdt.push(&aml::device(
        "\\_SB.PCI0",
        &[
            aml::name("_HID", aml::eisa_id("PNP0A03")),
            aml::name("_UID", aml::integer(0)),
            aml::name("_CRS", aml::buffer(&pci_host_bridge_resources())),
        ],
    ));
    sdt.finish()
}

fn facs() -> Vec<u8> {
    let mut bytes = vec![0u8; FACS_LEN];
    bytes[0..4].copy_from_slice(b"FACS");
    bytes[4..8].copy_from_slice(&(FACS_LEN as u32).to_le_bytes());
    bytes[32] = 2;
    bytes
}

fn fadt(dsdt: GuestAddress, facs: GuestAddress) -> Vec<u8> {
    let evt_port = ACPI_PM_PORT;
    let cnt_port = ACPI_PM_PORT + u16::from(ACPI_PM_EVT_LEN);
    let mut sdt = Sdt::with_len(b"FACP", FADT_REVISION, FADT_LEN);
    sdt.put(36, &(facs.raw() as u32).to_le_bytes());
    sdt.put(40, &(dsdt.raw() as u32).to_le_bytes());
    sdt.put(46, &SCI_IRQ.to_le_bytes());
    sdt.put(56, &u32::from(evt_port).to_le_bytes());
    sdt.put(64, &u32::from(cnt_port).to_le_bytes());
    sdt.put(88, &[ACPI_PM_EVT_LEN, ACPI_PM_CNT_LEN]);
    sdt.put(96, &C2_DISABLED_LATENCY.to_le_bytes());
    sdt.put(98, &C3_DISABLED_LATENCY.to_le_bytes());
    sdt.put(109, &BOOT_ARCH_LEGACY_DEVICES.to_le_bytes());
    let flags = FADT_FLAG_WBINVD
        | FADT_FLAG_POWER_BUTTON_IS_CONTROL_METHOD
        | FADT_FLAG_SLEEP_BUTTON_IS_CONTROL_METHOD;
    sdt.put(112, &flags.to_le_bytes());
    sdt.put(131, &[FADT_MINOR_REVISION]);
    sdt.put(132, &facs.raw().to_le_bytes());
    sdt.put(140, &dsdt.raw().to_le_bytes());
    sdt.put(148, &io_register(evt_port, ACPI_PM_EVT_LEN * 8));
    sdt.put(172, &io_register(cnt_port, ACPI_PM_CNT_LEN * 8));
    sdt.put(268, b"MINIVMM\0");
    sdt.finish()
}

fn madt(cpu_count: u8) -> Vec<u8> {
    let mut sdt = Sdt::new(b"APIC", MADT_REVISION);
    sdt.push(&(LAPIC_ADDRESS as u32).to_le_bytes());
    sdt.push(&MADT_PCAT_COMPAT.to_le_bytes());
    for cpu in 0..cpu_count {
        sdt.push(&[MADT_LOCAL_APIC, 8, cpu, cpu]);
        sdt.push(&LOCAL_APIC_ENABLED.to_le_bytes());
    }
    sdt.push(&[MADT_IO_APIC, 12, cpu_count, 0]);
    sdt.push(&(IOAPIC_ADDRESS as u32).to_le_bytes());
    sdt.push(&0u32.to_le_bytes());
    sdt.push(&[MADT_LOCAL_APIC_NMI, 6, ALL_PROCESSORS, 0, 0, 1]);
    sdt.finish()
}

fn xsdt(tables: &[GuestAddress]) -> Vec<u8> {
    let mut sdt = Sdt::new(b"XSDT", 1);
    for table in tables {
        sdt.push(&table.raw().to_le_bytes());
    }
    sdt.finish()
}

fn rsdp(xsdt: GuestAddress) -> Vec<u8> {
    let mut bytes = vec![0u8; RSDP_LEN];
    bytes[0..8].copy_from_slice(b"RSD PTR ");
    bytes[9..15].copy_from_slice(OEM_ID);
    bytes[15] = 2;
    bytes[20..24].copy_from_slice(&(RSDP_LEN as u32).to_le_bytes());
    bytes[24..32].copy_from_slice(&xsdt.raw().to_le_bytes());
    bytes[8] = checksum(&bytes[..20]);
    bytes[32] = checksum(&bytes);
    bytes
}

struct TableWriter<'a> {
    mem: &'a GuestMemory,
    next: u64,
}

impl TableWriter<'_> {
    fn place(&mut self, bytes: &[u8], align: u64) -> Result<GuestAddress, MemoryError> {
        let addr = self.next.next_multiple_of(align);
        let end = addr + bytes.len() as u64;
        if end > ACPI_TABLES_END.raw() {
            return Err(MemoryError::OutOfRange {
                addr,
                len: bytes.len(),
            });
        }
        self.mem.write(GuestAddress(addr), bytes)?;
        self.next = end;
        Ok(GuestAddress(addr))
    }
}

pub fn write_acpi_tables(mem: &GuestMemory, cpu_count: u8) -> Result<GuestAddress, MemoryError> {
    let rsdp_addr = ACPI_TABLES_START;
    let mut writer = TableWriter {
        mem,
        next: rsdp_addr.raw() + RSDP_LEN as u64,
    };
    let facs = writer.place(&facs(), 64)?;
    let dsdt = writer.place(&dsdt(), 8)?;
    let fadt = writer.place(&fadt(dsdt, facs), 8)?;
    let madt = writer.place(&madt(cpu_count), 8)?;
    let xsdt = writer.place(&xsdt(&[fadt, madt]), 8)?;
    mem.write(rsdp_addr, &rsdp(xsdt))?;
    Ok(rsdp_addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_table(mem: &GuestMemory, addr: u64) -> Vec<u8> {
        let len: u32 = mem.read_obj(GuestAddress(addr + 4)).unwrap();
        let mut bytes = vec![0u8; len as usize];
        mem.read(GuestAddress(addr), &mut bytes).unwrap();
        assert_eq!(checksum(&bytes), 0, "bad checksum");
        bytes
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    #[test]
    fn host_bridge_windows() {
        let resources = pci_host_bridge_resources();
        assert_eq!(resources.len(), 3 * 16 + 26 + 2);
        let memory = &resources[48..74];
        assert_eq!(u32_at(memory, 10), 0xc000_0000);
        assert_eq!(u32_at(memory, 14), 0xfebf_ffff);
        assert_eq!(u32_at(memory, 22), 0x3ec0_0000);
        let io_low = &resources[16..32];
        assert_eq!(u16::from_le_bytes([io_low[10], io_low[11]]), 0x0cf7);
        assert_eq!(&resources[74..], &END_TAG);
    }

    #[test]
    fn tables_are_linked_and_checksummed() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x100000)]).unwrap();
        let rsdp_addr = write_acpi_tables(&mem, 2).unwrap();
        let mut rsdp = [0u8; RSDP_LEN];
        mem.read(rsdp_addr, &mut rsdp).unwrap();
        assert_eq!(&rsdp[..8], b"RSD PTR ");
        assert_eq!(checksum(&rsdp[..20]), 0);
        assert_eq!(checksum(&rsdp), 0);

        let xsdt = read_table(&mem, u64_at(&rsdp, 24));
        assert_eq!(&xsdt[..4], b"XSDT");
        let fadt = read_table(&mem, u64_at(&xsdt, 36));
        let madt = read_table(&mem, u64_at(&xsdt, 44));
        assert_eq!(&fadt[..4], b"FACP");
        assert_eq!(fadt.len(), FADT_LEN);
        assert_eq!(u32_at(&fadt, 64), 0x604);

        let dsdt = read_table(&mem, u64_at(&fadt, 140));
        assert_eq!(&dsdt[..4], b"DSDT");
        assert!(dsdt.windows(4).any(|w| w == b"_S5_"));
        assert!(dsdt.windows(4).any(|w| w == b"_CRS"));
        let facs_addr = u64_at(&fadt, 132);
        assert_eq!(facs_addr % 64, 0);
        let mut facs = [0u8; 4];
        mem.read(GuestAddress(facs_addr), &mut facs).unwrap();
        assert_eq!(&facs, b"FACS");

        assert_eq!(&madt[..4], b"APIC");
        let lapics: Vec<_> = madt[44..]
            .chunks(8)
            .take_while(|e| e[0] == MADT_LOCAL_APIC)
            .map(|e| e[3])
            .collect();
        assert_eq!(lapics, vec![0, 1]);
    }
}
