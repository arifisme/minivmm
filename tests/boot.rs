mod common;

use common::{BOOT_TIMEOUT, Vm, artifact, environment_ready};

fn initramfs_args(cpus: u8) -> Vec<String> {
    vec![
        "--kernel".into(),
        artifact("bzImage").display().to_string(),
        "--initrd".into(),
        artifact("initramfs.cpio.gz").display().to_string(),
        "--cmdline".into(),
        "console=ttyS0 reboot=k panic=1 earlyprintk=serial loglevel=8".into(),
        "--cpus".into(),
        cpus.to_string(),
        "--mem".into(),
        "256M".into(),
        "--log-level".into(),
        "debug".into(),
    ]
}

#[test]
fn boots_initramfs_with_all_cpus_and_powers_off() {
    if !environment_ready("boots_initramfs", &["bzImage", "initramfs.cpio.gz"]) {
        return;
    }
    let mut vm = Vm::start("boots_initramfs", &initramfs_args(2));
    vm.wait_for("minivmm initramfs ready", BOOT_TIMEOUT);
    let out = vm.run("grep -c ^processor /proc/cpuinfo");
    assert!(out.contains("\n2"), "expected 2 CPUs: {out}");
    vm.send("poweroff -f\n");
    let status = vm.wait_exit(BOOT_TIMEOUT);
    assert!(status.success(), "exit status {status}");
}
