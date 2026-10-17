mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use common::{
    BOOT_TIMEOUT, COMMAND_TIMEOUT, Vm, api_request, copy_rootfs, environment_ready, scratch_file,
    wait_for_path,
};

const ECHO_PORT: u32 = 5000;
const READ_ONLY_CMDLINE: &str = "console=ttyS0 reboot=k panic=1 root=/dev/vda ro";

fn boot_args(disk: &Path, uds: &Path, api: &Path) -> Vec<String> {
    let mut args = common::kernel_args(READ_ONLY_CMDLINE, 2);
    args.extend([
        "--disk".into(),
        disk.display().to_string(),
        "--disk-read-only".into(),
        "--vsock-uds".into(),
        uds.display().to_string(),
        "--api".into(),
        api.display().to_string(),
    ]);
    args
}

fn connect(uds: &Path, port: u32) -> Option<UnixStream> {
    let mut stream = UnixStream::connect(uds).expect("connect to the vsock socket");
    stream.set_read_timeout(Some(COMMAND_TIMEOUT)).unwrap();
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    if line.is_empty() {
        return None;
    }
    assert!(line.starts_with("OK ") && line.ends_with('\n'), "{line:?}");
    Some(stream)
}

fn echo_round_trip(uds: &Path, len: usize) {
    let stream = connect(uds, ECHO_PORT).expect("the guest accepts the connection");
    let data: Vec<u8> = (0..len).map(|i| (i * 31 + i / 7) as u8).collect();
    let mut writer = stream.try_clone().unwrap();
    let sent = data.clone();
    let sender = thread::spawn(move || writer.write_all(&sent).unwrap());
    let mut echoed = vec![0u8; len];
    (&stream).read_exact(&mut echoed).unwrap();
    sender.join().unwrap();
    assert!(echoed == data, "echoed data differs");
}

fn start_echo(vm: &mut Vm) {
    vm.send(&format!("vsock-test echo {ECHO_PORT} &\n"));
    vm.wait_for(&format!("listening on {ECHO_PORT}"), COMMAND_TIMEOUT);
}

fn port_socket(uds: &Path, port: u32) -> PathBuf {
    let mut path = uds.as_os_str().to_owned();
    path.push(format!("_{port}"));
    PathBuf::from(path)
}

#[test]
fn vsock_carries_data_both_ways() {
    if !environment_ready("vsock", &["bzImage", "rootfs.ext4"]) {
        return;
    }
    let disk = copy_rootfs("vsock");
    let uds = scratch_file("vsock.sock");
    let api = scratch_file("vsock-api.sock");
    let mut vm = Vm::start("vsock", &boot_args(&disk, &uds, &api));
    wait_for_path(&uds, COMMAND_TIMEOUT);
    vm.wait_for("minivmm rootfs ready", BOOT_TIMEOUT);
    start_echo(&mut vm);

    echo_round_trip(&uds, 4 << 20);
    echo_round_trip(&uds, 1);
    assert!(connect(&uds, 5999).is_none(), "nobody listens on 5999");

    let host_port = port_socket(&uds, 6000);
    let _ = std::fs::remove_file(&host_port);
    let listener = UnixListener::bind(&host_port).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut message = String::new();
        stream.read_to_string(&mut message).unwrap();
        stream
            .write_all(format!("pong:{message}").as_bytes())
            .unwrap();
    });
    let reply = vm.run("vsock-test send 6000 hello");
    assert!(reply.contains("reply: pong:hello"), "{reply}");
    server.join().unwrap();
    let refused = vm.run("! vsock-test send 6001 nobody");
    assert!(refused.contains("vsock connect"), "{refused}");

    vm.send("poweroff -f\n");
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
}

#[test]
fn vsock_listeners_survive_restore_in_every_clone() {
    if !environment_ready("vsock_clones", &["bzImage", "rootfs.ext4"]) {
        return;
    }
    let disk = copy_rootfs("vsock_clones");
    let uds = scratch_file("vsock-source.sock");
    let api = scratch_file("vsock-source-api.sock");
    let snapshot = scratch_file("vsock.snap");
    let mut source = Vm::start("vsock_source", &boot_args(&disk, &uds, &api));
    wait_for_path(&api, COMMAND_TIMEOUT);
    source.wait_for("minivmm rootfs ready", BOOT_TIMEOUT);
    start_echo(&mut source);
    echo_round_trip(&uds, 64 << 10);
    let held_open = connect(&uds, ECHO_PORT);
    let response = api_request(
        &api,
        &format!(r#"{{"cmd":"snapshot","path":"{}"}}"#, snapshot.display()),
    );
    assert!(response.contains(r#""ok":true"#), "{response}");
    drop(held_open);
    api_request(&api, r#"{"cmd":"shutdown"}"#);
    assert!(source.wait_exit(BOOT_TIMEOUT).success());
    drop(source);

    let clones: Vec<(Vm, PathBuf)> = (0..2)
        .map(|index| {
            let uds = scratch_file(&format!("vsock-clone-{index}.sock"));
            let api = scratch_file(&format!("vsock-clone-{index}-api.sock"));
            let args = vec![
                "--restore".into(),
                snapshot.display().to_string(),
                "--vsock-uds".into(),
                uds.display().to_string(),
                "--api".into(),
                api.display().to_string(),
                "--log-level".into(),
                "info".into(),
            ];
            let vm = Vm::start(&format!("vsock_clone_{index}"), &args);
            wait_for_path(&uds, COMMAND_TIMEOUT);
            (vm, uds)
        })
        .collect();
    for (_, uds) in &clones {
        let mut attempts = 0;
        while connect(uds, ECHO_PORT).is_none() {
            attempts += 1;
            assert!(attempts < 50, "the restored guest never accepted");
            thread::sleep(Duration::from_millis(100));
        }
        echo_round_trip(uds, 1 << 20);
    }
    for (mut vm, _) in clones {
        vm.send("poweroff -f\n");
        assert!(vm.wait_exit(BOOT_TIMEOUT).success());
    }
}
