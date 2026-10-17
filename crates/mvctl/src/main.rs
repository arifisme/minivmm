mod bench;
mod package;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use mv_proto::agent::Runtime;
use mv_proto::api::{
    DeployConfig, ErrorBody, FunctionInfo, HEADER_DURATION, HEADER_FUNCTION_DURATION,
    HEADER_REQUEST_ID, HEADER_RESTORE, HEADER_START, HEADER_VM, LogEntry, MAX_LOG_ENTRIES,
    StatusInfo, VmInfo,
};
use reqwest::blocking::{Client, RequestBuilder, Response, multipart};

#[derive(Parser)]
#[command(version, about = "Deploy and invoke functions on an mvd server")]
struct Cli {
    #[arg(long, env = "MVD_URL", default_value = "http://127.0.0.1:8080")]
    url: String,
    #[arg(long, env = "MVD_API_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Deploy a function from a directory, an archive or a single executable
    Deploy {
        name: String,
        path: PathBuf,
        #[arg(long, value_parser = parse_runtime)]
        runtime: Runtime,
        #[arg(long)]
        handler: Option<String>,
        #[arg(long, default_value_t = 128)]
        memory: u32,
        #[arg(long, default_value_t = 1)]
        vcpus: u8,
        #[arg(long, default_value = "3s", value_parser = humantime::parse_duration)]
        timeout: Duration,
        #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env)]
        env: Vec<(String, String)>,
        #[arg(long, default_value_t = 10)]
        max_concurrency: u32,
    },
    /// Invoke a function and print its response
    Invoke {
        name: String,
        #[arg(long, short, conflicts_with = "file", help = "Event JSON (default {})")]
        data: Option<String>,
        #[arg(long, short, help = "Read the event from a file, or - for stdin")]
        file: Option<PathBuf>,
        #[arg(long, short, help = "Print request metadata to stderr")]
        verbose: bool,
    },
    /// Show recent logs of a function
    Logs {
        name: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long, short, help = "Keep printing new log lines")]
        follow: bool,
    },
    /// List deployed functions
    List,
    /// Show one function in detail
    Describe { name: String },
    /// Delete a function and stop its VMs
    Delete { name: String },
    /// Show daemon status, host capacity and running VMs
    Status,
    /// Send concurrent requests and report latency and throughput
    Bench {
        name: String,
        #[arg(long, default_value_t = 1000)]
        requests: usize,
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        #[arg(long, default_value = "{}")]
        data: String,
        #[arg(long, help = "Print the summary as JSON")]
        json: bool,
    },
}

fn parse_runtime(text: &str) -> Result<Runtime, String> {
    Runtime::parse(text).ok_or_else(|| format!("unknown runtime {text:?} (native or python)"))
}

fn parse_env(text: &str) -> Result<(String, String), String> {
    let (key, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not KEY=VALUE"))?;
    Ok((key.to_string(), value.to_string()))
}

struct Api {
    client: Client,
    url: String,
    token: Option<String>,
}

impl Api {
    fn new(url: &str, token: Option<String>) -> anyhow::Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(600))
                .build()?,
            url: url.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    fn send(&self, request: RequestBuilder) -> anyhow::Result<Response> {
        let request = match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        request
            .send()
            .with_context(|| format!("cannot reach mvd at {}", self.url))
    }

    fn json<T: serde::de::DeserializeOwned>(&self, request: RequestBuilder) -> anyhow::Result<T> {
        let response = checked(self.send(request)?)?;
        Ok(response.json()?)
    }
}

fn checked(response: Response) -> anyhow::Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let text = response.text().unwrap_or_default();
    match serde_json::from_str::<ErrorBody>(&text) {
        Ok(error) => {
            let mut message = format!("{} ({}, HTTP {})", error.error, error.kind, status.as_u16());
            for line in error.logs {
                message.push_str("\n  | ");
                message.push_str(&line);
            }
            bail!(message)
        }
        Err(_) => bail!("HTTP {}: {}", status.as_u16(), text.trim()),
    }
}

