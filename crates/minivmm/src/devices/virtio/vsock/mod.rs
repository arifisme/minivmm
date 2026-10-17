pub mod connection;
pub mod host;
pub mod packet;

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::os::fd::RawFd;

use log::debug;
use serde::{Deserialize, Serialize};

use super::queue::{ChainCursor, DescriptorChain, Queue, QueueError};
use super::{QueueMask, TYPE_VSOCK, VirtioDevice, queue_bit, read_config_bytes};
use crate::memory::GuestMemory;
use connection::{BUFFER_SIZE, Connection, State, VsockStream};
use packet::{HEADER_LEN, HOST_CID, Header, Op, SHUTDOWN_BOTH, TYPE_STREAM};

pub const QUEUE_SIZE: u16 = 256;
pub const RX_QUEUE: usize = 0;
pub const TX_QUEUE: usize = 1;
pub const EVENT_QUEUE: usize = 2;
const MAX_PAYLOAD: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_PENDING_CONTROL: usize = 256;
const LOCAL_PORT_BASE: u32 = 1 << 30;
const EVENT_TRANSPORT_RESET: u32 = 0;

pub trait VsockBackend: Send {
    fn connect(&mut self, port: u32) -> io::Result<Box<dyn VsockStream>>;
}

/// Host ports keep counting up across snapshots: a restored guest may still hold sockets that
/// are closing towards ports used before the snapshot, and reusing one would get the new
/// connection reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VsockState {
    pub next_local_port: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interest {
    pub id: u32,
    pub fd: RawFd,
    pub read: bool,
    pub write: bool,
}

/// Progress of the transport reset sent after a restore. The guest re-posts the event buffer
/// only after it has dropped every stale connection, so host connections are held back until
/// then; otherwise the reset can race with a new connection and silently kill it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportReset {
    Idle,
    Pending,
    Delivered { avail_index: u16 },
}

pub struct Vsock {
    cid: u64,
    backend: Box<dyn VsockBackend>,
    connections: BTreeMap<u32, Connection>,
    next_id: u32,
    next_local_port: u32,
    control: VecDeque<Header>,
    reset: TransportReset,
    deferred_requests: Vec<u32>,
    cursor: u32,
}

impl Vsock {
    pub fn new(cid: u64, backend: Box<dyn VsockBackend>) -> Self {
        Self {
            cid,
            backend,
            connections: BTreeMap::new(),
            next_id: 0,
            next_local_port: LOCAL_PORT_BASE,
            control: VecDeque::new(),
            reset: TransportReset::Idle,
            deferred_requests: Vec::new(),
            cursor: 0,
        }
    }

    pub fn cid(&self) -> u64 {
        self.cid
    }

    pub fn interests(&self) -> Vec<Interest> {
        self.connections
            .iter()
            .filter_map(|(id, conn)| {
                let read = conn.wants_to_read();
                let write = conn.wants_to_write();
                let fd = conn.raw_fd()?;
                (read || write).then_some(Interest {
                    id: *id,
                    fd,
                    read,
                    write,
                })
            })
            .collect()
    }

    /// Starts a connection requested by a host process to `port` in the guest. `early_data`
    /// holds bytes the host already sent behind its request line.
    pub fn connect_from_host(
        &mut self,
        stream: Box<dyn VsockStream>,
        port: u32,
        early_data: Vec<u8>,
    ) -> bool {
        if self.connections.len() >= MAX_CONNECTIONS {
            return false;
        }
        let host_port = self.allocate_local_port();
        let conn = Connection::new(stream, host_port, port, State::Connecting, early_data);
        self.insert(conn);
        if self.reset == TransportReset::Idle {
            self.queue_control(Op::Request, host_port, port, 0, 0);
        } else {
            self.deferred_requests.push(host_port);
        }
        true
    }

    pub fn host_ready(&mut self, id: u32) {
        let Some(conn) = self.connections.get_mut(&id) else {
            return;
        };
        if conn.is_established() && !conn.host_eof {
            conn.readable = true;
        }
        let flushed = conn.flush();
        self.after_host_write(id, flushed);
    }

