mod archive;
mod logs;
mod runtime;
mod server;
mod system;
mod transport;

use std::path::PathBuf;
use std::process::ExitCode;

use mv_proto::agent::AGENT_PORT;

use server::Agent;
use system::Reaper;
use transport::Listener;

const DEFAULT_WORK_DIR: &str = "/run/mv";
const USAGE: &str = "usage: mv-agent [--listen vsock:PORT|unix:PATH] [--work-dir DIR]";

struct Options {
    listen: String,
    work_dir: PathBuf,
}

fn parse_options(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        listen: format!("vsock:{AGENT_PORT}"),
        work_dir: PathBuf::from(DEFAULT_WORK_DIR),
    };
    let mut args = args;
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--listen" => options.listen = value()?,
            "--work-dir" => options.work_dir = PathBuf::from(value()?),
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    Ok(options)
}

fn listen(spec: &str) -> Result<Listener, String> {
    let bound = match spec.split_once(':') {
        Some(("vsock", port)) => {
            let port = port
                .parse()
                .map_err(|_| format!("invalid vsock port {port}"))?;
            Listener::vsock(port)
        }
        Some(("unix", path)) => Listener::unix(path.as_ref()),
        _ => return Err(format!("cannot listen on {spec}")),
    };
    bound.map_err(|e| format!("cannot listen on {spec}: {e}"))
}

fn run() -> Result<(), String> {
    let options = parse_options(std::env::args().skip(1)).map_err(|e| format!("{e}\n{USAGE}"))?;
    let guest = std::process::id() == 1;
    if guest {
        system::init_guest().map_err(|e| format!("cannot prepare the guest: {e}"))?;
    }
    std::fs::create_dir_all(&options.work_dir)
        .map_err(|e| format!("cannot create {}: {e}", options.work_dir.display()))?;
    let listener = listen(&options.listen)?;
    let mut agent = Agent::new(guest, options.work_dir, Reaper::new(guest));
    agent
        .serve(&listener)
        .map_err(|e| format!("agent stopped: {e}"))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mv-agent: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, String> {
        parse_options(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_options() {
        let defaults = parse(&[]).unwrap();
        assert_eq!(defaults.listen, "vsock:52");
        assert_eq!(defaults.work_dir, PathBuf::from("/run/mv"));
        let custom = parse(&["--listen", "unix:/tmp/a", "--work-dir", "/w"]).unwrap();
        assert_eq!(custom.listen, "unix:/tmp/a");
        assert_eq!(custom.work_dir, PathBuf::from("/w"));
        assert!(parse(&["--listen"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(listen("tcp:1").is_err());
        assert!(listen("vsock:x").is_err());
    }
}
