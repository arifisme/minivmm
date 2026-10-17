mod common;

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::{Daemon, Reply, artifacts, native_config, python_config, report};
use serde_json::{Value, json};

const COLD_RUNS: usize = 50;
const WARM_RUNS: usize = 500;
const DENSITY_VMS: usize = 200;

fn header_ms(reply: &Reply, name: &str) -> f64 {
    reply
        .header(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(f64::NAN)
}

fn percentile(values: &mut [f64], fraction: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    let index = ((values.len() as f64 * fraction).ceil() as usize).clamp(1, values.len()) - 1;
    values[index]
}

fn line(label: &str, values: &mut [f64]) {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    report(&format!(
        "BENCH {label:<34} n={:<4} p50 {:>8.2} ms  p90 {:>8.2} ms  p99 {:>8.2} ms  max {:>8.2} ms  mean {:>8.2} ms",
        values.len(),
        percentile(values, 0.5),
        percentile(values, 0.9),
        percentile(values, 0.99),
        percentile(values, 1.0),
        mean
    ));
}

fn cold_starts(daemon: &Daemon, function: &str, event: &Value) {
    let (mut client, mut server, mut restore, mut function_ms) = (vec![], vec![], vec![], vec![]);
    for _ in 0..COLD_RUNS {
        daemon.wait_until("the previous VM is reaped", Duration::from_secs(10), || {
            daemon.vms(function).is_empty()
        });
        let reply = daemon.invoke_ok(function, event.clone());
        assert_eq!(reply.header("x-mv-start").as_deref(), Some("cold"));
        client.push(reply.elapsed.as_secs_f64() * 1000.0);
        server.push(header_ms(&reply, "x-mv-duration-ms"));
        restore.push(header_ms(&reply, "x-mv-restore-ms"));
        function_ms.push(header_ms(&reply, "x-mv-function-ms"));
    }
    line(&format!("{function} cold client latency"), &mut client);
    line(&format!("{function} cold server time"), &mut server);
    line(&format!("{function} cold restore"), &mut restore);
    line(&format!("{function} cold function time"), &mut function_ms);
}

fn warm_invocations(daemon: &Daemon, function: &str, event: &Value) {
    daemon.invoke_ok(function, event.clone());
    let (mut client, mut server) = (vec![], vec![]);
    for _ in 0..WARM_RUNS {
        let reply = daemon.invoke_ok(function, event.clone());
        assert_eq!(reply.header("x-mv-start").as_deref(), Some("warm"));
        client.push(reply.elapsed.as_secs_f64() * 1000.0);
        server.push(header_ms(&reply, "x-mv-duration-ms"));
    }
    line(&format!("{function} warm client latency"), &mut client);
    line(&format!("{function} warm server time"), &mut server);
}

fn throughput(
    daemon: &Arc<Daemon>,
    function: &str,
    event: &Value,
    concurrency: usize,
    requests: usize,
) {
    let started = Instant::now();
    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let daemon = daemon.clone();
            let event = event.clone();
            let function = function.to_string();
            thread::spawn(move || {
                (0..requests / concurrency)
                    .map(|_| {
                        let reply = daemon.invoke(&function, event.clone());
                        (reply.status, reply.elapsed.as_secs_f64() * 1000.0)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let samples: Vec<(u16, f64)> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let elapsed = started.elapsed().as_secs_f64();
    let ok = samples.iter().filter(|(s, _)| *s == 200).count();
    let mut latencies: Vec<f64> = samples.iter().map(|(_, ms)| *ms).collect();
    report(&format!(
        "BENCH {function} throughput concurrency {concurrency:<3} {:.0} req/s ({ok}/{} ok)",
        ok as f64 / elapsed,
        samples.len()
    ));
    line(
        &format!("{function} latency at concurrency {concurrency}"),
        &mut latencies,
    );
}

fn available_mib(daemon: &Daemon) -> u64 {
    daemon.get("/v1/status").body["capacity"]["memory_available_mib"]
        .as_u64()
        .unwrap()
}

fn density(daemon: &Arc<Daemon>, function: &str, memory_mib: u64) {
    let before = available_mib(daemon);
    let started = Instant::now();
    let waves = DENSITY_VMS / 50;
    for _ in 0..waves {
        let handles: Vec<_> = (0..50)
            .map(|_| {
                let daemon = daemon.clone();
                let function = function.to_string();
                thread::spawn(move || {
                    daemon.invoke(&function, json!({"action": "sleep", "ms": 3000}))
                })
            })
            .collect();
        for handle in handles {
            let reply = handle.join().unwrap();
            assert_eq!(reply.status, 200, "{}", reply.body);
        }
    }
    let vms = daemon.vms(function);
    let count = vms.len();
    thread::sleep(Duration::from_secs(2));
    let after = available_mib(daemon);
    let vms = daemon.vms(function);
    let sum = |key: &str| -> u64 { vms.iter().filter_map(|vm| vm["memory"][key].as_u64()).sum() };
    let (rss, pss) = (sum("rss_kib"), sum("pss_kib"));
    report(&format!(
        "BENCH {function} density: {count} idle VMs of {memory_mib} MiB started in {:.1} s",
        started.elapsed().as_secs_f64()
    ));
    report(&format!(
        "BENCH {function} density: per VM rss {:.1} MiB, pss {:.1} MiB, host MemAvailable drop {:.1} MiB per VM ({} MiB total)",
        rss as f64 / count as f64 / 1024.0,
        pss as f64 / count as f64 / 1024.0,
        before.saturating_sub(after) as f64 / count as f64,
        before.saturating_sub(after)
    ));
    report(&format!(
        "BENCH {function} density: guest memory promised {} MiB, resident {:.0} MiB",
        memory_mib * count as u64,
        pss as f64 / 1024.0
    ));
}

#[test]
#[ignore = "run explicitly on a KVM host: it takes several minutes"]
fn benchmarks() {
    let Some(artifacts) = artifacts("benchmarks") else {
        return;
    };
    let daemon = Arc::new(Daemon::start(
        "bench",
        artifacts,
        &["--idle-timeout", "300ms", "--vcpu-limit", "64"],
    ));
    let echo = json!({"action": "echo"});
    daemon.deploy_ok(
        "native",
        native_config(json!({"max_concurrency": 32})),
        daemon.native_code(),
    );
    daemon.deploy_ok(
        "python",
        python_config(json!({"max_concurrency": 32})),
        daemon.python_example(),
    );
    for function in ["native", "python"] {
        cold_starts(&daemon, function, &echo);
        warm_invocations(&daemon, function, &echo);
        for concurrency in [1, 4, 16] {
            throughput(&daemon, function, &echo, concurrency, 1600);
        }
    }
    drop(daemon);

    let Some(artifacts) = common::artifacts("density") else {
        return;
    };
    let daemon = Arc::new(Daemon::start(
        "density",
        artifacts,
        &[
            "--idle-timeout",
            "10m",
            "--vcpu-limit",
            "1000",
            "--memory-overcommit",
            "4",
        ],
    ));
    let config = json!({"max_concurrency": DENSITY_VMS, "timeout_ms": 30000});
    daemon.deploy_ok(
        "native-density",
        native_config(config.clone()),
        daemon.native_code(),
    );
    daemon.deploy_ok(
        "python-density",
        python_config(config),
        daemon.python_example(),
    );
    density(&daemon, "native-density", 128);
    assert_eq!(daemon.delete("/v1/functions/native-density").status, 204);
    daemon.assert_no_leaks();
    density(&daemon, "python-density", 128);
}
