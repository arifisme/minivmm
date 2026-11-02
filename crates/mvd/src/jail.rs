use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::cgroup::Cgroup;

#[derive(Clone, Debug)]
pub struct JailConfig {
    pub enabled: bool,
    pub jailer: PathBuf,
    pub vmm: PathBuf,
    pub base_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    pub source: PathBuf,
    pub target: &'static str,
}

impl Bind {
    pub fn new(source: impl Into<PathBuf>, target: &'static str) -> Self {
        Self {
            source: source.into(),
            target,
        }
    }
}

/// A per-VM directory that becomes the VMM's root. The VMM's sockets and snapshots appear in
/// it on the host, while read-only inputs are bound in only inside the VMM's namespace.
#[derive(Debug)]
pub struct JailDir {
    dir: PathBuf,
    enabled: bool,
}

impl JailDir {
    pub fn create(config: &JailConfig, id: &str, uid: u32, gid: u32) -> io::Result<Self> {
        let dir = config.base_dir.join(id);
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&dir)?;
        if config.enabled {
            std::os::unix::fs::chown(&dir, Some(uid), Some(gid))?;
        }
        Ok(Self {
            dir,
            enabled: config.enabled,
        })
    }

    pub fn host_dir(&self) -> &Path {
        &self.dir
    }

    /// The path the VMM uses for a bound input: its target inside the jail, or the original
    /// file when VMMs run unjailed.
    pub fn vmm_path(&self, bind: &Bind) -> PathBuf {
        if self.enabled {
            PathBuf::from(bind.target)
        } else {
            bind.source.clone()
        }
    }

    pub fn vmm_file(&self, name: &str) -> PathBuf {
        if self.enabled {
            Path::new("/").join(name)
        } else {
            self.dir.join(name)
        }
    }

    pub fn host_file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The VMM runs with uid and gid `uid`, and cannot write files larger than
    /// `max_file_bytes`, which is enough for a snapshot of its guest memory.
    pub fn command(
        &self,
        config: &JailConfig,
        uid: u32,
        cgroup: Option<&Cgroup>,
        binds: &[Bind],
        vmm_args: &[String],
        max_file_bytes: u64,
    ) -> Command {
        if !self.enabled {
            let mut command = Command::new(&config.vmm);
            command.args(vmm_args).current_dir(&self.dir);
            return command;
        }
        let mut command = Command::new(&config.jailer);
        command
            .arg("--root")
            .arg(&self.dir)
            .arg("--uid")
            .arg(uid.to_string())
            .arg("--gid")
            .arg(uid.to_string())
            .arg("--max-file-bytes")
            .arg(max_file_bytes.to_string())
            .arg("--exec")
            .arg(&config.vmm);
        if let Some(cgroup) = cgroup {
            command.arg("--cgroup-procs").arg(cgroup.procs_file());
        }
        for bind in binds {
            command
                .arg("--bind")
                .arg(format!("{}:{}", bind.source.display(), bind.target));
        }
        command.arg("--").args(vmm_args).env_clear();
        command
    }

    pub fn remove(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn open_no_follow(path: &Path, flags: libc::c_int) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | flags)
        .open(path)
}

fn invalid(path: &Path, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} is not {what}", path.display()),
    )
}

/// A socket in a jail directory, which the VMM can write to. It is opened without following
/// symlinks and connected to through `/proc/self/fd`, so a compromised VMM cannot point the
/// daemon at another VM's socket.
pub struct JailSocket(fs::File);

impl JailSocket {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = open_no_follow(path, libc::O_PATH)?;
        if !file.metadata()?.file_type().is_socket() {
            return Err(invalid(path, "a socket"));
        }
        Ok(Self(file))
    }

    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }
}

