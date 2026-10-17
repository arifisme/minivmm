use std::io;
use std::os::fd::RawFd;

use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};

const MAX_EVENTS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interest {
    Read,
    Write,
    ReadWrite,
}

impl Interest {
    pub fn new(read: bool, write: bool) -> Option<Self> {
        match (read, write) {
            (true, true) => Some(Interest::ReadWrite),
            (true, false) => Some(Interest::Read),
            (false, true) => Some(Interest::Write),
            (false, false) => None,
        }
    }

    fn events(self) -> EventSet {
        match self {
            Interest::Read => EventSet::IN,
            Interest::Write => EventSet::OUT,
            Interest::ReadWrite => EventSet::IN | EventSet::OUT,
        }
    }
}

pub struct EventLoop {
    epoll: Epoll,
    events: Vec<EpollEvent>,
}

impl EventLoop {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            epoll: Epoll::new()?,
            events: vec![EpollEvent::default(); MAX_EVENTS],
        })
    }

    pub fn add(&self, fd: RawFd, token: u64) -> io::Result<()> {
        self.add_with(fd, token, Interest::Read)
    }

    pub fn add_with(&self, fd: RawFd, token: u64, interest: Interest) -> io::Result<()> {
        self.epoll.ctl(
            ControlOperation::Add,
            fd,
            EpollEvent::new(interest.events(), token),
        )
    }

    pub fn modify(&self, fd: RawFd, token: u64, interest: Interest) -> io::Result<()> {
        self.epoll.ctl(
            ControlOperation::Modify,
            fd,
            EpollEvent::new(interest.events(), token),
        )
    }

    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        self.epoll
            .ctl(ControlOperation::Delete, fd, EpollEvent::default())
    }

    pub fn wait(&mut self, timeout_ms: i32) -> io::Result<Vec<u64>> {
        let count = match self.epoll.wait(timeout_ms, &mut self.events) {
            Ok(count) => count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => 0,
            Err(err) => return Err(err),
        };
        Ok(self.events[..count].iter().map(EpollEvent::data).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

    use super::*;

    #[test]
    fn reports_ready_tokens() {
        let mut event_loop = EventLoop::new().unwrap();
        let a = EventFd::new(EFD_NONBLOCK).unwrap();
        let b = EventFd::new(EFD_NONBLOCK).unwrap();
        event_loop.add(a.as_raw_fd(), 1).unwrap();
        event_loop.add(b.as_raw_fd(), 2).unwrap();
        assert!(event_loop.wait(0).unwrap().is_empty());
        b.write(1).unwrap();
        assert_eq!(event_loop.wait(0).unwrap(), vec![2]);
        event_loop.remove(b.as_raw_fd()).unwrap();
        a.write(1).unwrap();
        assert_eq!(event_loop.wait(0).unwrap(), vec![1]);
    }

    #[test]
    fn write_interest_can_be_added_and_dropped() {
        let mut event_loop = EventLoop::new().unwrap();
        let (left, _right) = std::os::unix::net::UnixStream::pair().unwrap();
        event_loop
            .add_with(left.as_raw_fd(), 5, Interest::Read)
            .unwrap();
        assert!(event_loop.wait(0).unwrap().is_empty());
        event_loop
            .modify(left.as_raw_fd(), 6, Interest::ReadWrite)
            .unwrap();
        assert_eq!(event_loop.wait(0).unwrap(), vec![6]);
        assert_eq!(Interest::new(false, false), None);
        assert_eq!(Interest::new(false, true), Some(Interest::Write));
    }
}
