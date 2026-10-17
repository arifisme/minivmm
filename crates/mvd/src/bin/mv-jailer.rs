use std::convert::Infallible;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

const KVM_MAJOR: u32 = 10;
const KVM_MINOR: u32 = 232;
const VMM_TARGET: &str = "minivmm";
const OLD_ROOT: &str = "old-root";
const MAX_OPEN_FILES: libc::rlim_t = 4096;

/// Starts a VMM inside a minimal jail: a private mount namespace whose root holds only the
/// files bound into it, empty network, IPC and UTS namespaces, its own cgroup and an
/// unprivileged uid.
#[derive(Parser, Debug)]
#[command(version)]
struct Options {
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    uid: u32,
    #[arg(long)]
    gid: u32,
    #[arg(long, value_name = "FILE")]
    cgroup_procs: Option<PathBuf>,
    #[arg(long, value_name = "SOURCE:TARGET", value_parser = parse_bind)]
    bind: Vec<(PathBuf, String)>,
    #[arg(long, value_name = "PATH")]
    exec: PathBuf,
    #[arg(last = true)]
    args: Vec<OsString>,
}

fn parse_bind(text: &str) -> Result<(PathBuf, String), String> {
    let (source, target) = text
        .rsplit_once(':')
        .ok_or_else(|| format!("{text} is not SOURCE:TARGET"))?;
    let valid = target.starts_with('/')
        && target.len() > 1
        && target[1..]
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if !valid {
        return Err(format!("{target} is not an absolute path inside the jail"));
    }
    Ok((PathBuf::from(source), target.to_string()))
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)
}

fn check(result: libc::c_int, what: &str) -> io::Result<()> {
    if result < 0 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(err.kind(), format!("{what}: {err}")));
    }
    Ok(())
}

