use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::EntryType;

pub const MAX_FILES: u64 = 20_000;
pub const MAX_BYTES: u64 = 512 << 20;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

#[derive(Debug, PartialEq, Eq)]
pub struct Extracted {
    pub files: u64,
    pub bytes: u64,
}

fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(clean)
}

/// Unpacks a tar or tar.gz archive into `dest`, accepting only regular files and directories
/// with relative paths that stay inside `dest`.
pub fn extract(archive: &[u8], dest: &Path) -> Result<Extracted, String> {
    let reader: Box<dyn Read> = if archive.starts_with(&GZIP_MAGIC) {
        Box::new(GzDecoder::new(archive))
    } else {
        Box::new(archive)
    };
    let mut tar = tar::Archive::new(reader);
    let mut extracted = Extracted { files: 0, bytes: 0 };
    let entries = tar
        .entries()
        .map_err(|e| format!("cannot read archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("corrupt archive: {e}"))?;
        let raw_path = entry
            .path()
            .map_err(|e| format!("bad path in archive: {e}"))?
            .into_owned();
        let relative = safe_relative(&raw_path).ok_or_else(|| {
            format!(
                "archive path {} leaves the function directory",
                raw_path.display()
            )
        })?;
        let target = dest.join(&relative);
        match entry.header().entry_type() {
            EntryType::Directory => {
                fs::create_dir_all(&target)
                    .map_err(|e| format!("cannot create {}: {e}", relative.display()))?;
            }
            EntryType::Regular | EntryType::Continuous => {
                extracted.files += 1;
                if extracted.files > MAX_FILES {
                    return Err(format!("archive holds more than {MAX_FILES} files"));
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                }
                let executable = entry.header().mode().unwrap_or(0o644) & 0o111 != 0;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(if executable { 0o755 } else { 0o644 })
                    .open(&target)
                    .map_err(|e| format!("cannot create {}: {e}", relative.display()))?;
                let budget = MAX_BYTES - extracted.bytes;
                let written = io::copy(&mut (&mut entry).take(budget + 1), &mut file)
                    .map_err(|e| format!("cannot write {}: {e}", relative.display()))?;
                if written > budget {
                    return Err(format!(
                        "archive expands to more than {} MiB",
                        MAX_BYTES >> 20
                    ));
                }
                extracted.bytes += written;
            }
            EntryType::XHeader | EntryType::XGlobalHeader | EntryType::GNULongName => {}
            other => {
                return Err(format!(
                    "archive entry {} has unsupported type {other:?}",
                    relative.display()
                ));
            }
        }
    }
    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    fn header(path: &str, kind: EntryType, size: u64, mode: u32) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_entry_type(kind);
        header.set_size(size);
        header.set_mode(mode);
        header.set_cksum();
        header
    }

    fn archive(entries: &[(&str, EntryType, &[u8], u32)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, kind, data, mode) in entries {
            let header = header(path, *kind, data.len() as u64, *mode);
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        io::Write::write_all(&mut encoder, data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn extracts_files_directories_and_modes() {
        let dir = tempfile::tempdir().unwrap();
        let tar = archive(&[
            ("./", EntryType::Directory, b"", 0o755),
            ("pkg/", EntryType::Directory, b"", 0o755),
            ("pkg/main.py", EntryType::Regular, b"print(1)\n", 0o644),
            ("bootstrap", EntryType::Regular, b"#!/bin/sh\n", 0o4755),
        ]);
        let stats = extract(&gzip(&tar), dir.path()).unwrap();
        assert_eq!(
            stats,
            Extracted {
                files: 2,
                bytes: 19
            }
        );
        assert_eq!(
            fs::read(dir.path().join("pkg/main.py")).unwrap(),
            b"print(1)\n"
        );
        let mode = fs::metadata(dir.path().join("bootstrap"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755);
        let plain = tempfile::tempdir().unwrap();
        assert!(extract(&tar, plain.path()).is_ok());
    }

    #[test]
    fn rejects_escaping_paths_and_links() {
        for path in ["../evil", "/etc/passwd", "a/../../b"] {
            let dir = tempfile::tempdir().unwrap();
            let tar = archive(&[(path, EntryType::Regular, b"x", 0o644)]);
            assert!(extract(&tar, dir.path()).is_err(), "{path}");
        }
        let dir = tempfile::tempdir().unwrap();
        let tar = archive(&[("link", EntryType::Symlink, b"", 0o777)]);
        assert!(
            extract(&tar, dir.path())
                .unwrap_err()
                .contains("unsupported")
        );
        let duplicate = archive(&[
            ("a", EntryType::Regular, b"1", 0o644),
            ("a", EntryType::Regular, b"2", 0o644),
        ]);
        assert!(extract(&duplicate, dir.path()).is_err());
    }

    #[test]
    fn rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        assert!(extract(&[0x1f, 0x8b, 1, 2, 3], dir.path()).is_err());
        assert!(extract(&[7u8; 1000], dir.path()).is_err());
    }
}