fn table(header: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let line = |cells: Vec<String>| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(header.iter().map(|h| h.to_string()).collect());
    for row in rows {
        line(row);
    }
}

fn print_function(info: &FunctionInfo) {
    let config = &info.config;
    println!("name:            {}", info.name);
    println!("revision:        {}", info.revision);
    println!("runtime:         {}", config.runtime);
    println!("handler:         {}", config.handler());
    println!("memory:          {} MiB", config.memory_mib);
    println!("vcpus:           {}", config.vcpus);
    println!("timeout:         {} ms", config.timeout_ms);
    println!("max concurrency: {}", config.max_concurrency);
    println!(
        "code:            {} bytes, sha256 {}",
        info.code_size, info.code_sha256
    );
    println!("init:            {:.1} ms", info.init_duration_ms);
    println!("created:         {}", info.created_at);
    println!("updated:         {}", info.updated_at);
    println!(
        "vms:             {} idle, {} busy, {} starting",
        info.pool.idle, info.pool.busy, info.pool.starting
    );
    if !config.env.is_empty() {
        println!("environment:");
        for key in config.env.keys() {
            println!("  {key}");
        }
    }
}

fn read_event(data: Option<String>, file: Option<PathBuf>) -> anyhow::Result<String> {
    let event = match (data, file) {
        (Some(data), _) => data,
        (None, Some(path)) if path.as_os_str() == "-" => {
            let mut text = String::new();
            io::stdin().read_to_string(&mut text)?;
            text
        }
        (None, Some(path)) => {
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?
        }
        (None, None) => "{}".to_string(),
    };
    serde_json::from_str::<serde_json::Value>(&event).context("the event is not valid JSON")?;
    Ok(event)
}

fn header(response: &Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string()
}