fn mount(source: Option<&Path>, target: &Path, flags: libc::c_ulong) -> io::Result<()> {
    let source = source.map(c_path).transpose()?;
    let target_c = c_path(target)?;
    // SAFETY: the strings outlive the call and null pointers are allowed for the source,
    // filesystem type and data of bind and propagation mounts.
    let result = unsafe {
        libc::mount(
            source.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            target_c.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    check(result, &format!("mount {}", target.display()))
}

fn bind_read_only(source: &Path, target: &Path, executable: bool) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if source.is_dir() {
        fs::create_dir_all(target)?;
    } else {
        fs::File::create(target)?;
    }
    mount(Some(source), target, libc::MS_BIND)?;
    let mut flags =
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV;
    if !executable {
        flags |= libc::MS_NOEXEC;
    }
    mount(None, target, flags)
}

fn create_kvm_node(root: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let dev = root.join("dev");
    fs::DirBuilder::new()
        .mode(0o755)
        .recursive(true)
        .create(&dev)?;
    let node = c_path(&dev.join("kvm"))?;
    // SAFETY: node is a valid path; makedev builds the device number.
    check(
        unsafe {
            libc::mknod(
                node.as_ptr(),
                libc::S_IFCHR | 0o600,
                libc::makedev(KVM_MAJOR, KVM_MINOR),
            )
        },
        "mknod /dev/kvm",
    )?;
    // SAFETY: as above.
    check(
        unsafe { libc::chown(node.as_ptr(), uid, gid) },
        "chown /dev/kvm",
    )
}

fn enter_root(root: &Path) -> io::Result<()> {
    std::env::set_current_dir(root)?;
    fs::create_dir_all(OLD_ROOT)?;
    let new_root = CString::new(".").map_err(io::Error::other)?;
    let old_root = CString::new(OLD_ROOT).map_err(io::Error::other)?;
    // SAFETY: pivot_root with two valid paths relative to the current directory.
    check(
        unsafe {
            libc::syscall(libc::SYS_pivot_root, new_root.as_ptr(), old_root.as_ptr()) as libc::c_int
        },
        "pivot_root",
    )?;
    std::env::set_current_dir("/")?;
    let old = CString::new(format!("/{OLD_ROOT}")).map_err(io::Error::other)?;
    // SAFETY: detaching the old root, which is mounted at a valid path.
    check(
        unsafe { libc::umount2(old.as_ptr(), libc::MNT_DETACH) },
        "umount old root",
    )?;
    fs::remove_dir(format!("/{OLD_ROOT}"))
}

fn drop_privileges(uid: u32, gid: u32) -> io::Result<()> {
    let open_files = libc::rlimit {
        rlim_cur: MAX_OPEN_FILES,
        rlim_max: MAX_OPEN_FILES,
    };
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain system calls with valid arguments.
    unsafe {
        check(
            libc::setrlimit(libc::RLIMIT_NOFILE, &open_files),
            "setrlimit",
        )?;
        check(libc::setrlimit(libc::RLIMIT_CORE, &no_core), "setrlimit")?;
        check(libc::setgroups(0, std::ptr::null()), "setgroups")?;
        check(libc::setresgid(gid, gid, gid), "setresgid")?;
        check(libc::setresuid(uid, uid, uid), "setresuid")?;
        check(
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
            "no_new_privs",
        )
    }
}

fn exec(args: &[OsString]) -> io::Result<Infallible> {
    let program = CString::new(format!("/{VMM_TARGET}")).map_err(io::Error::other)?;
    let mut argv = vec![program.clone()];
    for arg in args {
        argv.push(CString::new(OsStr::as_bytes(arg)).map_err(io::Error::other)?);
    }
    let mut pointers: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    pointers.push(std::ptr::null());
    let environment: [*const libc::c_char; 1] = [std::ptr::null()];
    // SAFETY: argv and the environment are NULL-terminated arrays of valid C strings.
    unsafe { libc::execve(program.as_ptr(), pointers.as_ptr(), environment.as_ptr()) };
    Err(io::Error::last_os_error())
}

fn run(options: Options) -> io::Result<Infallible> {
    if let Some(procs) = &options.cgroup_procs {
        fs::write(procs, "0").map_err(|e| io::Error::new(e.kind(), format!("join cgroup: {e}")))?;
    }
    // SAFETY: unshare only affects this single-threaded process.
    check(
        unsafe {
            libc::unshare(
                libc::CLONE_NEWNS | libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS,
            )
        },
        "unshare",
    )?;
    mount(None, Path::new("/"), libc::MS_REC | libc::MS_PRIVATE)?;
    let root = &options.root;
    mount(Some(root), root, libc::MS_BIND | libc::MS_REC)?;
    bind_read_only(&options.exec, &root.join(VMM_TARGET), true)?;
    for (source, target) in &options.bind {
        bind_read_only(source, &root.join(target.trim_start_matches('/')), false)?;
    }
    create_kvm_node(root, options.uid, options.gid)?;
    enter_root(root)?;
    drop_privileges(options.uid, options.gid)?;
    exec(&options.args)
}

fn main() -> ExitCode {
    let options = Options::parse();
    match run(options) {
        Ok(never) => match never {},
        Err(err) => {
            eprintln!("mv-jailer: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jail_options() {
        let options = Options::try_parse_from([
            "mv-jailer",
            "--root",
            "/jail",
            "--uid",
            "900001",
            "--gid",
            "900001",
            "--bind",
            "/data/fn/snapshot.state:/snapshot.state",
            "--exec",
            "/usr/bin/minivmm",
            "--",
            "--restore",
            "/snapshot.state",
        ])
        .unwrap();
        assert_eq!(options.bind[0].1, "/snapshot.state");
        assert_eq!(options.args, vec!["--restore", "/snapshot.state"]);
        for bad in ["/a", "/a:relative", "/a:/", "/a:/../x", "/a:/x//y"] {
            assert!(parse_bind(bad).is_err(), "{bad}");
        }
    }
}
