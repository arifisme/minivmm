mod common;

use std::time::Duration;

use common::{BOOT_TIMEOUT, Vm, artifact, environment_ready, kernel_args};

fn initramfs_args(cpus: u8) -> Vec<String> {
    let mut args = kernel_args(
        "console=ttyS0 reboot=k panic=1 earlyprintk=serial loglevel=8",
        cpus,
    );
    args.push("--initrd".into());
    args.push(artifact("initramfs.cpio.gz").display().to_string());
    args
}

#[test]
fn boots_initramfs_with_all_cpus_and_powers_off() {
    if !environment_ready("boots_initramfs", &["bzImage", "initramfs.cpio.gz"]) {
        return;
    }
    let mut vm = Vm::start("boots_initramfs", &initramfs_args(4));
    vm.wait_for("minivmm initramfs ready", BOOT_TIMEOUT);
    let out = vm.run("grep -c ^processor /proc/cpuinfo");
    assert!(out.contains("\n4"), "expected 4 CPUs: {out}");
    let apic_ids = vm.run(r"grep '^apicid' /proc/cpuinfo | tr -d ' \t' | tr '\n' ' '");
    assert!(
        apic_ids.contains("apicid:0 apicid:1 apicid:2 apicid:3 "),
        "expected distinct APIC IDs: {apic_ids}"
    );
    vm.run("cat /proc/interrupts | grep -i ttyS0");
    vm.send("poweroff -f\n");
    let status = vm.wait_exit(BOOT_TIMEOUT);
    assert!(status.success(), "exit status {status}");
    assert!(vm.output().contains("PowerOff"));
}

#[test]
fn guest_reboot_stops_the_vmm() {
    if !environment_ready("guest_reboot", &["bzImage", "initramfs.cpio.gz"]) {
        return;
    }
    let mut vm = Vm::start("guest_reboot", &initramfs_args(2));
    vm.wait_for("minivmm initramfs ready", BOOT_TIMEOUT);
    vm.send("reboot -f\n");
    let status = vm.wait_exit(BOOT_TIMEOUT);
    assert!(status.success(), "exit status {status}");
    assert!(vm.output().contains("Reboot"));
}

#[test]
fn host_sigterm_stops_the_vmm() {
    if !environment_ready("host_sigterm", &["bzImage", "initramfs.cpio.gz"]) {
        return;
    }
    let mut vm = Vm::start("host_sigterm", &initramfs_args(2));
    vm.wait_for("minivmm initramfs ready", BOOT_TIMEOUT);
    // SAFETY: sending a signal to our own child process.
    assert_eq!(unsafe { libc::kill(vm.pid() as i32, libc::SIGTERM) }, 0);
    let status = vm.wait_exit(Duration::from_secs(10));
    assert!(status.success(), "exit status {status}");
    assert!(vm.output().contains("HostRequest"));
}

#[test]
fn guest_enumerates_pci_devices() {
    if !environment_ready("pci_enumeration", &["bzImage", "rootfs.ext4"]) {
        return;
    }
    let disk = common::copy_rootfs("pci_enumeration");
    let mut vm = Vm::start("pci_enumeration", &common::rootfs_args(&disk, 1));
    vm.wait_for("minivmm rootfs ready", BOOT_TIMEOUT);
    let devices = vm.run("cat /sys/bus/pci/devices/*/vendor /sys/bus/pci/devices/*/device");
    for id in ["0x1b36", "0x0008", "0x1af4", "0x1042"] {
        assert!(devices.contains(id), "missing PCI id {id}: {devices}");
    }
    let msix = vm.run("grep -c PCI-MSIX /proc/interrupts");
    assert!(!msix.contains("\n0\r"), "virtio should use MSI-X: {msix}");
    vm.send("poweroff\n");
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
}

#[test]
fn memory_above_the_mmio_gap_is_usable() {
    if !environment_ready("large_memory", &["bzImage", "initramfs.cpio.gz"]) {
        return;
    }
    let mut args = initramfs_args(2);
    let mem = args.iter().position(|a| a == "--mem").unwrap() + 1;
    args[mem] = "4G".into();
    let mut vm = Vm::start("large_memory", &args);
    vm.wait_for(
        "BIOS-e820: [mem 0x0000000100000000-0x000000013fffffff] usable",
        BOOT_TIMEOUT,
    );
    vm.wait_for("minivmm initramfs ready", BOOT_TIMEOUT);
    let total = vm.run("awk '/MemTotal/ { print int($2 / 1024) }' /proc/meminfo");
    let mib: u64 = total
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .next()
        .unwrap_or(0);
    assert!(mib > 3800, "guest sees only {mib} MiB: {total}");
    vm.send("poweroff -f\n");
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
}

#[test]
fn console_uses_raw_mode_and_restores_the_terminal() {
    if !environment_ready("raw_terminal", &["bzImage", "initramfs.cpio.gz"]) {
        return;
    }
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty fills in two valid descriptors on success.
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(ret, 0, "openpty failed");
    let canonical = |fd| {
        // SAFETY: termios is plain data filled in by tcgetattr on a valid tty.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd is an open terminal descriptor.
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut termios) }, 0);
        termios.c_lflag & libc::ICANON != 0
    };
    assert!(canonical(slave));
    let mut vm = Vm::start_with_stdin("raw_terminal", &initramfs_args(1), slave);
    vm.wait_for("minivmm initramfs ready", BOOT_TIMEOUT);
    assert!(
        !canonical(slave),
        "stdin should be in raw mode while the guest runs"
    );
    let command = b"poweroff -f\n";
    assert_eq!(
        // SAFETY: writing a command into the pty master that feeds the VMM's stdin.
        unsafe { libc::write(master, command.as_ptr().cast(), command.len()) },
        command.len() as isize
    );
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
    assert!(canonical(slave), "terminal settings were not restored");
    // SAFETY: closing the descriptors opened above.
    unsafe {
        libc::close(master);
        libc::close(slave);
    }
}
