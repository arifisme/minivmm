use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mv_proto::api::PoolInfo;
use tokio::sync::Notify;

pub trait PooledVm: Send + 'static {
    fn is_alive(&self) -> bool;
}

pub enum Lease<V> {
    Warm(V),
    Cold(StartPermit),
}

/// Proof that the pool counted a VM as starting; hand it back through `started` or
/// `start_failed`.
#[must_use]
pub struct StartPermit(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    ConcurrencyLimit,
    Closed,
}

struct Idle<V> {
    vm: V,
    since: Instant,
}

struct State<V> {
    idle: Vec<Idle<V>>,
    busy: u32,
    starting: u32,
    max: u32,
    closed: bool,
}

/// The VMs of one function revision: idle VMs ready for reuse plus counts of VMs serving a
/// request or starting. At most `max` VMs exist at once, each serving one request at a time.
pub struct Pool<V> {
    state: Mutex<State<V>>,
    changed: Notify,
}

impl<V: PooledVm> Pool<V> {
    pub fn new(max: u32) -> Self {
        Self {
            state: Mutex::new(State {
                idle: Vec::new(),
                busy: 0,
                starting: 0,
                max,
                closed: false,
            }),
            changed: Notify::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State<V>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        self.changed.notify_waiters();
    }

    pub fn set_max(&self, max: u32) {
        self.state().max = max;
        self.notify();
    }

    /// Takes an idle VM, or permission to start one, without waiting. Dead idle VMs are
    /// returned in `dead` for the caller to clean up.
    pub fn try_acquire(&self, dead: &mut Vec<V>) -> Result<Lease<V>, Busy> {
        let mut state = self.state();
        if state.closed {
            return Err(Busy::Closed);
        }
        while let Some(idle) = state.idle.pop() {
            if idle.vm.is_alive() {
                state.busy += 1;
                return Ok(Lease::Warm(idle.vm));
            }
            dead.push(idle.vm);
        }
        if state.busy + state.starting < state.max {
            state.starting += 1;
            return Ok(Lease::Cold(StartPermit(())));
        }
        Err(Busy::ConcurrencyLimit)
    }

    pub async fn acquire(&self, wait: Duration, dead: &mut Vec<V>) -> Result<Lease<V>, Busy> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_acquire(dead) {
                Err(Busy::ConcurrencyLimit) => {}
                other => return other,
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(Busy::ConcurrencyLimit);
            }
        }
    }

    pub fn started(&self, permit: StartPermit) {
        let StartPermit(()) = permit;
        let mut state = self.state();
        state.starting -= 1;
        state.busy += 1;
    }

    pub fn start_failed(&self, permit: StartPermit) {
        let StartPermit(()) = permit;
        self.state().starting -= 1;
        self.notify();
    }

    /// Returns a VM after a request. A closed pool hands the VM back for destruction.
    pub fn release(&self, vm: V) -> Option<V> {
        let mut state = self.state();
        state.busy -= 1;
        let rejected = if state.closed || !vm.is_alive() {
            Some(vm)
        } else {
            state.idle.push(Idle {
                vm,
                since: Instant::now(),
            });
            None
        };
        drop(state);
        self.notify();
        rejected
    }

    pub fn discard(&self) {
        self.state().busy -= 1;
        self.notify();
    }

    pub fn take_idle_since(&self, cutoff: Instant) -> Vec<V> {
        let mut state = self.state();
        let (expired, kept): (Vec<_>, Vec<_>) =
            state.idle.drain(..).partition(|idle| idle.since <= cutoff);
        state.idle = kept;
        expired.into_iter().map(|idle| idle.vm).collect()
    }

    pub fn take_all_idle(&self) -> Vec<V> {
        self.state().idle.drain(..).map(|idle| idle.vm).collect()
    }

    pub fn oldest_idle(&self) -> Option<Instant> {
        self.state().idle.iter().map(|idle| idle.since).min()
    }

    pub fn take_oldest_idle(&self) -> Option<V> {
        let mut state = self.state();
        let index = state
            .idle
            .iter()
            .enumerate()
            .min_by_key(|(_, idle)| idle.since)
            .map(|(index, _)| index)?;
        Some(state.idle.remove(index).vm)
    }

    pub fn close(&self) -> Vec<V> {
        let mut state = self.state();
        state.closed = true;
        let idle = state.idle.drain(..).map(|idle| idle.vm).collect();
        drop(state);
        self.notify();
        idle
    }

    pub fn info(&self) -> PoolInfo {
        let state = self.state();
        PoolInfo {
            idle: state.idle.len() as u32,
            busy: state.busy,
            starting: state.starting,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[derive(Clone, Debug)]
    struct FakeVm {
        id: u32,
        alive: Arc<AtomicBool>,
    }

    impl FakeVm {
        fn new(id: u32) -> Self {
            Self {
                id,
                alive: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    impl PooledVm for FakeVm {
        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
    }

    fn cold(pool: &Pool<FakeVm>) -> StartPermit {
        match pool.try_acquire(&mut Vec::new()) {
            Ok(Lease::Cold(permit)) => permit,
            _ => panic!("expected permission to start a VM"),
        }
    }

    #[test]
    fn scales_out_to_the_limit_then_reuses_idle_vms() {
        let pool = Pool::new(2);
        let first = cold(&pool);
        let second = cold(&pool);
        assert!(matches!(
            pool.try_acquire(&mut Vec::new()),
            Err(Busy::ConcurrencyLimit)
        ));
        pool.started(first);
        pool.start_failed(second);
        assert_eq!(
            pool.info(),
            PoolInfo {
                idle: 0,
                busy: 1,
                starting: 0
            }
        );
        assert!(pool.release(FakeVm::new(1)).is_none());
        match pool.try_acquire(&mut Vec::new()) {
            Ok(Lease::Warm(vm)) => assert_eq!(vm.id, 1),
            _ => panic!("expected the idle VM"),
        }
        pool.discard();
        assert_eq!(pool.info(), PoolInfo::default());
    }

    #[test]
    fn dead_idle_vms_are_skipped_and_reported() {
        let pool = Pool::new(1);
        pool.started(cold(&pool));
        let vm = FakeVm::new(5);
        let alive = vm.alive.clone();
        assert!(pool.release(vm).is_none());
        alive.store(false, Ordering::SeqCst);
        let mut dead = Vec::new();
        assert!(matches!(pool.try_acquire(&mut dead), Ok(Lease::Cold(_))));
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].id, 5);
    }

    #[tokio::test]
    async fn waiters_get_released_vms_or_time_out() {
        let pool = Arc::new(Pool::new(1));
        pool.started(cold(&pool));
        let started = Instant::now();
        assert!(matches!(
            pool.acquire(Duration::from_millis(50), &mut Vec::new())
                .await,
            Err(Busy::ConcurrencyLimit)
        ));
        assert!(started.elapsed() >= Duration::from_millis(50));
        let waiter = {
            let pool = pool.clone();
            tokio::spawn(async move { pool.acquire(Duration::from_secs(5), &mut Vec::new()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(pool.release(FakeVm::new(9)).is_none());
        match waiter.await.unwrap() {
            Ok(Lease::Warm(vm)) => assert_eq!(vm.id, 9),
            _ => panic!("the waiter should get the released VM"),
        }
    }

    #[test]
    fn idle_vms_expire_and_closing_rejects_everything() {
        let pool = Pool::new(3);
        let permits: Vec<StartPermit> = (0..3).map(|_| cold(&pool)).collect();
        for (id, permit) in permits.into_iter().enumerate() {
            pool.started(permit);
            assert!(pool.release(FakeVm::new(id as u32)).is_none());
        }
        assert!(pool.oldest_idle().is_some());
        assert_eq!(pool.take_oldest_idle().map(|vm| vm.id), Some(0));
        assert!(
            pool.take_idle_since(Instant::now() - Duration::from_secs(60))
                .is_empty()
        );
        assert_eq!(pool.take_idle_since(Instant::now()).len(), 2);
        pool.started(cold(&pool));
        pool.set_max(1);
        assert!(matches!(
            pool.try_acquire(&mut Vec::new()),
            Err(Busy::ConcurrencyLimit)
        ));
        assert!(pool.close().is_empty());
        assert!(matches!(
            pool.try_acquire(&mut Vec::new()),
            Err(Busy::Closed)
        ));
        assert!(pool.release(FakeVm::new(7)).is_some());
        assert!(pool.take_all_idle().is_empty());
    }
}