fn print_logs(entries: &[LogEntry]) {
    for entry in entries {
        let request = entry.request_id.as_deref().unwrap_or("-");
        println!(
            "{} {:<8} {} {}",
            entry.timestamp, entry.stream, request, entry.line
        );
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let api = Api::new(&cli.url, cli.token.clone())?;
    match cli.command {
        Command::Deploy {
            name,
            path,
            runtime,
            handler,
            memory,
            vcpus,
            timeout,
            env,
            max_concurrency,
        } => {
            let config = DeployConfig {
                runtime,
                handler: handler.clone(),
                memory_mib: memory,
                vcpus,
                timeout_ms: timeout.as_millis() as u64,
                env: env.into_iter().collect::<BTreeMap<_, _>>(),
                max_concurrency,
            };
            config.validate().map_err(anyhow::Error::msg)?;
            let entry = match runtime {
                Runtime::Native => config.handler().to_string(),
                Runtime::Python => format!(
                    "{}.py",
                    config.handler().rsplit_once('.').map_or("main", |(m, _)| m)
                ),
            };
            let code = package::package(&path, &entry)?;
            eprintln!("deploying {name} ({} bytes of code)...", code.len());
            let form = multipart::Form::new()
                .text("config", serde_json::to_string(&config)?)
                .part(
                    "code",
                    multipart::Part::bytes(code).file_name("code.tar.gz"),
                );
            let info: FunctionInfo = api.json(
                api.client
                    .put(api.url(&format!("/v1/functions/{name}")))
                    .multipart(form),
            )?;
            println!(
                "deployed {} revision {} (init {:.1} ms)",
                info.name, info.revision, info.init_duration_ms
            );
        }
        Command::Invoke {
            name,
            data,
            file,
            verbose,
        } => {
            let event = read_event(data, file)?;
            let response = api.send(
                api.client
                    .post(api.url(&format!("/v1/functions/{name}/invoke")))
                    .body(event),
            )?;
            if verbose {
                eprintln!(
                    "request {}  start {}  duration {} ms  function {} ms  restore {} ms  vm {}",
                    header(&response, HEADER_REQUEST_ID),
                    header(&response, HEADER_START),
                    header(&response, HEADER_DURATION),
                    header(&response, HEADER_FUNCTION_DURATION),
                    header(&response, HEADER_RESTORE),
                    header(&response, HEADER_VM),
                );
            }
            let status = response.status();
            if status.as_u16() == 500
                && response
                    .headers()
                    .contains_key(mv_proto::api::HEADER_FUNCTION_ERROR)
            {
                let error: mv_proto::agent::FunctionError = response.json()?;
                for line in &error.trace {
                    eprint!("{line}");
                }
                bail!("the function raised {}: {}", error.kind, error.message);
            }
            let body = checked(response)?.text()?;
            match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(value) => println!("{}", serde_json::to_string_pretty(&value)?),
                Err(_) => println!("{body}"),
            }
        }
        Command::Logs {
            name,
            limit,
            request_id,
            follow,
        } => {
            let path = api.url(&format!("/v1/functions/{name}/logs"));
            let fetch = |limit: usize| -> anyhow::Result<Vec<LogEntry>> {
                let mut query = vec![("limit", limit.to_string())];
                if let Some(id) = &request_id {
                    query.push(("request_id", id.clone()));
                }
                api.json(api.client.get(&path).query(&query))
            };
            let mut seen = fetch(limit)?;
            print_logs(&seen);
            if follow {
                loop {
                    thread::sleep(Duration::from_secs(1));
                    let latest = fetch(MAX_LOG_ENTRIES)?;
                    let position = seen
                        .last()
                        .and_then(|last| latest.iter().rposition(|e| e == last));
                    let fresh = &latest[position.map_or(0, |p| p + 1)..];
                    print_logs(fresh);
                    if !fresh.is_empty() {
                        seen = latest;
                    }
                }
            }
        }
        Command::List => {
            let functions: Vec<FunctionInfo> =
                api.json(api.client.get(api.url("/v1/functions")))?;
            let rows = functions
                .iter()
                .map(|f| {
                    vec![
                        f.name.clone(),
                        f.config.runtime.to_string(),
                        f.revision.to_string(),
                        format!("{} MiB", f.config.memory_mib),
                        f.config.vcpus.to_string(),
                        format!("{} ms", f.config.timeout_ms),
                        f.config.max_concurrency.to_string(),
                        format!("{}/{}/{}", f.pool.idle, f.pool.busy, f.pool.starting),
                    ]
                })
                .collect();
            table(
                &[
                    "NAME",
                    "RUNTIME",
                    "REV",
                    "MEMORY",
                    "VCPUS",
                    "TIMEOUT",
                    "MAX",
                    "IDLE/BUSY/STARTING",
                ],
                rows,
            );
        }
        Command::Describe { name } => {
            let info: FunctionInfo =
                api.json(api.client.get(api.url(&format!("/v1/functions/{name}"))))?;
            print_function(&info);
        }
        Command::Delete { name } => {
            checked(api.send(api.client.delete(api.url(&format!("/v1/functions/{name}"))))?)?;
            println!("deleted {name}");
        }
        Command::Status => {
            let status: StatusInfo = api.json(api.client.get(api.url("/v1/status")))?;
            let capacity = &status.capacity;
            println!(
                "mvd {} up {} s, {} functions",
                status.version, status.uptime_s, status.functions
            );
            println!(
                "vms:    {} idle, {} busy, {} starting",
                status.vms.idle, status.vms.busy, status.vms.starting
            );
            println!(
                "memory: {} of {} MiB reserved, {} MiB free of {} MiB",
                capacity.memory_reserved_mib,
                capacity.memory_limit_mib,
                capacity.memory_available_mib,
                capacity.memory_total_mib
            );
            println!(
                "cpu:    {} of {} vCPUs busy ({} host CPUs)",
                capacity.vcpus_busy, capacity.vcpu_limit, capacity.cpus
            );
            let vms: Vec<VmInfo> = api.json(api.client.get(api.url("/v1/vms")))?;
            if !vms.is_empty() {
                println!();
                let rows = vms
                    .iter()
                    .map(|vm| {
                        vec![
                            vm.id.clone(),
                            vm.function.clone(),
                            vm.state.clone(),
                            vm.pid.map_or("-".into(), |p| p.to_string()),
                            vm.uid.to_string(),
                            vm.invocations.to_string(),
                            vm.memory
                                .as_ref()
                                .map_or("-".into(), |m| format!("{} KiB", m.pss_kib)),
                        ]
                    })
                    .collect();
                table(
                    &[
                        "VM",
                        "FUNCTION",
                        "STATE",
                        "PID",
                        "UID",
                        "INVOCATIONS",
                        "PSS",
                    ],
                    rows,
                );
            }
        }
        Command::Bench {
            name,
            requests,
            concurrency,
            data,
            json,
        } => {
            serde_json::from_str::<serde_json::Value>(&data).context("--data is not valid JSON")?;
            let load = bench::Load {
                client: &api.client,
                url: api.url(&format!("/v1/functions/{name}/invoke")),
                token: api.token.clone(),
                body: data,
                requests,
                concurrency,
            };
            let (samples, elapsed) = bench::run(&load);
            let summary = bench::summarize(&samples, concurrency, elapsed);
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                print_summary(&summary);
            }
        }
    }
    Ok(())
}

