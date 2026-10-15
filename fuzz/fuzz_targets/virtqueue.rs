#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use minivmm::devices::virtio::queue::Queue;
use minivmm::memory::GuestAddress;

use common::{Bytes, memory_with};

fuzz_target!(|data: &[u8]| {
    let mut input = Bytes::new(data);
    let mut queue = Queue::new(256);
    queue.size = 1 << (input.u8() % 9);
    let desc = u64::from(input.u16());
    let avail = u64::from(input.u16());
    let used = u64::from(input.u16());
    let skip = input.u8();
    let seed = input.rest();
    let span = (seed.len() as u64).max(64);
    queue.desc_table = GuestAddress((desc % span) & !0xf);
    queue.avail_ring = GuestAddress((avail % span) & !0x1);
    queue.used_ring = GuestAddress(0x10000 + ((used % span) & !0x3));
    let memory = memory_with(seed);
    if !queue.is_valid(&memory) {
        return;
    }
    queue.ready = true;
    for _ in 0..skip % 4 {
        let _ = queue.pop(&memory);
    }
    let mut scratch = vec![0u8; 512];
    for _ in 0..64 {
        match queue.pop(&memory) {
            Ok(Some(chain)) => {
                let mut reader = chain.reader();
                let _ = reader.remaining();
                let _ = reader.read(&memory, &mut scratch);
                let mut writer = chain.writer();
                writer.skip(3);
                let _ = writer.write(&memory, &scratch);
                let _ = queue.add_used(&memory, chain.head, 7);
                let _ = queue.needs_notification(&memory);
            }
            Ok(None) | Err(_) => break,
        }
    }
});
