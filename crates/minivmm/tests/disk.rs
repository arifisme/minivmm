mod common;

use common::{BOOT_TIMEOUT, Vm, copy_rootfs, environment_ready, rootfs_args};

#[test]
fn ext4_root_persists_writes_across_reboot() {
    if !environment_ready("disk_persistence", &["bzImage", "rootfs.ext4"]) {
        return;
    }
    let disk = copy_rootfs("disk_persistence");
    let mut vm = Vm::start("disk_persistence_1", &rootfs_args(&disk, 2));
    vm.wait_for("minivmm rootfs ready", BOOT_TIMEOUT);
    let mounts = vm.run("grep ' / ' /proc/mounts");
    assert!(
        mounts.contains("/dev/root") || mounts.contains("/dev/vda"),
        "{mounts}"
    );
    assert!(mounts.contains("ext4"), "{mounts}");
    vm.run("dd if=/dev/urandom of=/root/blob bs=1k count=2048 2>/dev/null");
    let checksum = vm.run("md5sum /root/blob");
    vm.run("echo persisted-data > /root/marker && sync");
    let cache = vm.run("cat /sys/block/vda/queue/write_cache");
    assert!(
        cache.contains("write back"),
        "flush was not negotiated: {cache}"
    );
    let serial = vm.run("cat /sys/block/vda/serial");
    assert!(serial.contains("minivmm-"), "{serial}");
    vm.send("reboot\n");
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());

    let mut vm = Vm::start("disk_persistence_2", &rootfs_args(&disk, 2));
    vm.wait_for("minivmm rootfs ready", BOOT_TIMEOUT);
    let marker = vm.run("cat /root/marker");
    assert!(marker.contains("persisted-data"), "{marker}");
    let again = vm.run("md5sum /root/blob");
    let digest = |text: &str| {
        text.split_whitespace()
            .find(|w| w.len() == 32)
            .map(str::to_string)
    };
    assert_eq!(digest(&checksum), digest(&again));
    vm.send("poweroff\n");
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
}

#[test]
fn read_only_disk_rejects_writes() {
    if !environment_ready("disk_read_only", &["bzImage", "rootfs.ext4"]) {
        return;
    }
    let disk = copy_rootfs("disk_read_only");
    let before = std::fs::read(&disk).unwrap();
    let mut args = common::kernel_args("console=ttyS0 reboot=k panic=1 root=/dev/vda ro", 1);
    args.extend([
        "--disk".into(),
        disk.display().to_string(),
        "--disk-read-only".into(),
    ]);
    let mut vm = Vm::start("disk_read_only", &args);
    vm.wait_for("minivmm rootfs ready", BOOT_TIMEOUT);
    let flag = vm.run("cat /sys/block/vda/ro");
    assert!(flag.contains("\n1"), "{flag}");
    vm.run("! touch /root/should-fail 2>/dev/null");
    vm.run("dd if=/dev/vda of=/dev/null bs=1M count=8 2>/dev/null");
    vm.send("poweroff -f\n");
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
    assert!(
        std::fs::read(&disk).unwrap() == before,
        "read-only disk was modified"
    );
}