fn print_summary(summary: &bench::Summary) {
    println!(
        "{} requests, concurrency {}, {:.2} s, {:.1} req/s",
        summary.requests, summary.concurrency, summary.seconds, summary.throughput
    );
    println!(
        "ok: {} cold + {} warm, throttled: {}, errors: {}",
        summary.cold, summary.warm, summary.throttled, summary.errors
    );
    let line = |label: &str, p: &bench::Percentiles| {
        println!(
            "{label:<14} p50 {:>8.2}  p90 {:>8.2}  p99 {:>8.2}  max {:>8.2}  mean {:>8.2} ms",
            p.p50, p.p90, p.p99, p.max, p.mean
        );
    };
    line("latency", &summary.latency_ms);
    for (label, percentiles) in [
        ("cold latency", &summary.cold_latency_ms),
        ("warm latency", &summary.warm_latency_ms),
        ("restore", &summary.restore_ms),
        ("server time", &summary.server_ms),
    ] {
        if let Some(p) = percentiles {
            line(label, p);
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let cli = Cli::try_parse_from([
            "mvctl",
            "deploy",
            "hello",
            "./fn",
            "--runtime",
            "python",
            "--env",
            "A=b=c",
            "--timeout",
            "10s",
        ])
        .unwrap();
        match cli.command {
            Command::Deploy {
                runtime,
                env,
                timeout,
                ..
            } => {
                assert_eq!(runtime, Runtime::Python);
                assert_eq!(env, vec![("A".to_string(), "b=c".to_string())]);
                assert_eq!(timeout, Duration::from_secs(10));
            }
            _ => panic!("expected deploy"),
        }
        assert!(Cli::try_parse_from(["mvctl", "deploy", "x", "p", "--runtime", "ruby"]).is_err());
        assert!(
            Cli::try_parse_from([
                "mvctl",
                "deploy",
                "x",
                "p",
                "--runtime",
                "native",
                "--env",
                "NOEQ"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["mvctl", "invoke", "x", "--data", "{}", "--file", "f"]).is_err()
        );
        assert!(Cli::try_parse_from(["mvctl", "bench", "x", "--concurrency", "4"]).is_ok());
    }

    #[test]
    fn events_must_be_json() {
        assert_eq!(read_event(None, None).unwrap(), "{}");
        assert!(read_event(Some("{bad".into()), None).is_err());
        assert_eq!(read_event(Some("[1]".into()), None).unwrap(), "[1]");
    }
}
