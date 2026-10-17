use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn random_hex(len: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; len];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn list_dir(path: &str) -> Value {
    match fs::read_dir(path) {
        Ok(entries) => {
            let mut names: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            json!(names)
        }
        Err(err) => json!({ "error": err.to_string() }),
    }
}

fn handle(event: &Value, invocations: u64) -> Result<Value, String> {
    let number = |key: &str| event.get(key).and_then(Value::as_u64).unwrap_or(0);
    match event
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("echo")
    {
        "echo" => {
            Ok(json!({ "echo": event, "invocations": invocations, "pid": std::process::id() }))
        }
        "sleep" => {
            thread::sleep(Duration::from_millis(number("ms")));
            Ok(json!({ "slept_ms": number("ms") }))
        }
        "spin" => {
            let started = Instant::now();
            let mut counter = 0u64;
            while started.elapsed() < Duration::from_millis(number("ms")) {
                counter = counter.wrapping_add(1);
            }
            Ok(json!({ "iterations": counter }))
        }
        "random" => {
            thread::sleep(Duration::from_millis(number("ms")));
            Ok(json!({ "random": random_hex(32).map_err(|e| e.to_string())? }))
        }
        "time" => Ok(json!({ "unix_ms": unix_ms() as u64 })),
        "env" => {
            let env: BTreeMap<String, String> = std::env::vars().collect();
            Ok(json!({ "env": env }))
        }
        "files" => {
            let path = event.get("path").and_then(Value::as_str).unwrap_or("/");
            Ok(json!({ "path": path, "entries": list_dir(path) }))
        }
        "read" => {
            let path = event.get("path").and_then(Value::as_str).unwrap_or("");
            Ok(match fs::read_to_string(path) {
                Ok(text) => json!({ "contents": text }),
                Err(err) => json!({ "error": err.to_string() }),
            })
        }
        "big" => Ok(json!({ "data": "x".repeat(number("bytes") as usize) })),
        "log" => {
            for i in 0..number("lines") {
                eprintln!("log line {i}");
            }
            Ok(json!({ "logged": number("lines") }))
        }
        "fail" => Err("the function was asked to fail".into()),
        "exit" => std::process::exit(7),
        "crash" => {
            let _ = fs::write("/proc/sysrq-trigger", "c");
            thread::sleep(Duration::from_secs(60));
            Err("the guest did not crash".into())
        }
        other => Err(format!("unknown action {other}")),
    }
}

fn reply(out: &mut impl Write, message: &Value) -> io::Result<()> {
    writeln!(out, "{message}")?;
    out.flush()
}

fn main() -> io::Result<()> {
    let mut out = io::stdout().lock();
    if let Ok(ms) = std::env::var("INIT_SLEEP_MS") {
        thread::sleep(Duration::from_millis(ms.parse().unwrap_or(0)));
    }
    if std::env::var("INIT_FAIL").is_ok() {
        eprintln!("initialization failed on purpose");
        reply(
            &mut out,
            &json!({ "error": { "type": "InitError", "message": "INIT_FAIL is set" } }),
        )?;
        std::process::exit(1);
    }
    eprintln!("native example initialized");
    reply(&mut out, &json!({ "ready": true }))?;
    let mut invocations = 0;
    for line in io::stdin().lock().lines() {
        let line = line?;
        invocations += 1;
        let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let event = request.get("event").cloned().unwrap_or(Value::Null);
        let message = match handle(&event, invocations) {
            Ok(response) => json!({ "response": response }),
            Err(message) => json!({ "error": { "type": "FunctionError", "message": message } }),
        };
        reply(&mut out, &message)?;
    }
    Ok(())
}
