mod common;

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::{Daemon, artifacts, native_config, now_ms, python_config, report};
use serde_json::{Value, json};

const BROKEN_PYTHON: &str = "import does_not_exist\n\ndef handler(event, context):\n    return 1\n";

fn log_lines(daemon: &Daemon, function: &str, query: &str) -> Vec<Value> {
    daemon
        .get(&format!("/v1/functions/{function}/logs?{query}"))
        .body
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn has_line(lines: &[Value], text: &str) -> bool {
    lines.iter().any(|entry| {
        entry["line"]
            .as_str()
            .is_some_and(|line| line.contains(text))
    })
}

#[test]
fn native_functions_deploy_invoke_and_delete() {
    let Some(artifacts) = artifacts("native_functions") else {
        return;
    };
    let daemon = Daemon::start("native", artifacts, &[]);
    let deployed = daemon.deploy_ok("echo", native_config(json!({})), daemon.native_code());
    assert_eq!(deployed.body["revision"], 1);
    assert_eq!(deployed.body["config"]["runtime"], "native");
    report(&format!("native deploy took {:?}", deployed.elapsed));

    let cold = daemon.invoke_ok("echo", json!({"hello": "world"}));
    assert_eq!(cold.body["echo"]["hello"], "world");
    assert_eq!(cold.header("x-mv-start").as_deref(), Some("cold"));
    assert!(cold.header("x-mv-restore-ms").is_some());
    let request_id = cold.header("x-mv-request-id").unwrap();
    let warm = daemon.invoke_ok("echo", json!({"n": 2}));
    assert_eq!(warm.header("x-mv-start").as_deref(), Some("warm"));
    assert_eq!(warm.header("x-mv-vm"), cold.header("x-mv-vm"));
    assert_eq!(warm.body["invocations"], 2);
    report(&format!(
        "native cold {} ms (restore {} ms), warm {} ms",
        cold.header("x-mv-duration-ms").unwrap(),
        cold.header("x-mv-restore-ms").unwrap(),
        warm.header("x-mv-duration-ms").unwrap()
    ));

    let logged = daemon.invoke_ok("echo", json!({"action": "log", "lines": 3}));
    let logs = log_lines(
        &daemon,
        "echo",
        &format!("request_id={}", logged.header("x-mv-request-id").unwrap()),
    );
    assert_eq!(logs.len(), 3, "{logs:?}");
    assert!(has_line(
        &log_lines(&daemon, "echo", "limit=500"),
        "native example initialized"
    ));
    assert!(log_lines(&daemon, "echo", &format!("request_id={request_id}")).is_empty());

    let failed = daemon.invoke("echo", json!({"action": "fail"}));
    assert_eq!(failed.status, 500);
    assert_eq!(
        failed.header("x-mv-function-error").as_deref(),
        Some("unhandled")
    );
    assert_eq!(failed.body["type"], "FunctionError");
    assert_eq!(
        daemon
            .invoke("echo", json!({"action": "echo"}))
            .header("x-mv-start")
            .as_deref(),
        Some("warm")
    );
    let bad_event = daemon.invoke("echo", Value::String("{oops".into()));
    assert_eq!(bad_event.status, 200, "a JSON string is a valid event");
    assert_eq!(daemon.invoke("missing", json!({})).status, 404);

    let listed = daemon.get("/v1/functions");
    assert_eq!(listed.body.as_array().unwrap().len(), 1);
    let described = daemon.get("/v1/functions/echo");
    assert_eq!(described.body["pool"]["idle"], 1);
    let metrics = daemon.metrics();
    assert!(
        metrics.contains(r#"mvd_invocations_total{function="echo",start="cold"} 1"#),
        "{metrics}"
    );
    assert!(
        metrics.contains(r#"mvd_invocation_errors_total{function="echo",kind="function_error"} 1"#)
    );
    assert!(metrics.contains("mvd_restore_seconds_bucket"));
    assert!(metrics.contains(r#"mvd_pool_vms{function="echo",state="idle"} 1"#));

    let broken = daemon.deploy(
        "broken",
        native_config(json!({"env": {"INIT_FAIL": "1"}})),
        daemon.native_code(),
    );
    assert_eq!(broken.status, 422, "{}", broken.body);
    assert_eq!(broken.body["kind"], "init_failed");
    assert!(broken.body["error"].as_str().unwrap().contains("INIT_FAIL"));
    assert!(
        broken.body["logs"]
            .to_string()
            .contains("initialization failed on purpose")
    );
    assert_eq!(daemon.get("/v1/functions/broken").status, 404);
    let not_an_archive = daemon.deploy(
        "garbage",
        native_config(json!({})),
        b"not a tarball".to_vec(),
    );
    assert_eq!(not_an_archive.status, 422);
    assert_eq!(not_an_archive.body["kind"], "load_failed");
    let invalid = daemon.deploy("Bad Name", native_config(json!({})), daemon.native_code());
    assert_eq!(invalid.status, 400);

    let vm = daemon.vms("echo");
    assert_eq!(vm.len(), 1);
    let jail = vm[0]["jail"].as_str().unwrap().to_string();
    assert_eq!(daemon.delete("/v1/functions/echo").status, 204);
    assert_eq!(daemon.get("/v1/functions/echo").status, 404);
    daemon.wait_until("the VM is gone", Duration::from_secs(10), || {
        daemon.vms("echo").is_empty() && !std::path::Path::new(&jail).exists()
    });
    daemon.assert_no_leaks();
    let entries = fs::read_dir(daemon.data_dir.join("jails")).unwrap().count();
    assert_eq!(entries, 0, "build and failed-deploy jails are removed too");
}

#[test]
fn python_functions_keep_state_and_report_errors() {
    let Some(artifacts) = artifacts("python_functions") else {
        return;
    };
    let daemon = Daemon::start("python", artifacts, &[]);
    let deployed = daemon.deploy_ok(
        "py",
        python_config(json!({"env": {"GREETING": "hi"}})),
        daemon.python_example(),
    );
    report(&format!(
        "python deploy took {:?}, init {} ms",
        deployed.elapsed, deployed.body["init_duration_ms"]
    ));
    let cold = daemon.invoke_ok("py", json!({"action": "echo", "x": 1}));
    assert_eq!(cold.body["echo"]["x"], 1);
    assert_eq!(cold.body["invocations"], 1);
    assert_eq!(
        cold.body["request_id"].as_str(),
        cold.header("x-mv-request-id").as_deref()
    );
    let warm = daemon.invoke_ok("py", json!({"action": "echo"}));
    assert_eq!(warm.body["invocations"], 2);
    report(&format!(
        "python cold {} ms (restore {} ms), warm {} ms",
        cold.header("x-mv-duration-ms").unwrap(),
        cold.header("x-mv-restore-ms").unwrap(),
        warm.header("x-mv-duration-ms").unwrap()
    ));
    let env = daemon.invoke_ok("py", json!({"action": "env"}));
    assert_eq!(env.body["env"]["GREETING"], "hi");
    assert_eq!(env.body["env"]["MV_FUNCTION_NAME"], "py");
    let remaining = daemon.invoke_ok("py", json!({"action": "remaining"}));
    let left = remaining.body["remaining_ms"].as_u64().unwrap();
    assert!(left > 3000 && left <= 5000, "{left}");

    let raised = daemon.invoke("py", json!({"action": "fail"}));
    assert_eq!(raised.status, 500);
    assert_eq!(raised.body["type"], "ValueError");
    assert!(raised.body["trace"].to_string().contains("main.py"));
    let after = daemon.invoke_ok("py", json!({"action": "echo"}));
    assert_eq!(after.header("x-mv-start").as_deref(), Some("warm"));

    let logged = daemon.invoke_ok("py", json!({"action": "log", "lines": 2}));
    let id = logged.header("x-mv-request-id").unwrap();
    let lines = log_lines(&daemon, "py", &format!("request_id={id}"));
    assert!(has_line(&lines, "log line 1"), "{lines:?}");
    assert!(has_line(
        &log_lines(&daemon, "py", ""),
        "python example initialized"
    ));

    let broken = daemon.deploy(
        "broken-py",
        python_config(json!({})),
        daemon.python_code(BROKEN_PYTHON),
    );
    assert_eq!(broken.status, 422);
    assert!(
        broken.body["error"]
            .as_str()
            .unwrap()
            .contains("ModuleNotFoundError"),
        "{}",
        broken.body
    );
    assert!(broken.body["logs"].to_string().contains("does_not_exist"));

    let redeployed = daemon.deploy_ok(
        "py",
        python_config(json!({"env": {"GREETING": "hello"}})),
        daemon.python_example(),
    );
    assert_eq!(redeployed.body["revision"], 2);
    let fresh = daemon.invoke_ok("py", json!({"action": "env"}));
    assert_eq!(fresh.header("x-mv-start").as_deref(), Some("cold"));
    assert_eq!(fresh.body["env"]["GREETING"], "hello");
}

#[test]
fn concurrent_invocations_scale_out_to_the_limit() {
    let Some(artifacts) = artifacts("scale_out") else {
        return;
    };
    let daemon = Arc::new(Daemon::start(
        "scale",
        artifacts,
        &["--queue-timeout", "100ms"],
    ));
    daemon.deploy_ok(
        "wide",
        native_config(json!({"max_concurrency": 4})),
        daemon.native_code(),
    );
    let run = |count: usize| {
        let handles: Vec<_> = (0..count)
            .map(|_| {
                let daemon = daemon.clone();
                thread::spawn(move || daemon.invoke("wide", json!({"action": "sleep", "ms": 1500})))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    };
    let replies = run(4);
    assert!(replies.iter().all(|r| r.status == 200));
    let vms: HashSet<String> = replies.iter().filter_map(|r| r.header("x-mv-vm")).collect();
    assert_eq!(vms.len(), 4, "every concurrent request gets its own VM");
    assert_eq!(daemon.get("/v1/functions/wide").body["pool"]["idle"], 4);

    let replies = run(6);
    let ok = replies.iter().filter(|r| r.status == 200).count();
    let throttled: Vec<_> = replies.iter().filter(|r| r.status == 429).collect();
    assert_eq!(
        ok,
        4,
        "{:?}",
        replies.iter().map(|r| r.status).collect::<Vec<_>>()
    );
    assert_eq!(throttled.len(), 2);
    assert_eq!(throttled[0].body["kind"], "concurrency");
    assert!(
        replies
            .iter()
            .filter(|r| r.status == 200)
            .all(|r| r.header("x-mv-start").as_deref() == Some("warm"))
    );
    assert!(
        daemon
            .metrics()
            .contains(r#"mvd_throttled_total{function="wide",reason="concurrency"} 2"#)
    );
}

#[test]
fn idle_vms_scale_to_zero_and_come_back() {
    let Some(artifacts) = artifacts("scale_to_zero") else {
        return;
    };
    let daemon = Daemon::start("zero", artifacts, &["--idle-timeout", "1s"]);
    daemon.deploy_ok("napper", native_config(json!({})), daemon.native_code());
    let first = daemon.invoke_ok("napper", json!({}));
    assert_eq!(first.header("x-mv-start").as_deref(), Some("cold"));
    assert_eq!(daemon.vms("napper").len(), 1);
    let pid = daemon.vms("napper")[0]["pid"].as_u64().unwrap();
    daemon.wait_until("the idle VM is stopped", Duration::from_secs(10), || {
        daemon.vms("napper").is_empty()
    });
    assert_eq!(
        daemon.get("/v1/functions/napper").body["pool"],
        json!({"idle": 0, "busy": 0, "starting": 0})
    );
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    let again = daemon.invoke_ok("napper", json!({}));
    assert_eq!(again.header("x-mv-start").as_deref(), Some("cold"));
    assert_eq!(again.body["invocations"], 1);
    daemon.assert_no_leaks();
}

#[test]
fn timeouts_kill_the_vm() {
    let Some(artifacts) = artifacts("timeouts") else {
        return;
    };
    let daemon = Daemon::start("timeout", artifacts, &[]);
    daemon.deploy_ok(
        "slow",
        native_config(json!({"timeout_ms": 500})),
        daemon.native_code(),
    );
    let started = Instant::now();
    let reply = daemon.invoke("slow", json!({"action": "sleep", "ms": 5000}));
    assert_eq!(reply.status, 504, "{}", reply.body);
    assert_eq!(reply.body["kind"], "timeout");
    assert!(started.elapsed() < Duration::from_secs(3));
    let vm = reply.header("x-mv-vm").unwrap();
    daemon.wait_until("the timed out VM is gone", Duration::from_secs(10), || {
        daemon.vms("slow").iter().all(|v| v["id"] != vm.as_str())
    });
    let next = daemon.invoke_ok("slow", json!({"action": "sleep", "ms": 10}));
    assert_eq!(next.header("x-mv-start").as_deref(), Some("cold"));
    assert!(
        daemon
            .metrics()
            .contains(r#"mvd_invocation_timeouts_total{function="slow"} 1"#)
    );
}

#[test]
fn guest_crashes_fail_only_their_request() {
    let Some(artifacts) = artifacts("guest_crash") else {
        return;
    };
    let daemon = Daemon::start("crash", artifacts, &[]);
    daemon.deploy_ok("fragile", native_config(json!({})), daemon.native_code());
    daemon.deploy_ok("exits", native_config(json!({})), daemon.native_code());
    daemon.invoke_ok("fragile", json!({}));
    let crashed = daemon.invoke("fragile", json!({"action": "crash"}));
    assert_eq!(crashed.status, 502, "{}", crashed.body);
    assert_eq!(crashed.body["kind"], "vm_crashed");
    let recovered = daemon.invoke_ok("fragile", json!({}));
    assert_eq!(recovered.header("x-mv-start").as_deref(), Some("cold"));

    let exited = daemon.invoke("exits", json!({"action": "exit"}));
    assert_eq!(exited.status, 502);
    assert_eq!(exited.body["kind"], "runtime_exited");
    assert!(exited.body["error"].as_str().unwrap().contains('7'));
    daemon.invoke_ok("exits", json!({}));
    daemon.assert_no_leaks();
}

#[test]
fn a_killed_vmm_fails_only_its_request() {
    let Some(artifacts) = artifacts("vmm_killed") else {
        return;
    };
    let daemon = Arc::new(Daemon::start("killed", artifacts, &[]));
    daemon.deploy_ok(
        "victim",
        native_config(json!({"max_concurrency": 2})),
        daemon.native_code(),
    );
    daemon.invoke_ok("victim", json!({}));
    let pending = {
        let daemon = daemon.clone();
        thread::spawn(move || daemon.invoke("victim", json!({"action": "sleep", "ms": 4000})))
    };
    let mut pid = None;
    daemon.wait_until("the invocation is running", Duration::from_secs(10), || {
        pid = daemon
            .vms("victim")
            .iter()
            .find(|vm| vm["state"] == "busy")
            .and_then(|vm| vm["pid"].as_u64());
        pid.is_some()
    });
    // SAFETY: killing the VMM process the platform reported as busy.
    unsafe { libc::kill(pid.unwrap() as i32, libc::SIGKILL) };
    let reply = pending.join().unwrap();
    assert_eq!(reply.status, 502, "{}", reply.body);
    assert_eq!(reply.body["kind"], "vm_crashed");
    assert!(reply.elapsed < Duration::from_secs(4));
    let next = daemon.invoke_ok("victim", json!({}));
    assert_eq!(next.header("x-mv-start").as_deref(), Some("cold"));
    daemon.assert_no_leaks();
}

#[test]
fn functions_survive_a_daemon_restart() {
    let Some(artifacts) = artifacts("restart") else {
        return;
    };
    let mut daemon = Daemon::start("restart", artifacts, &[]);
    let deployed = daemon.deploy_ok(
        "keeper",
        native_config(json!({"env": {"KEPT": "yes"}})),
        daemon.native_code(),
    );
    daemon.deploy_ok(
        "pykeeper",
        python_config(json!({})),
        daemon.python_example(),
    );
    daemon.invoke_ok("keeper", json!({}));
    let pid = daemon.vms("keeper")[0]["pid"].as_u64().unwrap();
    daemon.restart();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "VMs stop with the daemon"
    );
    let listed = daemon.get("/v1/functions");
    let names: Vec<&str> = listed
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["keeper", "pykeeper"]);
    let described = daemon.get("/v1/functions/keeper");
    assert_eq!(described.body["code_sha256"], deployed.body["code_sha256"]);
    assert_eq!(described.body["revision"], 1);
    let env = daemon.invoke_ok("keeper", json!({"action": "env"}));
    assert_eq!(env.body["env"]["KEPT"], "yes");
    assert_eq!(env.header("x-mv-start").as_deref(), Some("cold"));
    daemon.invoke_ok("pykeeper", json!({"action": "echo"}));
    let redeployed = daemon.deploy_ok("keeper", native_config(json!({})), daemon.native_code());
    assert_eq!(redeployed.body["revision"], 2);
}

fn process_uid(pid: u64) -> u32 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
        .unwrap()
}

#[test]
fn functions_are_isolated_from_each_other() {
    let Some(artifacts) = artifacts("isolation") else {
        return;
    };
    let daemon = Daemon::start("isolation", artifacts, &[]);
    daemon.deploy_ok(
        "alpha",
        native_config(json!({"env": {"SECRET": "alpha-secret"}})),
        daemon.native_code(),
    );
    daemon.deploy_ok(
        "beta",
        python_config(json!({"env": {"SECRET": "beta-secret"}})),
        daemon.python_example(),
    );
    let alpha_env = daemon.invoke_ok("alpha", json!({"action": "env"}));
    let beta_env = daemon.invoke_ok("beta", json!({"action": "env"}));
    assert_eq!(alpha_env.body["env"]["SECRET"], "alpha-secret");
    assert_eq!(beta_env.body["env"]["SECRET"], "beta-secret");
    assert!(!alpha_env.body.to_string().contains("beta-secret"));
    let code = daemon.invoke_ok(
        "alpha",
        json!({"action": "files", "path": "/run/mv/function"}),
    );
    assert_eq!(code.body["entries"], json!(["bootstrap"]));
    let python_code = daemon.invoke_ok(
        "beta",
        json!({"action": "files", "path": "/run/mv/function"}),
    );
    assert_eq!(python_code.body["entries"], json!(["main.py"]));

    let alpha = &daemon.vms("alpha")[0];
    let beta = &daemon.vms("beta")[0];
    let (alpha_pid, beta_pid) = (
        alpha["pid"].as_u64().unwrap(),
        beta["pid"].as_u64().unwrap(),
    );
    let (alpha_uid, beta_uid) = (process_uid(alpha_pid), process_uid(beta_pid));
    assert_ne!(alpha_uid, 0);
    assert_ne!(beta_uid, 0);
    assert_ne!(alpha_uid, beta_uid);
    assert_eq!(alpha["uid"].as_u64(), Some(u64::from(alpha_uid)));
    assert_ne!(alpha["cgroup"], beta["cgroup"]);
    let cgroup = alpha["cgroup"].as_str().unwrap();
    let procs = fs::read_to_string(format!("{cgroup}/cgroup.procs")).unwrap();
    assert!(procs.lines().any(|p| p == alpha_pid.to_string()));
    let memory_max: u64 = fs::read_to_string(format!("{cgroup}/memory.max"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(memory_max, (128 + 32) << 20);
    assert_eq!(
        fs::read_to_string(format!("{cgroup}/cpu.max"))
            .unwrap()
            .trim(),
        "100000 100000"
    );

    let mut root: Vec<String> = fs::read_dir(format!("/proc/{alpha_pid}/root"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    root.sort();
    assert_eq!(
        root,
        [
            "api.sock",
            "dev",
            "minivmm",
            "rootfs.ext4",
            "snapshot.mem",
            "snapshot.state",
            "v.sock"
        ]
    );
    let alpha_mounts = fs::read_to_string(format!("/proc/{alpha_pid}/mountinfo")).unwrap();
    assert!(
        !alpha_mounts.contains("functions/2/"),
        "alpha sees beta's files:\n{alpha_mounts}"
    );
    let host_mounts = fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(
        !host_mounts.contains("snapshot.state"),
        "jail mounts leaked to the host"
    );
    let beta_snapshot = daemon.data_dir.join("functions/2/rev-1/snapshot.mem");
    let metadata = fs::metadata(&beta_snapshot).unwrap();
    assert_eq!(metadata.uid(), beta_uid);
    assert_eq!(metadata.mode() & 0o777, 0o400);
    let alpha_net = fs::read_link(format!("/proc/{alpha_pid}/ns/net")).unwrap();
    assert_ne!(alpha_net, fs::read_link("/proc/self/ns/net").unwrap());
}

#[test]
fn restored_vms_get_fresh_entropy_and_the_current_time() {
    let Some(artifacts) = artifacts("entropy") else {
        return;
    };
    let daemon = Arc::new(Daemon::start("entropy", artifacts, &[]));
    daemon.deploy_ok(
        "native-rng",
        native_config(json!({"max_concurrency": 2})),
        daemon.native_code(),
    );
    daemon.deploy_ok(
        "python-rng",
        python_config(json!({"max_concurrency": 2})),
        daemon.python_example(),
    );
    thread::sleep(Duration::from_secs(3));
    for function in ["native-rng", "python-rng"] {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let daemon = daemon.clone();
                thread::spawn(move || {
                    let random = daemon.invoke_ok(function, json!({"action": "random", "ms": 500}));
                    let time = daemon.invoke_ok(function, json!({"action": "time"}));
                    (random, time, now_ms())
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let vms: HashSet<String> = results
            .iter()
            .filter_map(|(r, _, _)| r.header("x-mv-vm"))
            .collect();
        assert_eq!(
            vms.len(),
            2,
            "{function}: both requests need their own clone"
        );
        assert!(
            results
                .iter()
                .all(|(r, _, _)| r.header("x-mv-start").as_deref() == Some("cold"))
        );
        assert_ne!(
            results[0].0.body["random"], results[1].0.body["random"],
            "{function}: kernel entropy is shared"
        );
        if function == "python-rng" {
            assert_ne!(
                results[0].0.body["python_random"], results[1].0.body["python_random"],
                "python's random module is shared"
            );
        }
        for (_, time, host_after) in &results {
            let guest = time.body["unix_ms"].as_u64().unwrap();
            let drift = guest.abs_diff(*host_after);
            assert!(drift < 1000, "{function}: guest clock is {drift} ms off");
            report(&format!(
                "{function}: guest clock within {drift} ms of the host"
            ));
        }
    }
}

#[test]
fn a_short_load_test_succeeds() {
    let Some(artifacts) = artifacts("load") else {
        return;
    };
    let daemon = Arc::new(Daemon::start("load", artifacts, &[]));
    daemon.deploy_ok(
        "busy",
        native_config(json!({"max_concurrency": 8})),
        daemon.native_code(),
    );
    let started = Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|worker| {
            let daemon = daemon.clone();
            thread::spawn(move || {
                (0..50)
                    .map(|i| daemon.invoke("busy", json!({"worker": worker, "i": i})))
                    .map(|r| (r.status, r.elapsed))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut latencies: Vec<Duration> = Vec::new();
    for handle in handles {
        for (status, elapsed) in handle.join().unwrap() {
            assert_eq!(status, 200);
            latencies.push(elapsed);
        }
    }
    let elapsed = started.elapsed();
    latencies.sort();
    report(&format!(
        "load: {} requests in {:?} ({:.0} req/s), p50 {:?}, p99 {:?}",
        latencies.len(),
        elapsed,
        latencies.len() as f64 / elapsed.as_secs_f64(),
        latencies[latencies.len() / 2],
        latencies[latencies.len() * 99 / 100]
    ));
    assert_eq!(daemon.vms("busy").len(), 8);
}

#[test]
fn admission_control_refuses_to_overcommit() {
    let Some(artifacts) = artifacts("admission_memory") else {
        return;
    };
    let daemon = Arc::new(Daemon::start(
        "admission-memory",
        artifacts,
        &["--memory-limit-mib", "350", "--queue-timeout", "100ms"],
    ));
    daemon.deploy_ok(
        "hungry",
        native_config(json!({"max_concurrency": 4})),
        daemon.native_code(),
    );
    daemon.deploy_ok("other", native_config(json!({})), daemon.native_code());
    let burst: Vec<_> = (0..3)
        .map(|_| {
            let daemon = daemon.clone();
            thread::spawn(move || daemon.invoke("hungry", json!({"action": "sleep", "ms": 1500})))
        })
        .collect();
    let replies: Vec<_> = burst.into_iter().map(|h| h.join().unwrap()).collect();
    let statuses: Vec<u16> = replies.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        2,
        "{statuses:?}"
    );
    let refused = replies.iter().find(|r| r.status == 429).unwrap();
    assert_eq!(refused.body["kind"], "memory");
    assert_eq!(refused.header("retry-after").as_deref(), Some("1"));
    let status = daemon.get("/v1/status");
    assert_eq!(status.body["capacity"]["memory_reserved_mib"], 320);

    let evicting = daemon.invoke_ok("other", json!({}));
    assert_eq!(evicting.header("x-mv-start").as_deref(), Some("cold"));
    assert_eq!(
        daemon.vms("hungry").len(),
        1,
        "an idle VM was evicted to make room"
    );
    assert!(
        daemon
            .metrics()
            .contains(r#"mvd_throttled_total{function="hungry",reason="memory"} 1"#)
    );
    daemon.assert_no_leaks();
    drop(daemon);

    let Some(artifacts) = common::artifacts("admission_cpu") else {
        return;
    };
    let daemon = Arc::new(Daemon::start(
        "admission-cpu",
        artifacts,
        &["--vcpu-limit", "2"],
    ));
    daemon.deploy_ok(
        "wide",
        native_config(json!({"vcpus": 2, "max_concurrency": 4})),
        daemon.native_code(),
    );
    let burst: Vec<_> = (0..2)
        .map(|_| {
            let daemon = daemon.clone();
            thread::spawn(move || daemon.invoke("wide", json!({"action": "sleep", "ms": 1500})))
        })
        .collect();
    let replies: Vec<common::Reply> = burst.into_iter().map(|h| h.join().unwrap()).collect();
    let statuses: Vec<(u16, Value)> = replies
        .iter()
        .map(|r| (r.status, r.body["kind"].clone()))
        .collect();
    let bodies: Vec<&Value> = replies.iter().map(|r| &r.body).collect();
    assert!(
        statuses.contains(&(200, Value::Null)),
        "{bodies:?}\n{}",
        daemon.get("/v1/functions/wide/logs?limit=50").body
    );
    assert!(statuses.contains(&(429, json!("cpu"))), "{statuses:?}");
}

#[test]
fn the_cli_drives_the_platform() {
    let (Some(artifacts), Some(mvctl)) = (artifacts("cli"), common::mvctl()) else {
        return;
    };
    let daemon = Daemon::start("cli", artifacts, &[]);
    let dir = common::scratch("cli-function");
    fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/python/main.py"),
        dir.join("main.py"),
    )
    .unwrap();
    let run = |args: &[&str]| {
        let output = std::process::Command::new(&mvctl)
            .arg("--url")
            .arg(&daemon.url)
            .args(args)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (ok, out, err) = run(&[
        "deploy",
        "cli-fn",
        dir.to_str().unwrap(),
        "--runtime",
        "python",
        "--env",
        "GREETING=hey",
    ]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("deployed cli-fn revision 1"), "{out}");
    let (ok, out, err) = run(&[
        "invoke",
        "cli-fn",
        "--data",
        r#"{"action":"env"}"#,
        "--verbose",
    ]);
    assert!(ok, "{err}");
    assert!(out.contains("\"GREETING\": \"hey\""), "{out}");
    assert!(err.contains("start cold"), "{err}");
    let (ok, out, _) = run(&["list"]);
    assert!(
        ok && out.contains("cli-fn") && out.contains("python"),
        "{out}"
    );
    let (ok, out, _) = run(&["status"]);
    assert!(
        ok && out.contains("1 functions") && out.contains("cli-fn"),
        "{out}"
    );
    let (ok, out, _) = run(&["logs", "cli-fn"]);
    assert!(ok && out.contains("python example initialized"), "{out}");
    let (ok, _, err) = run(&["invoke", "cli-fn", "--data", r#"{"action":"fail"}"#]);
    assert!(!ok && err.contains("ValueError"), "{err}");
    let (ok, out, err) = run(&[
        "bench",
        "cli-fn",
        "--requests",
        "40",
        "--concurrency",
        "4",
        "--json",
    ]);
    assert!(ok, "{err}");
    let summary: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(summary["requests"], 40);
    assert_eq!(summary["errors"], 0);
    let (ok, _, err) = run(&["describe", "missing"]);
    assert!(!ok && err.contains("not_found"), "{err}");
    let (ok, out, _) = run(&["delete", "cli-fn"]);
    assert!(ok && out.contains("deleted cli-fn"));
}
