use std::fs;
use std::io;
use std::path::Path;

use flate2::Compression;
use flate2::write::GzEncoder;

const ARCHIVE_SUFFIXES: [&str; 3] = [".tar.gz", ".tgz", ".tar"];

/// Turns a directory into a tar.gz, passes archives through unchanged, and wraps a single file
/// into an archive under `entry_name` (the handler for native functions).
pub fn package(path: &Path, entry_name: &str) -> anyhow::Result<Vec<u8>> {
    let metadata =
        fs::metadata(path).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    if metadata.is_dir() {
        return directory_archive(path);
    }
    let name = path.to_string_lossy();
    if ARCHIVE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return Ok(fs::read(path)?);
    }
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    builder.append_path_with_name(path, entry_name)?;
    Ok(builder.into_inner()?.finish()?)
}

fn directory_archive(dir: &Path) -> anyhow::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    builder.follow_symlinks(true);
    append_dir(&mut builder, dir, Path::new(""))?;
    Ok(builder.into_inner()?.finish()?)
}

fn append_dir<W: io::Write>(
    builder: &mut tar::Builder<W>,
    dir: &Path,
    prefix: &Path,
) -> anyhow::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = prefix.join(entry.file_name());
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() == "__pycache__" {
                continue;
            }
            builder.append_dir(&name, &path)?;
            append_dir(builder, &path, &name)?;
        } else {
            builder.append_path_with_name(&path, &name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    use flate2::read::GzDecoder;

    use super::*;

    fn entries(archive: &[u8]) -> Vec<(String, u32)> {
        let mut tar = tar::Archive::new(GzDecoder::new(archive));
        tar.entries()
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.path().unwrap().display().to_string(),
                    entry.header().mode().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn packages_directories_files_and_archives() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("pkg/__pycache__")).unwrap();
        fs::write(src.join("main.py"), "def handler(e, c): pass\n").unwrap();
        fs::write(src.join("pkg/util.py"), "").unwrap();
        fs::write(src.join("pkg/__pycache__/util.pyc"), "").unwrap();
        let names: Vec<String> = entries(&package(&src, "unused").unwrap())
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["main.py", "pkg", "pkg/util.py"]);

        let binary = dir.path().join("my-function");
        fs::write(&binary, b"\x7fELF").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let wrapped = entries(&package(&binary, "bootstrap").unwrap());
        assert_eq!(wrapped[0].0, "bootstrap");
        assert_eq!(wrapped[0].1 & 0o111, 0o111);

        let archive = dir.path().join("code.tar.gz");
        fs::write(&archive, b"opaque").unwrap();
        let mut passed = Vec::new();
        package(&archive, "x")
            .unwrap()
            .as_slice()
            .read_to_end(&mut passed)
            .unwrap();
        assert_eq!(passed, b"opaque");
        assert!(package(&dir.path().join("missing"), "x").is_err());
    }
}