    pub fn fill_rx(
        &mut self,
        mem: &GuestMemory,
        queues: &mut [Queue],
    ) -> Result<QueueMask, QueueError> {
        let Some(queue) = queues.get_mut(RX_QUEUE) else {
            return Ok(0);
        };
        let mut mask = 0;
        loop {
            let next = match self.control.front() {
                Some(_) => None,
                None => match self.next_sendable() {
                    Some(id) => Some(id),
                    None => break,
                },
            };
            let Some(chain) = queue.pop(mem)? else {
                break;
            };
            let mut writer = chain.writer();
            let room = writer.remaining();
            let needed = if next.is_some() {
                HEADER_LEN + 1
            } else {
                HEADER_LEN
            };
            if room < needed as u64 {
                debug!("dropping a vsock receive buffer of {room} bytes");
                queue.add_used(mem, chain.head, 0)?;
                mask = queue_bit(RX_QUEUE);
                continue;
            }
            let written = match next {
                None => match self.control.pop_front() {
                    Some(header) => writer.write(mem, &header.encode())?,
                    None => 0,
                },
                Some(id) => {
                    let room = (room as usize - HEADER_LEN).min(MAX_PAYLOAD);
                    match self.data_packet(id, mem, &mut writer, room)? {
                        Some(written) => written,
                        None => {
                            queue.undo_pop();
                            continue;
                        }
                    }
                }
            };
            queue.add_used(mem, chain.head, written as u32)?;
            mask = queue_bit(RX_QUEUE);
        }
        Ok(mask)
    }

    fn allocate_local_port(&mut self) -> u32 {
        loop {
            let port = self.next_local_port;
            self.next_local_port = self
                .next_local_port
                .checked_add(1)
                .unwrap_or(LOCAL_PORT_BASE);
            if !self.connections.values().any(|c| c.host_port == port) {
                return port;
            }
        }
    }

    fn insert(&mut self, conn: Connection) {
        while self.connections.contains_key(&self.next_id) {
            self.next_id = self.next_id.wrapping_add(1);
        }
        self.connections.insert(self.next_id, conn);
        self.next_id = self.next_id.wrapping_add(1);
    }

    fn find(&self, host_port: u32, guest_port: u32) -> Option<u32> {
        self.connections
            .iter()
            .find(|(_, c)| c.host_port == host_port && c.guest_port == guest_port)
            .map(|(id, _)| *id)
    }

    fn header(&self, op: Op, host_port: u32, guest_port: u32, flags: u32, fwd_cnt: u32) -> Header {
        Header {
            src_cid: HOST_CID,
            dst_cid: self.cid,
            src_port: host_port,
            dst_port: guest_port,
            len: 0,
            kind: TYPE_STREAM,
            op: op.raw(),
            flags,
            buf_alloc: BUFFER_SIZE,
            fwd_cnt,
        }
    }

    fn queue_control(&mut self, op: Op, host_port: u32, guest_port: u32, flags: u32, fwd_cnt: u32) {
        let header = self.header(op, host_port, guest_port, flags, fwd_cnt);
        self.control.push_back(header);
    }

    fn reset_connection(&mut self, id: u32) {
        if let Some(conn) = self.connections.remove(&id) {
            self.queue_control(Op::Rst, conn.host_port, conn.guest_port, 0, 0);
        }
    }

    fn queue_credit_update(&mut self, id: u32) {
        if let Some(conn) = self.connections.get_mut(&id) {
            let fwd_cnt = conn.advertise_fwd_cnt();
            let (host_port, guest_port) = (conn.host_port, conn.guest_port);
            self.queue_control(Op::CreditUpdate, host_port, guest_port, 0, fwd_cnt);
        }
    }

    fn after_host_write(&mut self, id: u32, result: io::Result<()>) {
        let Some(conn) = self.connections.get_mut(&id) else {
            return;
        };
        let closed = result.and_then(|()| conn.settle());
        match closed {
            Ok(true) => self.reset_connection(id),
            Ok(false) => {
                if conn.wants_credit_update() {
                    self.queue_credit_update(id);
                }
            }
            Err(err) => {
                debug!("vsock host stream failed: {err}");
                self.reset_connection(id);
            }
        }
    }

    fn next_sendable(&mut self) -> Option<u32> {
        let after = self
            .connections
            .range(self.cursor..)
            .chain(self.connections.range(..self.cursor))
            .find(|(_, conn)| conn.has_data_for_guest())
            .map(|(id, _)| *id)?;
        self.cursor = after.wrapping_add(1);
        Some(after)
    }

