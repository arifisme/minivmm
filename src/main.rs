use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;
use log::{LevelFilter, error, info};

use minivmm::exit::ExitReason;
use minivmm::logger;
use minivmm::vmm::{VmConfig, Vmm};

const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=1";

#[derive(Parser, Debug)]
#[command(version, about = "A small KVM-based virtual machine monitor")]
struct Cli {
    #[arg(long, value_name = "PATH", required = true)]
    kernel: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    initrd: Option<PathBuf>,
    #[arg(long, default_value = DEFAULT_CMDLINE)]
    cmdline: String,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=64))]
    cpus: u8,
    #[arg(long, default_value = "512M", value_parser = parse_size)]
    mem: u64,
    #[arg(long, default_value = "warn")]
    log_level: LevelFilter,
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
    let kernel = cli.kernel.context("--kernel is required")?;
    let config = VmConfig {
        kernel,
        initrd: cli.initrd,
        cmdline: cli.cmdline,
        cpus: cli.cpus,
        mem_size: cli.mem,
    };
    let vmm = Vmm::boot(&config).context("failed to start the virtual machine")?;
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
}
