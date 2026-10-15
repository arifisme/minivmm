#![no_main]

use libfuzzer_sys::fuzz_target;
use minivmm::boot::bzimage::{load_bzimage, parse_header};
use minivmm::boot::params::{ZeroPage, initrd_address, write_cmdline};
use minivmm::memory::{GuestAddress, GuestMemory};

const MEMORY_SIZE: usize = 0x400000;

fuzz_target!(|data: &[u8]| {
    let memory = GuestMemory::new(&[(GuestAddress(0), MEMORY_SIZE)]).expect("guest memory");
    if let Ok(header) = parse_header(data) {
        let page = ZeroPage::new(&header);
        let _ = page.write_to(&memory, GuestAddress(0x7000));
        let _ = write_cmdline(&memory, &header, "console=ttyS0");
    }
    if let Ok(kernel) = load_bzimage(&memory, data, MEMORY_SIZE as u64) {
        let _ = initrd_address(&kernel, data.len(), MEMORY_SIZE as u64);
    }
});
