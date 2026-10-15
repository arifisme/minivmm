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
