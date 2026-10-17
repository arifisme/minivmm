use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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

    pub fn command(
        &self,
        config: &JailConfig,
        uid: u32,
        gid: u32,
        cgroup: Option<&Cgroup>,
        binds: &[Bind],
        vmm_args: &[String],
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
            .arg(gid.to_string())
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

pub fn make_world_readable(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))
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
            8,
            None,
            &[rootfs],
            &["--restore".into(), "/s".into()],
        );
        assert_eq!(command.as_std().get_program(), "/usr/bin/mv-jailer");
        let args = args(&command);
        let bind = args.iter().position(|a| a == "--bind").unwrap();
        assert_eq!(args[bind + 1], "/images/python.ext4:/rootfs.ext4");
        assert_eq!(&args[args.len() - 3..], &["--", "--restore", "/s"]);
        assert!(args.windows(2).any(|w| w == ["--uid", "7"]));
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
        let command = jail.command(&config, 1, 1, None, &[rootfs], &["--x".into()]);
        assert_eq!(command.as_std().get_program(), "/usr/bin/minivmm");
        assert_eq!(args(&command), vec!["--x"]);
        jail.remove();
        assert!(!jail.host_dir().exists());
    }
}
