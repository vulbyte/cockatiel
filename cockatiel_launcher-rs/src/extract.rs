//! Archive extraction (zip / tar / tar.gz / tar.zst) with a path-traversal
//! guard.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;
use ruzstd::StreamingDecoder;
use zip::ZipArchive;

use crate::paths::is_within;

/// Extract `archive` into `dest`.
pub trait Extractor {
    fn extract(&self, archive: &Path, dest: &Path) -> Result<(), String>;
}

/// The real extractor, dispatching on the archive extension.
pub struct ArchiveExtractor;

impl Extractor for ArchiveExtractor {
    fn extract(&self, archive: &Path, dest: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dest)
            .map_err(|e| format!("create {}: {}", dest.display(), e))?;

        let name = archive
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();

        if name.ends_with(".tar.zst") || name.ends_with(".tzst") {
            let file = open(archive)?;
            let decoder = StreamingDecoder::new(file)
                .map_err(|e| format!("zstd decode {}: {}", archive.display(), e))?;
            extract_tar(decoder, dest)
        } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            let file = open(archive)?;
            extract_tar(GzDecoder::new(file), dest)
        } else if name.ends_with(".tar") {
            extract_tar(open(archive)?, dest)
        } else if name.ends_with(".zip") {
            extract_zip(archive, dest)
        } else {
            Err(format!(
                "unsupported archive extension for {} (expected .zip, .tar, .tar.gz/.tgz or .tar.zst/.tzst)",
                archive.display()
            ))
        }
    }
}

fn open(path: &Path) -> Result<File, String> {
    File::open(path).map_err(|e| format!("open {}: {}", path.display(), e))
}

fn extract_tar<R: Read>(reader: R, dest: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| format!("read tar entries: {}", e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("read tar entry: {}", e))?;
        let path = entry
            .path()
            .map_err(|e| format!("tar entry path: {}", e))?
            .into_owned();
        if !is_within(dest, &path) {
            return Err(format!(
                "archive entry escapes destination: {}",
                path.display()
            ));
        }
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|e| format!("unpack {}: {}", path.display(), e))?;
        if !unpacked {
            return Err(format!(
                "archive entry escapes destination: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = open(archive)?;
    let mut zip =
        ZipArchive::new(file).map_err(|e| format!("open zip {}: {}", archive.display(), e))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("read zip entry {}: {}", i, e))?;

        let rel = match entry.enclosed_name() {
            Some(rel) => rel,
            None => {
                return Err(format!(
                    "archive entry escapes destination: {}",
                    entry.name()
                ))
            }
        };
        let out = dest.join(&rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|e| format!("create dir {}: {}", out.display(), e))?;
            continue;
        }

        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {}", parent.display(), e))?;
        }
        let mut writer =
            File::create(&out).map_err(|e| format!("create {}: {}", out.display(), e))?;
        std::io::copy(&mut entry, &mut writer)
            .map_err(|e| format!("extract {}: {}", out.display(), e))?;

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cockatiel_extract_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn write_tar_zst(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let tar = tar_bytes(entries);
        let zst = zstd::stream::encode_all(&tar[..], 3).unwrap();
        let path = dir.join("a.tar.zst");
        std::fs::write(&path, zst).unwrap();
        path
    }

    /// Build a single-entry tar without the `tar` crate's path validation, so
    /// a `..` entry can actually be written to disk.
    fn raw_tar(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", data.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        let chksum = format!("{:06o}\0 ", sum);
        header[148..156].copy_from_slice(chksum.as_bytes());

        let mut out = header.to_vec();
        out.extend_from_slice(data);
        let pad = (512 - (data.len() % 512)) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        out
    }

    fn write_tar_gz(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let tar = tar_bytes(entries);
        let path = dir.join("a.tar.gz");
        let file = File::create(&path).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap();
        path
    }

    fn write_zip(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("a.zip");
        let file = File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
        path
    }

    #[test]
    fn extracts_tar_zst() {
        let root = temp_dir("tarzst");
        let archive = write_tar_zst(&root, &[("dir/hello.txt", b"hello")]);
        let dest = root.join("out");
        ArchiveExtractor.extract(&archive, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("dir/hello.txt")).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extracts_tar_gz() {
        let root = temp_dir("targz");
        let archive = write_tar_gz(&root, &[("hello.txt", b"gz")]);
        let dest = root.join("out");
        ArchiveExtractor.extract(&archive, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("hello.txt")).unwrap(), b"gz");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extracts_plain_tar() {
        let root = temp_dir("tar");
        let path = root.join("a.tar");
        std::fs::write(&path, tar_bytes(&[("hello.txt", b"plain")])).unwrap();
        let dest = root.join("out");
        ArchiveExtractor.extract(&path, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("hello.txt")).unwrap(), b"plain");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extracts_zip() {
        let root = temp_dir("zip");
        let archive = write_zip(&root, &[("hello.txt", b"world"), ("sub/x.txt", b"x")]);
        let dest = root.join("out");
        ArchiveExtractor.extract(&archive, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("hello.txt")).unwrap(), b"world");
        assert_eq!(std::fs::read(dest.join("sub/x.txt")).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_tar_traversal() {
        let root = temp_dir("tartrav");
        let archive = root.join("evil.tar.zst");
        let tar = raw_tar("../evil.txt", b"nope");
        std::fs::write(&archive, zstd::stream::encode_all(&tar[..], 3).unwrap()).unwrap();
        let dest = root.join("out");
        let err = ArchiveExtractor.extract(&archive, &dest).unwrap_err();
        assert!(err.contains("escapes destination"), "{}", err);
        assert!(!root.join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_zip_traversal() {
        let root = temp_dir("ziptrav");
        let archive = write_zip(&root, &[("../evil.txt", b"nope")]);
        let dest = root.join("out");
        let err = ArchiveExtractor.extract(&archive, &dest).unwrap_err();
        assert!(err.contains("escapes destination"), "{}", err);
        assert!(!root.join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_unknown_extension() {
        let root = temp_dir("unknown");
        let path = root.join("a.rar");
        std::fs::write(&path, b"x").unwrap();
        let err = ArchiveExtractor.extract(&path, &root.join("out")).unwrap_err();
        assert!(err.contains("unsupported archive extension"), "{}", err);
        let _ = std::fs::remove_dir_all(&root);
    }
}
