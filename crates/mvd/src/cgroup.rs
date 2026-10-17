use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONTROLLERS: &str = "+cpu +memory +pids";
const CPU_PERIOD_US: u64 = 100_000;
const REMOVE_ATTEMPTS: u32 = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub memory_bytes: u64,
    pub vcpus: u8,
    pub pids: u32,
}

#[derive(Clone, Debug)]
pub struct CgroupRoot {
    path: PathBuf,
}

#[derive(Debug)]
pub struct Cgroup {
    path: PathBuf,
}

fn write(path: &Path, value: &str) -> io::Result<()> {
    fs::write(path, value).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

impl CgroupRoot {
    pub fn prepare(path: &Path) -> io::Result<Self> {
        fs::create_dir_all(path)?;
        write(&path.join("cgroup.subtree_control"), CONTROLLERS)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn create(&self, name: &str, limits: Limits) -> io::Result<Cgroup> {
        let path = self.path.join(name);
        fs::create_dir(&path)?;
        let cgroup = Cgroup { path };
        let configured = (|| {
            write(
                &cgroup.path.join("memory.max"),
                &limits.memory_bytes.to_string(),
            )?;
            let swap = cgroup.path.join("memory.swap.max");
            if swap.exists() {
                write(&swap, "0")?;
            }
            write(&cgroup.path.join("memory.oom.group"), "1")?;
            let quota = u64::from(limits.vcpus) * CPU_PERIOD_US;
            write(
                &cgroup.path.join("cpu.max"),
                &format!("{quota} {CPU_PERIOD_US}"),
            )?;
            write(&cgroup.path.join("pids.max"), &limits.pids.to_string())
        })();
        if let Err(err) = configured {
            let _ = fs::remove_dir(&cgroup.path);
            return Err(err);
        }
        Ok(cgroup)
    }

    /// Kills and removes cgroups left behind by an earlier daemon that did not shut down
    /// cleanly.
    pub fn remove_leftovers(&self) -> usize {
        let Ok(entries) = fs::read_dir(&self.path) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let cgroup = Cgroup { path: entry.path() };
                cgroup.kill();
                if cgroup.remove().is_ok() {
                    removed += 1;
                }
            }
        }
        removed
    }
}

impl Cgroup {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn procs_file(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    pub fn kill(&self) {
        let _ = fs::write(self.path.join("cgroup.kill"), "1");
    }

    pub fn remove(&self) -> io::Result<()> {
        let mut attempt = 0;
        loop {
            match fs::remove_dir(&self.path) {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(err) if attempt >= REMOVE_ATTEMPTS => return Err(err),
                Err(_) => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_limits_into_a_fake_hierarchy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mvd");
        let cgroups = CgroupRoot::prepare(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("cgroup.subtree_control")).unwrap(),
            CONTROLLERS
        );
        let limits = Limits {
            memory_bytes: 256 << 20,
            vcpus: 2,
            pids: 64,
        };
        let cgroup = cgroups.create("vm-1", limits).unwrap();
        let read = |name: &str| fs::read_to_string(cgroup.path().join(name)).unwrap();
        assert_eq!(read("memory.max"), (256u64 << 20).to_string());
        assert_eq!(read("cpu.max"), "200000 100000");
        assert_eq!(read("pids.max"), "64");
        assert_eq!(read("memory.oom.group"), "1");
        assert!(cgroups.create("vm-1", limits).is_err());
        assert_eq!(cgroup.procs_file(), root.join("vm-1/cgroup.procs"));
    }
}
