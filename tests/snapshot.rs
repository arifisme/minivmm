mod common;

use std::time::Duration;

use common::{
    BOOT_TIMEOUT, COMMAND_TIMEOUT, HOST_IP, Vm, api_request, copy_rootfs, environment_ready,
    rootfs_args, scratch_file, tap_ready, wait_for_path,
};

#[test]
fn snapshot_restores_in_a_fresh_process() {
    if !environment_ready("snapshot", &["bzImage", "rootfs.ext4"]) {
        return;
    }
    let tap = tap_ready("snapshot");
    let disk = copy_rootfs("snapshot");
    let socket = scratch_file("snapshot.sock");
    let snapshot = scratch_file("vm.snap");
    let _ = std::fs::remove_file(&snapshot);
    let mut args = rootfs_args(&disk, 2);
    args.extend(["--api".into(), socket.display().to_string()]);
    if let Some(tap) = &tap {
        args.extend(["--tap".into(), tap.clone()]);
    }
    let mut vm = Vm::start("snapshot_source", &args);
    wait_for_path(&socket, COMMAND_TIMEOUT);
    vm.wait_for("minivmm rootfs ready", BOOT_TIMEOUT);
    vm.run("echo before-snapshot > /tmp/state");
    vm.run("i=0; while [ $i -lt 20 ]; do i=$((i+1)); done");

    let response = api_request(
        &socket,
        &format!(r#"{{"cmd":"snapshot","path":"{}"}}"#, snapshot.display()),
    );
    assert!(response.contains(r#""ok":true"#), "{response}");
    assert!(response.contains(r#""state":"running""#), "{response}");
    assert!(snapshot.exists());
    vm.run("echo after-snapshot > /tmp/state");
    let bye = api_request(&socket, r#"{"cmd":"shutdown"}"#);
    assert!(bye.contains(r#""ok":true"#), "{bye}");
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
    drop(vm);

    let restore_args = vec![
        "--restore".into(),
        snapshot.display().to_string(),
        "--api".into(),
        socket.display().to_string(),
        "--log-level".into(),
        "info".into(),
    ];
    let mut restored = Vm::start("snapshot_restored", &restore_args);
    wait_for_path(&socket, COMMAND_TIMEOUT);
    let state = restored.run("cat /tmp/state");
    assert!(state.contains("before-snapshot"), "{state}");
    let cpus = restored.run("grep -c ^processor /proc/cpuinfo");
    assert!(cpus.contains("\n2"), "{cpus}");
    restored.run("echo restored-write > /root/after && sync && cat /root/after");
    restored.run("dd if=/root/after of=/dev/null 2>/dev/null");
    if tap.is_some() {
        let ping = restored.run_with_timeout(
            &format!("ping -c 3 -W 2 {HOST_IP}"),
            Duration::from_secs(30),
        );
        assert!(ping.contains("3 packets received"), "{ping}");
    }
    let status = api_request(&socket, r#"{"cmd":"status"}"#);
    assert!(status.contains(r#""state":"running""#), "{status}");
    restored.send("poweroff\n");
    assert!(restored.wait_exit(BOOT_TIMEOUT).success());
}