/// Takes over a file a VMM wrote after it was moved out of the jail: it must be a plain file
/// with a single link (not a symlink, FIFO or hard link planted by a compromised VMM), and it
/// becomes owned by root with the given group and mode.
pub fn seal_output(path: &Path, gid: u32, mode: u32) -> io::Result<()> {
    let file = open_no_follow(path, libc::O_NONBLOCK)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(invalid(path, "a regular file"));
    }
    if is_root() {
        std::os::unix::fs::fchown(&file, Some(0), Some(gid))?;
    }
    file.set_permissions(fs::Permissions::from_mode(mode))
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base: &Path, enabled: bool) -> JailConfig {
        JailConfig {
            enabled,
            jailer: PathBuf::from("/usr/bin/mv-jailer"),
            vmm: PathBuf::from("/usr/bin/minivmm"),
            base_dir: base.to_path_buf(),
        }
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn jailed_commands_bind_inputs_and_use_jail_paths() {
        let base = tempfile::tempdir().unwrap();
        let config = config(base.path(), true);
        let jail = JailDir {
            dir: base.path().join("vm-1"),
            enabled: true,
        };
        let rootfs = Bind::new("/images/python.ext4", "/rootfs.ext4");
        assert_eq!(jail.vmm_path(&rootfs), PathBuf::from("/rootfs.ext4"));
        assert_eq!(jail.vmm_file("v.sock"), PathBuf::from("/v.sock"));
        assert_eq!(jail.host_file("v.sock"), base.path().join("vm-1/v.sock"));
        let command = jail.command(
            &config,
            7,
            None,
            &[rootfs],
            &["--restore".into(), "/s".into()],
            1 << 30,
        );
        assert_eq!(command.as_std().get_program(), "/usr/bin/mv-jailer");
        let args = args(&command);
        let bind = args.iter().position(|a| a == "--bind").unwrap();
        assert_eq!(args[bind + 1], "/images/python.ext4:/rootfs.ext4");
        assert_eq!(&args[args.len() - 3..], &["--", "--restore", "/s"]);
        assert!(args.windows(2).any(|w| w == ["--uid", "7"]));
        assert!(args.windows(2).any(|w| w == ["--gid", "7"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--max-file-bytes", "1073741824"])
        );
    }

    #[test]
    fn unjailed_commands_run_the_vmm_directly() {
        let base = tempfile::tempdir().unwrap();
        let config = config(base.path(), false);
        let jail = JailDir::create(&config, "vm-2", 1, 1).unwrap();
        assert!(jail.host_dir().is_dir());
        let rootfs = Bind::new("/images/native.ext4", "/rootfs.ext4");
        assert_eq!(jail.vmm_path(&rootfs), PathBuf::from("/images/native.ext4"));
        assert_eq!(jail.vmm_file("api.sock"), base.path().join("vm-2/api.sock"));
        let command = jail.command(&config, 1, None, &[rootfs], &["--x".into()], 1 << 30);
        assert_eq!(command.as_std().get_program(), "/usr/bin/minivmm");
        assert_eq!(args(&command), vec!["--x"]);
        jail.remove();
        assert!(!jail.host_dir().exists());
    }

    #[test]
    fn sockets_and_outputs_must_not_be_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("v.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let link = dir.path().join("link.sock");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        let opened = JailSocket::open(&socket).unwrap();
        std::os::unix::net::UnixStream::connect(opened.path()).unwrap();
        assert!(JailSocket::open(&link).is_err());
        assert!(JailSocket::open(dir.path()).is_err());

        let target = dir.path().join("secret");
        fs::write(&target, b"x").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let planted = dir.path().join("out.state");
        std::os::unix::fs::symlink(&target, &planted).unwrap();
        assert!(seal_output(&planted, 0, 0o444).is_err());
        let linked = dir.path().join("linked");
        fs::hard_link(&target, &linked).unwrap();
        assert!(seal_output(&linked, 0, 0o444).is_err());
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let output = dir.path().join("out.mem");
        fs::write(&output, b"memory").unwrap();
        let gid = fs::metadata(&output).unwrap().gid();
        seal_output(&output, gid, 0o440).unwrap();
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o440
        );
    }
}
