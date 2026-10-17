use std::io::{self, Read, Write};
use std::os::fd::RawFd;

use super::packet::{Header, SHUTDOWN_BOTH, SHUTDOWN_SEND};

pub const BUFFER_SIZE: u32 = 256 * 1024;
const CREDIT_UPDATE_THRESHOLD: u32 = BUFFER_SIZE / 2;

pub trait VsockStream: Read + Write + Send {
    fn raw_fd(&self) -> Option<RawFd>;
    fn shutdown_write(&mut self) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Connecting,
    Established,
}

pub struct CreditViolation;

pub struct Connection {
    pub host_port: u32,
    pub guest_port: u32,
    pub state: State,
    stream: Box<dyn VsockStream>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    rx_cnt: u32,
    fwd_cnt: u32,
    advertised_fwd_cnt: u32,
    to_host: Vec<u8>,
    from_host: Vec<u8>,
    peer_shutdown: u32,
    write_closed: bool,
    pub host_eof: bool,
    pub readable: bool,
}

impl Connection {
    pub fn new(
        stream: Box<dyn VsockStream>,
        host_port: u32,
        guest_port: u32,
        state: State,
        from_host: Vec<u8>,
    ) -> Self {
        Self {
            host_port,
            guest_port,
            state,
            stream,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            rx_cnt: 0,
            fwd_cnt: 0,
            advertised_fwd_cnt: 0,
            to_host: Vec::new(),
            from_host,
            peer_shutdown: 0,
            write_closed: false,
            host_eof: false,
            readable: false,
        }
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        self.stream.raw_fd()
    }

    pub fn is_established(&self) -> bool {
        self.state == State::Established
    }

    pub fn update_peer_credit(&mut self, header: &Header) {
        self.peer_buf_alloc = header.buf_alloc;
        self.peer_fwd_cnt = header.fwd_cnt;
    }

    pub fn peer_credit(&self) -> u32 {
        let in_flight = self.rx_cnt.wrapping_sub(self.peer_fwd_cnt);
        self.peer_buf_alloc.saturating_sub(in_flight)
    }

    pub fn advertise_fwd_cnt(&mut self) -> u32 {
        self.advertised_fwd_cnt = self.fwd_cnt;
        self.fwd_cnt
    }

    pub fn wants_credit_update(&self) -> bool {
        self.fwd_cnt.wrapping_sub(self.advertised_fwd_cnt) >= CREDIT_UPDATE_THRESHOLD
    }

    pub fn wants_to_write(&self) -> bool {
        !self.to_host.is_empty()
    }

    pub fn wants_to_read(&self) -> bool {
        self.is_established() && !self.host_eof && !self.readable
    }

    pub fn has_data_for_guest(&self) -> bool {
        self.is_established()
            && !self.host_eof
            && (self.readable || !self.from_host.is_empty())
            && self.peer_credit() > 0
    }

    pub fn greet_host(&mut self) -> io::Result<()> {
        let greeting = format!("OK {}\n", self.host_port);
        let written = self.stream.write(greeting.as_bytes())?;
        if written != greeting.len() {
            return Err(io::ErrorKind::WriteZero.into());
        }
        self.state = State::Established;
        Ok(())
    }

    pub fn accept_from_guest(&mut self, data: &[u8]) -> Result<io::Result<()>, CreditViolation> {
        if self.to_host.len() + data.len() > BUFFER_SIZE as usize {
            return Err(CreditViolation);
        }
        self.to_host.extend_from_slice(data);
        Ok(self.flush())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        let mut written = 0;
        let result = loop {
            if written == self.to_host.len() {
                break Ok(());
            }
            match self.stream.write(&self.to_host[written..]) {
                Ok(0) => break Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => written += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break Ok(()),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => break Err(err),
            }
        };
        self.to_host.drain(..written);
        self.fwd_cnt = self.fwd_cnt.wrapping_add(written as u32);
        result
    }

    pub fn read_for_guest(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.from_host.is_empty() {
            let n = buf.len().min(self.from_host.len());
            buf[..n].copy_from_slice(&self.from_host[..n]);
            self.from_host.drain(..n);
            return Ok(n);
        }
        self.stream.read(buf)
    }

    pub fn sent_to_guest(&mut self, len: usize) {
        self.rx_cnt = self.rx_cnt.wrapping_add(len as u32);
    }

    pub fn peer_shutdown(&mut self, flags: u32) {
        self.peer_shutdown |= flags & SHUTDOWN_BOTH;
    }

    /// Applies a guest shutdown once everything it sent has reached the host, and reports
    /// whether both directions are now closed so the connection can be dropped.
    pub fn settle(&mut self) -> io::Result<bool> {
        if self.wants_to_write() {
            return Ok(false);
        }
        if self.peer_shutdown & SHUTDOWN_SEND != 0 && !self.write_closed {
            self.write_closed = true;
            self.stream.shutdown_write()?;
        }
        let guest_done = self.peer_shutdown == SHUTDOWN_BOTH;
        let both_sides_done = self.host_eof && self.peer_shutdown & SHUTDOWN_SEND != 0;
        Ok(guest_done || both_sides_done)
    }
}