    fn data_packet(
        &mut self,
        id: u32,
        mem: &GuestMemory,
        writer: &mut ChainCursor,
        room: usize,
    ) -> Result<Option<usize>, QueueError> {
        let Some(conn) = self.connections.get_mut(&id) else {
            return Ok(None);
        };
        let mut buf = vec![0u8; room.min(conn.peer_credit() as usize)];
        let header = match conn.read_for_guest(&mut buf) {
            Ok(0) => {
                conn.host_eof = true;
                conn.readable = false;
                let fwd_cnt = conn.advertise_fwd_cnt();
                let (host_port, guest_port) = (conn.host_port, conn.guest_port);
                let header =
                    self.header(Op::Shutdown, host_port, guest_port, SHUTDOWN_BOTH, fwd_cnt);
                self.after_host_write(id, Ok(()));
                header
            }
            Ok(len) => {
                buf.truncate(len);
                conn.sent_to_guest(len);
                let fwd_cnt = conn.advertise_fwd_cnt();
                let (host_port, guest_port) = (conn.host_port, conn.guest_port);
                Header {
                    len: len as u32,
                    ..self.header(Op::Rw, host_port, guest_port, 0, fwd_cnt)
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                conn.readable = false;
                return Ok(None);
            }
            Err(err) => {
                debug!("vsock host stream read failed: {err}");
                let (host_port, guest_port) = (conn.host_port, conn.guest_port);
                self.connections.remove(&id);
                self.header(Op::Rst, host_port, guest_port, 0, 0)
            }
        };
        let mut written = writer.write(mem, &header.encode())?;
        if header.len > 0 {
            written += writer.write(mem, &buf)?;
        }
        Ok(Some(written))
    }

    fn process_tx(
        &mut self,
        mem: &GuestMemory,
        queues: &mut [Queue],
    ) -> Result<QueueMask, QueueError> {
        let mut mask = 0;
        while self.control.len() < MAX_PENDING_CONTROL {
            let Some(queue) = queues.get_mut(TX_QUEUE) else {
                break;
            };
            let Some(chain) = queue.pop(mem)? else {
                break;
            };
            let head = chain.head;
            self.handle_packet(mem, &chain)?;
            if let Some(queue) = queues.get_mut(TX_QUEUE) {
                queue.add_used(mem, head, 0)?;
            }
            mask = queue_bit(TX_QUEUE);
        }
        Ok(mask)
    }

    fn handle_packet(
        &mut self,
        mem: &GuestMemory,
        chain: &DescriptorChain,
    ) -> Result<(), QueueError> {
        let mut reader = chain.reader();
        let mut raw = [0u8; HEADER_LEN];
        if reader.read(mem, &mut raw)? != HEADER_LEN {
            debug!("dropping a truncated vsock packet");
            return Ok(());
        }
        let header = Header::parse(&raw);
        if header.src_cid != self.cid || header.dst_cid != HOST_CID {
            debug!(
                "dropping a vsock packet from {} to {}",
                header.src_cid, header.dst_cid
            );
            return Ok(());
        }
        let len = header.len as usize;
        let op = header.op();
        let malformed = header.kind != TYPE_STREAM
            || op.is_none()
            || len > MAX_PAYLOAD
            || reader.remaining() < len as u64
            || (len > 0 && op != Some(Op::Rw));
        if malformed {
            self.reply_rst(&header);
            return Ok(());
        }
        let mut payload = vec![0u8; len];
        reader.read(mem, &mut payload)?;
        let Some(op) = op else {
            return Ok(());
        };
        match self.find(header.dst_port, header.src_port) {
            Some(id) => self.connection_packet(id, op, &header, &payload),
            None if op == Op::Request => self.guest_connect(&header),
            None if op == Op::Rst => {}
            None => self.reply_rst(&header),
        }
        Ok(())
    }

    fn reply_rst(&mut self, header: &Header) {
        if header.op() != Some(Op::Rst) {
            self.queue_control(Op::Rst, header.dst_port, header.src_port, 0, 0);
        }
    }

    fn guest_connect(&mut self, header: &Header) {
        if self.connections.len() >= MAX_CONNECTIONS {
            self.reply_rst(header);
            return;
        }
        match self.backend.connect(header.dst_port) {
            Ok(stream) => {
                let mut conn = Connection::new(
                    stream,
                    header.dst_port,
                    header.src_port,
                    State::Established,
                    Vec::new(),
                );
                conn.update_peer_credit(header);
                self.insert(conn);
                self.queue_control(Op::Response, header.dst_port, header.src_port, 0, 0);
            }
            Err(err) => {
                debug!(
                    "guest connection to host port {} failed: {err}",
                    header.dst_port
                );
                self.reply_rst(header);
            }
        }
    }

    fn connection_packet(&mut self, id: u32, op: Op, header: &Header, payload: &[u8]) {
        let Some(conn) = self.connections.get_mut(&id) else {
            return;
        };
        conn.update_peer_credit(header);
        match op {
            Op::Request => self.reset_connection(id),
            Op::Response => {
                if conn.state != State::Connecting || conn.greet_host().is_err() {
                    self.reset_connection(id);
                }
            }
            Op::Rst => {
                self.connections.remove(&id);
            }
            Op::Shutdown => {
                conn.peer_shutdown(header.flags);
                self.after_host_write(id, Ok(()));
            }
            Op::Rw if conn.is_established() => match conn.accept_from_guest(payload) {
                Ok(result) => self.after_host_write(id, result),
                Err(_) => {
                    debug!("guest exceeded its vsock credit");
                    self.reset_connection(id);
                }
            },
            Op::Rw => self.reset_connection(id),
            Op::CreditUpdate => {}
            Op::CreditRequest => self.queue_credit_update(id),
        }
    }

    fn advance_reset(
        &mut self,
        mem: &GuestMemory,
        queues: &mut [Queue],
    ) -> Result<QueueMask, QueueError> {
        let Some(queue) = queues.get_mut(EVENT_QUEUE) else {
            return Ok(0);
        };
        match self.reset {
            TransportReset::Idle => Ok(0),
            TransportReset::Pending => {
                let Some(chain) = queue.pop(mem)? else {
                    return Ok(0);
                };
                let written = chain
                    .writer()
                    .write(mem, &EVENT_TRANSPORT_RESET.to_le_bytes())?;
                queue.add_used(mem, chain.head, written as u32)?;
                self.reset = TransportReset::Delivered {
                    avail_index: queue.avail_index(mem)?,
                };
                Ok(queue_bit(EVENT_QUEUE))
            }
            TransportReset::Delivered { avail_index } => {
                if queue.avail_index(mem)? == avail_index {
                    return Ok(0);
                }
                self.reset = TransportReset::Idle;
                for host_port in std::mem::take(&mut self.deferred_requests) {
                    let guest_port = self
                        .connections
                        .values()
                        .find(|c| c.host_port == host_port && c.state == State::Connecting)
                        .map(|c| c.guest_port);
                    if let Some(guest_port) = guest_port {
                        self.queue_control(Op::Request, host_port, guest_port, 0, 0);
                    }
                }
                self.fill_rx(mem, queues)
            }
        }
    }
}

impl VirtioDevice for Vsock {
    type State = VsockState;

