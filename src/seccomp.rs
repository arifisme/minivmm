use std::collections::BTreeMap;

use kvm_bindings::{
    kvm_clock_data, kvm_cpuid2, kvm_ioeventfd, kvm_irq_routing, kvm_irqchip, kvm_irqfd,
    kvm_lapic_state, kvm_mp_state, kvm_msrs, kvm_pit_state2, kvm_regs, kvm_sregs, kvm_vcpu_events,
    kvm_xcrs, kvm_xsave,
};
use libc::{c_int, c_void, siginfo_t};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use thiserror::Error;
use vmm_sys_util::signal::register_signal_handler;
use vmm_sys_util::{ioctl_io_nr, ioctl_ior_nr, ioctl_iow_nr, ioctl_iowr_nr};

const KVMIO: u32 = 0xae;
const BLOCKED_SYSCALL_EXIT_CODE: i32 = 159;
const SIGSYS_SYSCALL_OFFSET: usize = 24;

ioctl_io_nr!(KVM_RUN, KVMIO, 0x80);
ioctl_ior_nr!(KVM_GET_REGS, KVMIO, 0x81, kvm_regs);
ioctl_ior_nr!(KVM_GET_SREGS, KVMIO, 0x83, kvm_sregs);
ioctl_iowr_nr!(KVM_GET_MSRS, KVMIO, 0x88, kvm_msrs);
ioctl_ior_nr!(KVM_GET_LAPIC, KVMIO, 0x8e, kvm_lapic_state);
ioctl_iowr_nr!(KVM_GET_CPUID2, KVMIO, 0x91, kvm_cpuid2);
ioctl_ior_nr!(KVM_GET_MP_STATE, KVMIO, 0x98, kvm_mp_state);
ioctl_ior_nr!(KVM_GET_VCPU_EVENTS, KVMIO, 0x9f, kvm_vcpu_events);
ioctl_ior_nr!(KVM_GET_XSAVE, KVMIO, 0xa4, kvm_xsave);
ioctl_ior_nr!(KVM_GET_XCRS, KVMIO, 0xa6, kvm_xcrs);
ioctl_iow_nr!(KVM_SET_GSI_ROUTING, KVMIO, 0x6a, kvm_irq_routing);
ioctl_iow_nr!(KVM_IRQFD, KVMIO, 0x76, kvm_irqfd);
ioctl_iow_nr!(KVM_IOEVENTFD, KVMIO, 0x79, kvm_ioeventfd);
ioctl_iowr_nr!(KVM_GET_IRQCHIP, KVMIO, 0x62, kvm_irqchip);
ioctl_ior_nr!(KVM_GET_PIT2, KVMIO, 0x9f, kvm_pit_state2);
ioctl_ior_nr!(KVM_GET_CLOCK, KVMIO, 0x7c, kvm_clock_data);

