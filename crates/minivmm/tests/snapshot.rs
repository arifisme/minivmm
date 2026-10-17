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

fn restore_args(snapshot: &std::path::Path, socket: &std::path::Path) -> Vec<String> {
    vec![
        "--restore".into(),
        snapshot.display().to_string(),
        "--api".into(),
        socket.display().to_string(),
        "--log-level".into(),
        "info".into(),
    ]
}

fn restore_micros(socket: &std::path::Path) -> u64 {
    let status = api_request(socket, r#"{"cmd":"status"}"#);
    let field = status
        .split("\"restore_us\":")
        .nth(1)
        .unwrap_or_else(|| panic!("no restore time in {status}"));
    field
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_else(|| panic!("bad restore time in {status}"))
}

#[test]
fn clones_restore_independently_from_one_snapshot() {
    if !environment_ready("clones", &["bzImage", "rootfs.ext4"]) {
        return;
    }
    let disk = copy_rootfs("clones");
    let socket = scratch_file("clones-source.sock");
    let snapshot = scratch_file("clones.snap");
    let mut args = common::kernel_args("console=ttyS0 reboot=k panic=1 root=/dev/vda ro", 1);
    args.extend([
        "--disk".into(),
        disk.display().to_string(),
        "--disk-read-only".into(),
        "--api".into(),
        socket.display().to_string(),
    ]);
    let mut source = Vm::start("clones_source", &args);
    wait_for_path(&socket, COMMAND_TIMEOUT);
    source.wait_for("minivmm rootfs ready", BOOT_TIMEOUT);
    source.run("echo shared > /tmp/origin");
    let response = api_request(
        &socket,
        &format!(r#"{{"cmd":"snapshot","path":"{}"}}"#, snapshot.display()),
    );
    assert!(response.contains(r#""ok":true"#), "{response}");
    api_request(&socket, r#"{"cmd":"shutdown"}"#);
    assert!(source.wait_exit(BOOT_TIMEOUT).success());
    drop(source);

    let memory = minivmm::snapshot::default_memory_path(&snapshot);
    let allocated = memory.metadata().unwrap();
    assert!(
        std::os::unix::fs::MetadataExt::blocks(&allocated) * 512 < allocated.len(),
        "the memory file should be sparse"
    );

    let sockets = [scratch_file("clone-a.sock"), scratch_file("clone-b.sock")];
    let mut clones: Vec<Vm> = sockets
        .iter()
        .enumerate()
        .map(|(index, socket)| {
            let vm = Vm::start(&format!("clone_{index}"), &restore_args(&snapshot, socket));
            wait_for_path(socket, COMMAND_TIMEOUT);
            vm
        })
        .collect();
    for (index, clone) in clones.iter_mut().enumerate() {
        let origin = clone.run("cat /tmp/origin");
        assert!(origin.contains("shared"), "{origin}");
        clone.run(&format!("echo clone-{index} > /tmp/identity"));
    }
    for (index, clone) in clones.iter_mut().enumerate() {
        let identity = clone.run("cat /tmp/identity");
        assert!(identity.contains(&format!("clone-{index}")), "{identity}");
    }
    for (clone, socket) in clones.iter().zip(&sockets) {
        let (rss, pss) = common::memory_usage_kib(clone.pid());
        println!(
            "clone memory: rss {rss} KiB, pss {pss} KiB, restore {} us",
            restore_micros(socket)
        );
        assert!(pss < rss, "clones should share unmodified pages");
    }
    for mut clone in clones {
        clone.send("poweroff -f\n");
        assert!(clone.wait_exit(BOOT_TIMEOUT).success());
    }

    let mut latencies: Vec<u64> = (0..10)
        .map(|round| {
            let socket = scratch_file("clone-timing.sock");
            let mut clone = Vm::start(
                &format!("clone_timing_{round}"),
                &restore_args(&snapshot, &socket),
            );
            wait_for_path(&socket, COMMAND_TIMEOUT);
            let micros = restore_micros(&socket);
            api_request(&socket, r#"{"cmd":"shutdown"}"#);
            assert!(clone.wait_exit(BOOT_TIMEOUT).success());
            micros
        })
        .collect();
    latencies.sort_unstable();
    println!(
        "restore latency over {} runs: min {} us, median {} us, max {} us",
        latencies.len(),
        latencies[0],
        latencies[latencies.len() / 2],
        latencies[latencies.len() - 1]
    );
}