    fn device_type(&self) -> u32 {
        TYPE_VSOCK
    }

    fn pci_class(&self) -> (u8, u8) {
        (0xff, 0x00)
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE, QUEUE_SIZE, QUEUE_SIZE]
    }

    fn features(&self) -> u64 {
        0
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.cid.to_le_bytes(), offset, data);
    }

    fn reset(&mut self) {
        self.connections.clear();
        self.control.clear();
        self.deferred_requests.clear();
        self.reset = TransportReset::Idle;
    }

    fn process_queue(
        &mut self,
        mem: &GuestMemory,
        queues: &mut [Queue],
        index: usize,
    ) -> Result<QueueMask, QueueError> {
        match index {
            RX_QUEUE | TX_QUEUE => {
                let mut mask = self.fill_rx(mem, queues)?;
                mask |= self.process_tx(mem, queues)?;
                mask |= self.fill_rx(mem, queues)?;
                Ok(mask)
            }
            EVENT_QUEUE => self.advance_reset(mem, queues),
            _ => Ok(0),
        }
    }

    fn state(&self) -> VsockState {
        VsockState {
            next_local_port: self.next_local_port,
        }
    }

    /// Host connections cannot be carried across a snapshot, so the guest is told to drop
    /// every connection it remembers as soon as it has an event buffer.
    fn restore(&mut self, state: VsockState) {
        self.reset();
        self.reset = TransportReset::Pending;
        self.next_local_port = state.next_local_port.max(LOCAL_PORT_BASE);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    use super::packet::{SHUTDOWN_RECEIVE, SHUTDOWN_SEND};
    use super::*;
    use crate::memory::GuestAddress;

    const GUEST_CID: u64 = 3;
    const GUEST_PORT: u32 = 1024;

    #[derive(Default)]
    struct StreamState {
        incoming: VecDeque<u8>,
        outgoing: Vec<u8>,
        eof: bool,
        write_room: Option<usize>,
        write_shut: bool,
    }

    #[derive(Clone, Default)]
    struct FakeStream(Arc<Mutex<StreamState>>);

    impl Read for FakeStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut state = self.0.lock().unwrap();
            if state.incoming.is_empty() {
                return if state.eof {
                    Ok(0)
                } else {
                    Err(io::ErrorKind::WouldBlock.into())
                };
            }
            let n = buf.len().min(state.incoming.len());
            for byte in buf.iter_mut().take(n) {
                *byte = state.incoming.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl Write for FakeStream {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            let mut state = self.0.lock().unwrap();
            let n = state
                .write_room
                .map_or(data.len(), |room| room.min(data.len()));
            if n == 0 && !data.is_empty() {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            if let Some(room) = &mut state.write_room {
                *room -= n;
            }
            state.outgoing.extend_from_slice(&data[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl VsockStream for FakeStream {
        fn raw_fd(&self) -> Option<RawFd> {
            Some(7)
        }

        fn shutdown_write(&mut self) -> io::Result<()> {
            self.0.lock().unwrap().write_shut = true;
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeBackend(Arc<Mutex<Vec<(u32, FakeStream)>>>);

    impl VsockBackend for FakeBackend {
        fn connect(&mut self, port: u32) -> io::Result<Box<dyn VsockStream>> {
            if port == 404 {
                return Err(io::ErrorKind::ConnectionRefused.into());
            }
            let stream = FakeStream::default();
            self.0.lock().unwrap().push((port, stream.clone()));
            Ok(Box::new(stream))
        }
    }

    struct Ring {
        base: u64,
        data: u64,
        next_desc: u16,
        avail: u16,
        used_seen: u16,
    }

    impl Ring {
        fn desc(&self) -> u64 {
            self.base
        }

        fn avail_ring(&self) -> u64 {
            self.base + 0x1000
        }

        fn used_ring(&self) -> u64 {
            self.base + 0x2000
        }
    }

    struct Harness {
        mem: GuestMemory,
        queues: Vec<Queue>,
        rings: Vec<Ring>,
        vsock: Vsock,
        backend: FakeBackend,
    }

    impl Harness {
        fn new() -> Self {
            let mem = GuestMemory::new(&[(GuestAddress(0), 0x1000000)]).unwrap();
            let rings: Vec<Ring> = (0..3u64)
                .map(|i| Ring {
                    base: 0x10000 + i * 0x10000,
                    data: 0x100000 + i * 0x300000,
                    next_desc: 0,
                    avail: 0,
                    used_seen: 0,
                })
                .collect();
            let queues = rings
                .iter()
                .map(|ring| {
                    let mut queue = Queue::new(QUEUE_SIZE);
                    queue.desc_table = GuestAddress(ring.desc());
                    queue.avail_ring = GuestAddress(ring.avail_ring());
                    queue.used_ring = GuestAddress(ring.used_ring());
                    queue.ready = true;
                    queue
                })
                .collect();
            let backend = FakeBackend::default();
            let vsock = Vsock::new(GUEST_CID, Box::new(backend.clone()));
            Self {
                mem,
                queues,
                rings,
                vsock,
                backend,
            }
        }

        fn post(&mut self, queue: usize, contents: &[u8], len: u32, writable: bool) {
            let ring = &mut self.rings[queue];
            let index = ring.next_desc % QUEUE_SIZE;
            let addr = ring.data + u64::from(index % 32) * 0x11000;
            self.mem.write(GuestAddress(addr), contents).unwrap();
            let flags: u16 = if writable { 2 } else { 0 };
            let desc = ring.desc() + u64::from(index) * 16;
            self.mem.write_obj(GuestAddress(desc), &addr).unwrap();
            self.mem.write_obj(GuestAddress(desc + 8), &len).unwrap();
            self.mem.write_obj(GuestAddress(desc + 12), &flags).unwrap();
            let slot = u64::from(ring.avail % QUEUE_SIZE);
            self.mem
                .write_obj(GuestAddress(ring.avail_ring() + 4 + slot * 2), &index)
                .unwrap();
            ring.avail = ring.avail.wrapping_add(1);
            ring.next_desc = ring.next_desc.wrapping_add(1);
            self.mem
                .write_obj(GuestAddress(ring.avail_ring() + 2), &ring.avail)
                .unwrap();
        }

        fn post_rx(&mut self, count: usize) {
            for _ in 0..count {
                self.post(RX_QUEUE, &[], 4096 + HEADER_LEN as u32, true);
            }
        }

        fn send(&mut self, header: Header, payload: &[u8]) {
            let mut bytes = header.encode().to_vec();
            bytes.extend_from_slice(payload);
            let len = bytes.len() as u32;
            self.post(TX_QUEUE, &bytes, len, false);
            self.process(TX_QUEUE);
        }

        fn process(&mut self, queue: usize) -> QueueMask {
            self.vsock
                .process_queue(&self.mem, &mut self.queues, queue)
                .unwrap()
        }

        fn fill(&mut self) {
            self.vsock.fill_rx(&self.mem, &mut self.queues).unwrap();
        }

        fn received(&mut self) -> Vec<(Header, Vec<u8>)> {
            let ring = &mut self.rings[RX_QUEUE];
            let used: u16 = self
                .mem
                .read_obj(GuestAddress(ring.used_ring() + 2))
                .unwrap();
            let mut packets = Vec::new();
            while ring.used_seen != used {
                let slot = u64::from(ring.used_seen % QUEUE_SIZE);
                let entry = ring.used_ring() + 4 + slot * 8;
                let id: u32 = self.mem.read_obj(GuestAddress(entry)).unwrap();
                let len: u32 = self.mem.read_obj(GuestAddress(entry + 4)).unwrap();
                let desc = ring.desc() + u64::from(id) * 16;
                let addr: u64 = self.mem.read_obj(GuestAddress(desc)).unwrap();
                let mut raw = vec![0u8; len as usize];
                self.mem.read(GuestAddress(addr), &mut raw).unwrap();
                let header = Header::parse(raw[..HEADER_LEN].try_into().unwrap());
                packets.push((header, raw[HEADER_LEN..].to_vec()));
                ring.used_seen = ring.used_seen.wrapping_add(1);
            }
            packets
        }

        fn connect_from_host(&mut self, early: &[u8]) -> (FakeStream, u32) {
            let stream = FakeStream::default();
            assert!(self.vsock.connect_from_host(
                Box::new(stream.clone()),
                GUEST_PORT,
                early.to_vec()
            ));
            self.post_rx(1);
            self.fill();
            let packets = self.received();
            assert_eq!(packets.len(), 1);
            let request = packets[0].0;
            assert_eq!(request.op(), Some(Op::Request));
            assert_eq!(request.dst_port, GUEST_PORT);
            assert_eq!(request.dst_cid, GUEST_CID);
            self.send(guest_header(Op::Response, request.src_port, 0), &[]);
            (stream, request.src_port)
        }
    }

    fn guest_header(op: Op, host_port: u32, len: u32) -> Header {
        Header {
            src_cid: GUEST_CID,
            dst_cid: HOST_CID,
            src_port: GUEST_PORT,
            dst_port: host_port,
            len,
            kind: TYPE_STREAM,
            op: op.raw(),
            flags: 0,
            buf_alloc: 64 * 1024,
            fwd_cnt: 0,
        }
    }

    #[test]
    fn host_connections_are_confirmed_after_the_guest_responds() {
        let mut h = Harness::new();
        let (stream, port) = h.connect_from_host(b"");
        assert!(port >= LOCAL_PORT_BASE);
        assert_eq!(
            stream.0.lock().unwrap().outgoing,
            format!("OK {port}\n").into_bytes()
        );
        let interests = h.vsock.interests();
        assert_eq!(interests.len(), 1);
        assert!(interests[0].read && !interests[0].write);
    }

    #[test]
    fn host_data_flows_to_the_guest_within_its_credit() {
        let mut h = Harness::new();
        let (stream, port) = h.connect_from_host(b"early ");
        stream
            .0
            .lock()
            .unwrap()
            .incoming
            .extend(b"data from the host");
        let id = h.vsock.interests()[0].id;
        h.send(
            Header {
                buf_alloc: 10,
                ..guest_header(Op::CreditUpdate, port, 0)
            },
            &[],
        );
        h.vsock.host_ready(id);
        h.post_rx(8);
        h.fill();
        let payload = |packets: Vec<(Header, Vec<u8>)>| {
            assert!(packets.iter().all(|(h, _)| h.op() == Some(Op::Rw)));
            packets
                .into_iter()
                .flat_map(|(_, data)| data)
                .collect::<Vec<u8>>()
        };
        assert_eq!(payload(h.received()), b"early data");
        h.send(
            Header {
                buf_alloc: 10,
                fwd_cnt: 10,
                ..guest_header(Op::CreditUpdate, port, 0)
            },
            &[],
        );
        assert_eq!(payload(h.received()), b" from the ");
        h.send(guest_header(Op::CreditUpdate, port, 0), &[]);
        assert_eq!(payload(h.received()), b"host");
        assert!(h.vsock.interests()[0].read);
    }

    #[test]
    fn guest_data_is_buffered_until_the_host_can_take_it() {
        let mut h = Harness::new();
        let (stream, port) = h.connect_from_host(b"");
        stream.0.lock().unwrap().outgoing.clear();
        stream.0.lock().unwrap().write_room = Some(3);
        h.send(guest_header(Op::Rw, port, 8), b"12345678");
        assert_eq!(stream.0.lock().unwrap().outgoing, b"123");
        let interest = h.vsock.interests()[0];
        assert!(interest.write);
        stream.0.lock().unwrap().write_room = None;
        h.vsock.host_ready(interest.id);
        assert_eq!(stream.0.lock().unwrap().outgoing, b"12345678");
        assert!(h.vsock.interests().is_empty());
    }

    #[test]
    fn credit_updates_follow_forwarded_data() {
        let mut h = Harness::new();
        let (_, port) = h.connect_from_host(b"");
        h.post_rx(8);
        let chunk = vec![7u8; MAX_PAYLOAD];
        h.send(guest_header(Op::Rw, port, chunk.len() as u32), &chunk);
        assert!(h.received().is_empty());
        h.send(guest_header(Op::Rw, port, chunk.len() as u32), &chunk);
        let packets = h.received();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0.op(), Some(Op::CreditUpdate));
        assert_eq!(packets[0].0.fwd_cnt, 2 * MAX_PAYLOAD as u32);
        assert_eq!(packets[0].0.buf_alloc, BUFFER_SIZE);
        h.send(guest_header(Op::CreditRequest, port, 0), &[]);
        assert_eq!(h.received()[0].0.op(), Some(Op::CreditUpdate));
    }

    #[test]
    fn guests_that_exceed_their_credit_are_reset() {
        let mut h = Harness::new();
        let (stream, port) = h.connect_from_host(b"");
        stream.0.lock().unwrap().write_room = Some(0);
        let chunk = vec![1u8; MAX_PAYLOAD];
        for _ in 0..BUFFER_SIZE as usize / MAX_PAYLOAD {
            h.send(guest_header(Op::Rw, port, chunk.len() as u32), &chunk);
        }
        h.post_rx(2);
        h.fill();
        assert!(h.received().is_empty());
        h.send(guest_header(Op::Rw, port, 1), &[1]);
        let packets = h.received();
        assert_eq!(packets[0].0.op(), Some(Op::Rst));
        assert!(h.vsock.interests().is_empty());
    }

    #[test]
    fn guest_initiated_connections_use_the_backend() {
        let mut h = Harness::new();
        h.post_rx(4);
        h.send(guest_header(Op::Request, 9000, 0), &[]);
        let packets = h.received();
        assert_eq!(packets[0].0.op(), Some(Op::Response));
        assert_eq!(packets[0].0.src_port, 9000);
        assert_eq!(packets[0].0.dst_port, GUEST_PORT);
        h.send(guest_header(Op::Rw, 9000, 2), b"hi");
        let streams = h.backend.0.lock().unwrap();
        assert_eq!(streams[0].0, 9000);
        assert_eq!(streams[0].1.0.lock().unwrap().outgoing, b"hi");
        drop(streams);
        h.send(guest_header(Op::Request, 404, 0), &[]);
        assert_eq!(h.received()[0].0.op(), Some(Op::Rst));
    }

    #[test]
    fn host_close_shuts_the_guest_side_down() {
        let mut h = Harness::new();
        let (stream, port) = h.connect_from_host(b"");
        stream.0.lock().unwrap().eof = true;
        let id = h.vsock.interests()[0].id;
        h.vsock.host_ready(id);
        h.post_rx(2);
        h.fill();
        let packets = h.received();
        assert_eq!(packets[0].0.op(), Some(Op::Shutdown));
        assert_eq!(packets[0].0.flags, SHUTDOWN_BOTH);
        assert!(h.vsock.interests().is_empty());
        h.send(
            Header {
                flags: SHUTDOWN_SEND,
                ..guest_header(Op::Shutdown, port, 0)
            },
            &[],
        );
        assert_eq!(h.received()[0].0.op(), Some(Op::Rst));
        assert!(h.vsock.connections.is_empty());
    }

    #[test]
    fn guest_shutdown_waits_for_buffered_data() {
        let mut h = Harness::new();
        let (stream, port) = h.connect_from_host(b"");
        stream.0.lock().unwrap().write_room = Some(0);
        h.send(guest_header(Op::Rw, port, 4), b"last");
        h.send(
            Header {
                flags: SHUTDOWN_RECEIVE | SHUTDOWN_SEND,
                ..guest_header(Op::Shutdown, port, 0)
            },
            &[],
        );
        h.post_rx(2);
        h.fill();
        assert!(h.received().is_empty());
        assert!(!stream.0.lock().unwrap().write_shut);
        stream.0.lock().unwrap().write_room = None;
        let id = h.vsock.interests()[0].id;
        h.vsock.host_ready(id);
        h.fill();
        assert!(stream.0.lock().unwrap().outgoing.ends_with(b"last"));
        assert!(stream.0.lock().unwrap().write_shut);
        assert_eq!(h.received()[0].0.op(), Some(Op::Rst));
        assert!(h.vsock.connections.is_empty());
    }

    #[test]
    fn malformed_packets_are_dropped_or_reset() {
        let mut h = Harness::new();
        h.post_rx(8);
        h.send(
            Header {
                src_cid: 9,
                ..guest_header(Op::Request, 1, 0)
            },
            &[],
        );
        assert!(h.received().is_empty());
        h.send(
            Header {
                kind: 2,
                ..guest_header(Op::Request, 1, 0)
            },
            &[],
        );
        assert_eq!(h.received()[0].0.op(), Some(Op::Rst));
        h.send(guest_header(Op::Rw, 1, 100), b"short");
        assert_eq!(h.received()[0].0.op(), Some(Op::Rst));
        h.send(guest_header(Op::Rw, 1, 1), b"x");
        assert_eq!(h.received()[0].0.op(), Some(Op::Rst));
        h.send(guest_header(Op::Rst, 1, 0), &[]);
        h.post(TX_QUEUE, &[0u8; 10], 10, false);
        h.process(TX_QUEUE);
        assert!(h.received().is_empty());
        assert!(h.backend.0.lock().unwrap().is_empty());
    }

    #[test]
    fn replies_stop_queueing_when_the_guest_posts_no_buffers() {
        let mut h = Harness::new();
        for _ in 0..QUEUE_SIZE {
            h.post(
                TX_QUEUE,
                &guest_header(Op::Rw, 5, 0).encode(),
                HEADER_LEN as u32,
                false,
            );
        }
        h.process(TX_QUEUE);
        assert_eq!(h.vsock.control.len(), MAX_PENDING_CONTROL);
        let used: u16 = h
            .mem
            .read_obj(GuestAddress(h.rings[TX_QUEUE].used_ring() + 2))
            .unwrap();
        assert_eq!(usize::from(used), MAX_PENDING_CONTROL);
    }

    #[test]
    fn restore_announces_a_transport_reset_before_new_connections() {
        let mut h = Harness::new();
        let (_, old_port) = h.connect_from_host(b"");
        let state = h.vsock.state();
        h.vsock = Vsock::new(GUEST_CID, Box::new(h.backend.clone()));
        h.vsock.restore(state);
        assert!(h.vsock.connections.is_empty());
        assert_eq!(h.process(EVENT_QUEUE), 0);
        let stream = FakeStream::default();
        assert!(
            h.vsock
                .connect_from_host(Box::new(stream.clone()), GUEST_PORT, Vec::new())
        );
        h.post_rx(2);
        h.fill();
        assert!(h.received().is_empty(), "requests wait for the reset");
        h.post(EVENT_QUEUE, &[0xff; 4], 4, true);
        h.post(EVENT_QUEUE, &[0xff; 4], 4, true);
        assert_eq!(h.process(EVENT_QUEUE), queue_bit(EVENT_QUEUE));
        let used: u32 = h
            .mem
            .read_obj(GuestAddress(h.rings[EVENT_QUEUE].used_ring() + 8))
            .unwrap();
        assert_eq!(used, 4);
        let addr: u64 = h
            .mem
            .read_obj(GuestAddress(h.rings[EVENT_QUEUE].desc()))
            .unwrap();
        let event: u32 = h.mem.read_obj(GuestAddress(addr)).unwrap();
        assert_eq!(event, EVENT_TRANSPORT_RESET);
        assert_eq!(h.process(EVENT_QUEUE), 0);
        assert!(
            h.received().is_empty(),
            "the guest has not finished the reset"
        );
        h.post(EVENT_QUEUE, &[0xff; 4], 4, true);
        assert_eq!(h.process(EVENT_QUEUE), queue_bit(RX_QUEUE));
        let packets = h.received();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0.op(), Some(Op::Request));
        assert_eq!(packets[0].0.dst_port, GUEST_PORT);
        assert!(
            packets[0].0.src_port > old_port,
            "host ports are not reused"
        );
        h.send(guest_header(Op::Response, packets[0].0.src_port, 0), &[]);
        assert!(stream.0.lock().unwrap().outgoing.starts_with(b"OK "));
        assert_eq!(h.process(EVENT_QUEUE), 0);
    }

    #[test]
    fn config_space_reports_the_guest_cid() {
        let vsock = Vsock::new(0x1_0000_0003, Box::new(FakeBackend::default()));
        let mut config = [0u8; 8];
        vsock.read_config(0, &mut config);
        assert_eq!(u64::from_le_bytes(config), 0x1_0000_0003);
    }
}
