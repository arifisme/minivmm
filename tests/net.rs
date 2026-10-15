mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use common::{
    BOOT_TIMEOUT, GUEST_IP, HOST_IP, Vm, copy_rootfs, environment_ready, rootfs_args, tap_ready,
};

fn connect_with_retry(addr: &str) -> TcpStream {
    for _ in 0..50 {
        if let Ok(stream) =
            TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(2))
        {
            return stream;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("could not connect to {addr}");
}

#[test]
fn guest_network_reaches_host_both_ways() {
    if !environment_ready("network", &["bzImage", "rootfs.ext4"]) {
        return;
    }
    let Some(tap) = tap_ready("network") else {
        return;
    };
    let disk = copy_rootfs("network");
    let mut args = rootfs_args(&disk, 2);
    args.extend(["--tap".into(), tap]);
    let mut vm = Vm::start("network", &args);
    vm.wait_for("minivmm rootfs ready", BOOT_TIMEOUT);

    let ping = vm.run(&format!("ping -c 3 -W 2 {HOST_IP}"));
    assert!(ping.contains("3 packets received"), "{ping}");

    let mut ssh = connect_with_retry(&format!("{GUEST_IP}:22"));
    ssh.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut banner = [0u8; 16];
    ssh.read_exact(&mut banner).unwrap();
    assert!(banner.starts_with(b"SSH-2.0-dropbear"), "{banner:?}");
    drop(ssh);

    vm.send("nc -l -p 5000 > /tmp/from-host &\n");
    std::thread::sleep(Duration::from_millis(500));
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut to_guest = connect_with_retry(&format!("{GUEST_IP}:5000"));
    to_guest.write_all(&payload).unwrap();
    drop(to_guest);
    std::thread::sleep(Duration::from_millis(500));
    let size = vm.run("wc -c < /tmp/from-host");
    assert!(size.contains(&payload.len().to_string()), "{size}");

    let listener = TcpListener::bind(format!("{HOST_IP}:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    vm.send(&format!(
        "dd if=/dev/zero bs=1k count=300 2>/dev/null | nc {HOST_IP} {port} &\n"
    ));
    let (mut from_guest, _) = listener.accept().unwrap();
    from_guest
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut received = Vec::new();
    from_guest.read_to_end(&mut received).unwrap();
    assert_eq!(received.len(), 300 * 1024);
    drop(from_guest);
    vm.run("wait");

    vm.send("poweroff\n");
    assert!(vm.wait_exit(BOOT_TIMEOUT).success());
}
