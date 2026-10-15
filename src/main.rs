use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;
use log::{LevelFilter, error, info};

use minivmm::devices::virtio::net::parse_mac;
use minivmm::exit::ExitReason;
use minivmm::logger;
use minivmm::vmm::{BootConfig, DiskConfig, MachineConfig, NetConfig, RuntimeOptions, Vmm};

const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=1";
const DEFAULT_MAC: &str = "06:00:ac:10:00:02";

#[derive(Parser, Debug)]
#[command(version, about = "A small KVM-based virtual machine monitor")]
struct Cli {
    #[arg(long, value_name = "PATH", required_unless_present = "restore")]
    kernel: Option<PathBuf>,
    #[arg(long, value_name = "PATH", conflicts_with = "restore")]
    initrd: Option<PathBuf>,
    #[arg(long, default_value = DEFAULT_CMDLINE, conflicts_with = "restore")]
    cmdline: String,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=64))]
    cpus: u8,
    #[arg(long, default_value = "512M", value_parser = parse_size)]
    mem: u64,
    #[arg(long, value_name = "PATH", conflicts_with = "restore")]
    disk: Option<PathBuf>,
    #[arg(long, requires = "disk")]
    disk_read_only: bool,
    #[arg(long, value_name = "NAME", conflicts_with = "restore")]
    tap: Option<String>,
    #[arg(long, default_value = DEFAULT_MAC, value_parser = parse_mac_arg)]
    mac: [u8; 6],
    #[arg(long, value_name = "PATH")]
    api: Option<PathBuf>,
    #[arg(long, value_name = "SNAPSHOT", conflicts_with = "kernel")]
    restore: Option<PathBuf>,
    #[arg(long)]
    no_seccomp: bool,
    #[arg(long, default_value = "warn")]
    log_level: LevelFilter,
}

fn parse_mac_arg(text: &str) -> Result<[u8; 6]> {
    parse_mac(text).with_context(|| format!("invalid MAC address {text:?}"))
}

fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let (digits, shift) = match text.chars().last() {
        Some('K' | 'k') => (&text[..text.len() - 1], 10),
        Some('M' | 'm') => (&text[..text.len() - 1], 20),
        Some('G' | 'g') => (&text[..text.len() - 1], 30),
        _ => (text, 0),
    };
    let value: u64 = digits
        .parse()
        .with_context(|| format!("invalid size {text:?}"))?;
    let bytes = value
        .checked_mul(1 << shift)
        .with_context(|| format!("size {text:?} is too large"))?;
    if bytes == 0 {
        bail!("size must be positive");
    }
    Ok(bytes)
}

fn run(cli: Cli) -> Result<ExitReason> {
    let options = RuntimeOptions {
        api_socket: cli.api,
        seccomp: !cli.no_seccomp,
    };
    let vmm = match (cli.restore, cli.kernel) {
        (Some(snapshot), _) => Vmm::restore(&snapshot, &options)
            .with_context(|| format!("failed to restore {}", snapshot.display()))?,
        (None, Some(kernel)) => {
            let machine = MachineConfig {
                cpus: cli.cpus,
                mem_size: cli.mem,
                disk: cli.disk.map(|path| DiskConfig {
                    path,
                    read_only: cli.disk_read_only,
                }),
                net: cli.tap.map(|tap| NetConfig { tap, mac: cli.mac }),
            };
            let boot = BootConfig {
                kernel,
                initrd: cli.initrd,
                cmdline: cli.cmdline,
            };
            Vmm::boot(machine, &boot, &options).context("failed to start the virtual machine")?
        }
        (None, None) => bail!("either --kernel or --restore is required"),
    };
    Ok(vmm.run()?)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logger::init(cli.log_level);
    match run(cli) {
        Ok(ExitReason::Fault) => {
            error!("the virtual machine stopped because of a fault");
            ExitCode::FAILURE
        }
        Ok(reason) => {
            info!("virtual machine exited: {reason:?}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("2G").unwrap(), 2 << 30);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("64k").unwrap(), 64 << 10);
        assert!(parse_size("0").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("99999999999999G").is_err());
    }

    #[test]
    fn command_line_validation() {
        assert!(Cli::try_parse_from(["minivmm"]).is_err());
        assert!(Cli::try_parse_from(["minivmm", "--kernel", "k", "--restore", "s"]).is_err());
        assert!(Cli::try_parse_from(["minivmm", "--kernel", "k", "--disk-read-only"]).is_err());
        let cli = Cli::try_parse_from(["minivmm", "--restore", "s", "--api", "a"]).unwrap();
        assert_eq!(cli.restore, Some(PathBuf::from("s")));
        let cli = Cli::try_parse_from(["minivmm", "--kernel", "k", "--mac", "02:00:00:00:00:09"])
            .unwrap();
        assert_eq!(cli.mac, [2, 0, 0, 0, 0, 9]);
        assert!(Cli::try_parse_from(["minivmm", "--kernel", "k", "--cpus", "0"]).is_err());
    }
}
