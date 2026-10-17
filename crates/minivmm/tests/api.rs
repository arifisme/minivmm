mod common;

use common::{
    BOOT_TIMEOUT, COMMAND_TIMEOUT, Vm, api_request, artifact, environment_ready, kernel_args,
    scratch_file, wait_for_path,
};

#[test]
fn control_socket_pauses_resumes_and_shuts_down() {
    if !environment_ready("api", &["bzImage", "initramfs.cpio.gz"]) {
        return;
    }
    let socket = scratch_file("api.sock");
    let mut args = kernel_args("console=ttyS0 reboot=k panic=1", 2);
    args.extend([
        "--initrd".into(),
        artifact("initramfs.cpio.gz").display().to_string(),
        "--api".into(),
        socket.display().to_string(),
    ]);
    let mut vm = Vm::start("api", &args);
    wait_for_path(&socket, COMMAND_TIMEOUT);
    vm.wait_for("minivmm initramfs ready", BOOT_TIMEOUT);

    let status = api_request(&socket, r#"{"cmd":"status"}"#);
    assert!(status.contains(r#""state":"running""#), "{status}");
    assert!(status.contains(r#""vcpus":2"#), "{status}");

    let paused = api_request(&socket, r#"{"cmd":"pause"}"#);
    assert!(paused.contains(r#""state":"paused""#), "{paused}");
    vm.send("echo during-pause\n");
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(
        !vm.output().contains("\nduring-pause"),
        "guest ran while paused"
    );
    let resumed = api_request(&socket, r#"{"cmd":"resume"}"#);
    assert!(resumed.contains(r#""state":"running""#), "{resumed}");
    vm.wait_for("\nduring-pause", COMMAND_TIMEOUT);
    vm.run("true");

    let bad = api_request(&socket, r#"{"cmd":"explode"}"#);
    assert!(bad.contains(r#""ok":false"#), "{bad}");
    let malformed = api_request(&socket, "not json");
    assert!(malformed.contains(r#""ok":false"#), "{malformed}");

    let bye = api_request(&socket, r#"{"cmd":"shutdown"}"#);
    assert!(bye.contains(r#""ok":true"#), "{bye}");
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
    assert!(!socket.exists(), "API socket should be removed on exit");
}

#[test]
fn runs_with_stdin_from_dev_null() {
    if !environment_ready("api_null_stdin", &["bzImage", "initramfs.cpio.gz"]) {
        return;
    }
    let socket = scratch_file("null-stdin.sock");
    let mut args = kernel_args("console=ttyS0 reboot=k panic=1", 1);
    args.extend([
        "--initrd".into(),
        artifact("initramfs.cpio.gz").display().to_string(),
        "--api".into(),
        socket.display().to_string(),
    ]);
    let mut vm = Vm::start_without_stdin("api_null_stdin", &args);
    wait_for_path(&socket, COMMAND_TIMEOUT);
    vm.wait_for("minivmm initramfs ready", BOOT_TIMEOUT);
    let status = api_request(&socket, r#"{"cmd":"status"}"#);
    assert!(status.contains(r#""state":"running""#), "{status}");
    api_request(&socket, r#"{"cmd":"shutdown"}"#);
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
}
