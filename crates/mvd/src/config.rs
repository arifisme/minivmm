use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Clone, Debug, Parser)]
#[command(version, about = "Serverless function platform built on minivmm")]
pub struct Options {
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,
    #[arg(long, default_value = "/var/lib/mvd")]
    pub data_dir: PathBuf,
    #[arg(long, value_name = "PATH")]
    pub kernel: PathBuf,
    #[arg(
        long,
        value_name = "DIR",
        help = "Directory holding native.ext4 and python.ext4"
    )]
    pub runtime_dir: PathBuf,
    #[arg(long, value_name = "PATH", help = "Statically linked minivmm binary")]
    pub vmm: PathBuf,
    #[arg(
        long,
        value_name = "PATH",
        help = "mv-jailer binary (default: next to mvd)"
    )]
    pub jailer: Option<PathBuf>,
    #[arg(
        long,
        help = "Run VMMs without the jail and cgroups (development only)"
    )]
    pub no_jail: bool,
    #[arg(long, default_value = "/sys/fs/cgroup/mvd")]
    pub cgroup_root: PathBuf,
    #[arg(
        long,
        default_value_t = 900_000,
        help = "First uid/gid given to VMM processes"
    )]
    pub uid_base: u32,
    #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
    pub idle_timeout: Duration,
    #[arg(long, default_value = "2s", value_parser = humantime::parse_duration)]
    pub queue_timeout: Duration,
    #[arg(long, default_value = "30s", value_parser = humantime::parse_duration)]
    pub init_timeout: Duration,
    #[arg(
        long,
        help = "Memory available to VMs in MiB (default: host memory minus the reserve)"
    )]
    pub memory_limit_mib: Option<u64>,
    #[arg(long, default_value_t = 512)]
    pub memory_reserve_mib: u64,
    #[arg(
        long,
        default_value_t = 32,
        help = "Memory charged per VM beyond guest memory"
    )]
    pub vm_overhead_mib: u64,
    #[arg(
        long,
        default_value_t = 1.0,
        help = "How far VM memory limits may exceed the memory limit"
    )]
    pub memory_overcommit: f64,
    #[arg(
        long,
        help = "vCPUs that may be running invocations at once (default: 2 per host CPU)"
    )]
    pub vcpu_limit: Option<u32>,
    #[arg(long, default_value_t = 64 << 20)]
    pub max_code_bytes: usize,
    #[arg(
        long,
        env = "MVD_API_TOKEN",
        help = "Require this bearer token on every request"
    )]
    pub api_token: Option<String>,
    #[arg(long, value_enum, default_value = "text")]
    pub log_format: LogFormat,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_and_defaults() {
        let options = Options::try_parse_from([
            "mvd",
            "--kernel",
            "k",
            "--runtime-dir",
            "r",
            "--vmm",
            "v",
            "--idle-timeout",
            "1500ms",
        ])
        .unwrap();
        assert_eq!(options.idle_timeout, Duration::from_millis(1500));
        assert_eq!(options.queue_timeout, Duration::from_secs(2));
        assert_eq!(options.listen.port(), 8080);
        assert!(!options.no_jail);
        assert!(Options::try_parse_from(["mvd"]).is_err());
    }
}