#[derive(Debug, Error)]
pub enum SeccompError {
    #[error("failed to build seccomp filter: {0}")]
    Build(#[from] seccompiler::BackendError),
    #[error("failed to install seccomp filter: {0}")]
    Install(#[from] seccompiler::Error),
    #[error("failed to install SIGSYS handler: {0}")]
    Handler(#[from] vmm_sys_util::errno::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadKind {
    Vmm,
    Vcpu,
}

const COMMON_SYSCALLS: &[i64] = &[
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_futex,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_madvise,
    libc::SYS_brk,
    libc::SYS_rt_sigreturn,
    // The kernel resumes a timed wait interrupted by a stop and continue through this call.
    libc::SYS_restart_syscall,
    libc::SYS_rt_sigprocmask,
    libc::SYS_sigaltstack,
    libc::SYS_sched_yield,
    libc::SYS_close,
    libc::SYS_clock_gettime,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_getrandom,
    libc::SYS_gettid,
    libc::SYS_getpid,
    libc::SYS_tgkill,
    libc::SYS_exit,
    libc::SYS_exit_group,
];

const VMM_SYSCALLS: &[i64] = &[
    libc::SYS_epoll_wait,
    libc::SYS_epoll_pwait,
    libc::SYS_epoll_ctl,
    libc::SYS_accept4,
    libc::SYS_recvfrom,
    libc::SYS_sendto,
    libc::SYS_readv,
    libc::SYS_writev,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_openat,
    libc::SYS_lseek,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_statx,
    libc::SYS_rename,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_unlink,
    libc::SYS_unlinkat,
    libc::SYS_fcntl,
];

fn vcpu_ioctls() -> Vec<u64> {
    [
        KVM_RUN(),
        KVM_GET_REGS(),
        KVM_GET_SREGS(),
        KVM_GET_MSRS(),
        KVM_GET_LAPIC(),
        KVM_GET_CPUID2(),
        KVM_GET_MP_STATE(),
        KVM_GET_VCPU_EVENTS(),
        KVM_GET_XSAVE(),
        KVM_GET_XCRS(),
        KVM_SET_GSI_ROUTING(),
        KVM_IOEVENTFD(),
    ]
    .to_vec()
}

fn vmm_ioctls() -> Vec<u64> {
    [
        KVM_GET_IRQCHIP(),
        KVM_GET_PIT2(),
        KVM_GET_CLOCK(),
        KVM_SET_GSI_ROUTING(),
        KVM_IRQFD(),
        KVM_IOEVENTFD(),
        libc::FIONBIO,
        libc::TCGETS,
        libc::TCSETS,
    ]
    .to_vec()
}

fn ioctl_rules(requests: &[u64]) -> Result<Vec<SeccompRule>, SeccompError> {
    requests
        .iter()
        .map(|request| {
            let condition =
                SeccompCondition::new(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, *request)?;
            Ok(SeccompRule::new(vec![condition])?)
        })
        .collect()
}

fn non_executable_rule() -> Result<Vec<SeccompRule>, SeccompError> {
    let condition = SeccompCondition::new(
        2,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::MaskedEq(libc::PROT_EXEC as u64),
        0,
    )?;
    Ok(vec![SeccompRule::new(vec![condition])?])
}

pub fn build_filter(kind: ThreadKind) -> Result<BpfProgram, SeccompError> {
    let (extra, ioctls) = match kind {
        ThreadKind::Vmm => (VMM_SYSCALLS, vmm_ioctls()),
        ThreadKind::Vcpu => (&[][..], vcpu_ioctls()),
    };
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = COMMON_SYSCALLS
        .iter()
        .chain(extra)
        .map(|syscall| (*syscall, Vec::new()))
        .collect();
    rules.insert(libc::SYS_ioctl, ioctl_rules(&ioctls)?);
    rules.insert(libc::SYS_mmap, non_executable_rule()?);
    rules.insert(libc::SYS_mprotect, non_executable_rule()?);
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Trap,
        SeccompAction::Allow,
        TargetArch::x86_64,
    )?;
    Ok(filter.try_into()?)
}

pub fn apply(program: &BpfProgram) -> Result<(), SeccompError> {
    Ok(seccompiler::apply_filter(program)?)
}

fn decimal(mut value: u32, buf: &mut [u8; 10]) -> &[u8] {
    let mut start = buf.len();
    loop {
        start -= 1;
        buf[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buf[start..];
        }
    }
}

extern "C" fn report_blocked_syscall(_: c_int, info: *mut siginfo_t, _: *mut c_void) {
    // SAFETY: the kernel passes a valid siginfo for SIGSYS, whose _sigsys
    // member stores the syscall number right after the faulting address.
    let syscall = unsafe {
        info.cast::<u8>()
            .add(SIGSYS_SYSCALL_OFFSET)
            .cast::<i32>()
            .read()
    };
    let mut digits = [0u8; 10];
    let parts: [&[u8]; 3] = [
        b"minivmm: seccomp blocked syscall ",
        decimal(syscall as u32, &mut digits),
        b"\n",
    ];
    for part in parts {
        // SAFETY: write(2) is async-signal-safe and the buffer is valid.
        unsafe { libc::write(libc::STDERR_FILENO, part.as_ptr().cast(), part.len()) };
    }
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(BLOCKED_SYSCALL_EXIT_CODE) };
}

pub fn install_violation_reporter() -> Result<(), SeccompError> {
    Ok(register_signal_handler(
        libc::SIGSYS,
        report_blocked_syscall,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_build_for_every_thread_kind() {
        for kind in [ThreadKind::Vmm, ThreadKind::Vcpu] {
            assert!(!build_filter(kind).unwrap().is_empty());
        }
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_abi() {
        assert_eq!(KVM_RUN(), 0xae80);
        assert_eq!(KVM_GET_REGS(), 0x8090_ae81);
        assert_eq!(KVM_SET_GSI_ROUTING(), 0x4008_ae6a);
        assert_eq!(KVM_IRQFD(), 0x4020_ae76);
        assert_eq!(KVM_IOEVENTFD(), 0x4040_ae79);
        assert_eq!(KVM_GET_IRQCHIP(), 0xc208_ae62);
        assert_eq!(KVM_GET_XSAVE(), 0x9000_aea4);
    }

    #[test]
    fn formats_decimal_numbers() {
        let mut buf = [0u8; 10];
        assert_eq!(decimal(0, &mut buf), b"0");
        assert_eq!(decimal(318, &mut buf), b"318");
        assert_eq!(decimal(u32::MAX, &mut buf), b"4294967295");
    }

    #[test]
    fn filter_kills_unlisted_syscalls_in_a_child() {
        let program = build_filter(ThreadKind::Vcpu).unwrap();
        // SAFETY: the child only performs async-signal-safe calls before exiting.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let installed = install_violation_reporter().is_ok() && apply(&program).is_ok();
            // SAFETY: getppid is a plain syscall that the vCPU filter does not allow.
            unsafe { libc::syscall(libc::SYS_getppid) };
            // SAFETY: exiting the forked child.
            unsafe { libc::_exit(if installed { 0 } else { 1 }) };
        }
        let mut status = 0;
        // SAFETY: waiting for the child created above.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), BLOCKED_SYSCALL_EXIT_CODE);
    }

    fn run_filtered_child(kind: ThreadKind, child: fn(), parent: impl Fn(libc::pid_t)) -> i32 {
        let program = build_filter(kind).unwrap();
        // SAFETY: the child only performs raw system calls before exiting.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            if install_violation_reporter().is_ok() && apply(&program).is_ok() {
                child();
            }
            // SAFETY: exiting the forked child.
            unsafe { libc::_exit(0) };
        }
        parent(pid);
        let mut status = 0;
        // SAFETY: waiting for the child created above.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status));
        libc::WEXITSTATUS(status)
    }

    #[test]
    fn timed_waits_survive_a_stop_and_continue() {
        let wait = || {
            let word = 0u32;
            let timeout = libc::timespec {
                tv_sec: 1,
                tv_nsec: 0,
            };
            // SAFETY: a private futex wait on a local word with a relative timeout.
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    &word,
                    libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                    0,
                    &timeout,
                )
            };
        };
        let stop_and_continue = |pid| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            // SAFETY: signalling our own child.
            unsafe { libc::kill(pid, libc::SIGSTOP) };
            std::thread::sleep(std::time::Duration::from_millis(100));
            // SAFETY: signalling our own child.
            unsafe { libc::kill(pid, libc::SIGCONT) };
        };
        for kind in [ThreadKind::Vmm, ThreadKind::Vcpu] {
            assert_eq!(run_filtered_child(kind, wait, stop_and_continue), 0);
        }
    }

    fn map_page(protection: libc::c_int) -> *mut libc::c_void {
        // SAFETY: an anonymous private mapping with no address hint.
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                protection,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if page == libc::MAP_FAILED {
            // SAFETY: exiting the forked child.
            unsafe { libc::_exit(2) };
        }
        page
    }

    #[test]
    fn executable_mappings_are_refused() {
        let map_executable = || {
            map_page(libc::PROT_READ | libc::PROT_EXEC);
        };
        let make_executable = || {
            let page = map_page(libc::PROT_READ | libc::PROT_WRITE);
            // SAFETY: changing the protection of the page mapped above.
            unsafe { libc::mprotect(page, 4096, libc::PROT_READ | libc::PROT_EXEC) };
        };
        let make_writable = || {
            let page = map_page(libc::PROT_READ);
            // SAFETY: changing the protection of the page mapped above.
            if unsafe { libc::mprotect(page, 4096, libc::PROT_READ | libc::PROT_WRITE) } != 0 {
                // SAFETY: exiting the forked child.
                unsafe { libc::_exit(3) };
            }
        };
        for kind in [ThreadKind::Vmm, ThreadKind::Vcpu] {
            for child in [map_executable, make_executable] {
                assert_eq!(
                    run_filtered_child(kind, child, |_| {}),
                    BLOCKED_SYSCALL_EXIT_CODE
                );
            }
            assert_eq!(run_filtered_child(kind, make_writable, |_| {}), 0);
        }
    }
}
