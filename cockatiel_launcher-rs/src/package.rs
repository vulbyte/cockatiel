//! Turning a built component binary into a release archive plus the
//! `release.assets` manifest entry the launcher consumes.
//!
//! The archive format is chosen from the resolved file name extension:
//! `.zip` -> zip, `.tar.gz`/`.tgz` -> gzip'd tar, `.tar` -> plain tar, and
//! `.tar.zst`/`.tzst` is rejected because `ruzstd` is decode-only and a
//! C-backed zstd encoder would break the crate's pure-Rust property.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::fetch::sha256_file;
use crate::manifest::{
    default_asset_pattern, interpolate, ComponentManifest, ReleaseAsset,
};
use crate::platform::Platform;

#[derive(Debug, Clone)]
pub struct PackageOptions {
    pub manifest_path: PathBuf,
    pub binary: PathBuf,
    pub out_dir: PathBuf,
    pub platform: Platform,
    pub version: Option<String>,
    pub write_manifest: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageOutput {
    pub archive: PathBuf,
    pub sha256: String,
    pub asset_file: String,
    pub asset: ReleaseAsset,
}

pub fn package(opts: &PackageOptions) -> Result<PackageOutput, String> {
    let manifest = ComponentManifest::load(&opts.manifest_path)?;
    let version = opts
        .version
        .clone()
        .unwrap_or_else(|| manifest.version.clone());
    let platform = &opts.platform;

    let pattern = manifest
        .release
        .as_ref()
        .map(|r| r.pattern_for(platform))
        .unwrap_or_else(|| default_asset_pattern(platform));
    let asset_file = interpolate(&pattern, &manifest.name, &version, platform);

    std::fs::create_dir_all(&opts.out_dir)
        .map_err(|e| format!("create {}: {}", opts.out_dir.display(), e))?;
    let archive = opts.out_dir.join(&asset_file);

    let binary_name = opts
        .binary
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| format!("binary path has no file name: {}", opts.binary.display()))?;

    write_archive(&archive, &opts.binary, &binary_name)?;

    let sha256 = sha256_file(&archive)?;

    let asset = ReleaseAsset {
        file: Some(asset_file.clone()),
        url: None,
        sha256: Some(sha256.clone()),
    };

    if opts.write_manifest {
        patch_manifest(&opts.manifest_path, platform, &asset_file, &sha256, &version)?;
    }

    Ok(PackageOutput {
        archive,
        sha256,
        asset_file,
        asset,
    })
}

fn write_archive(archive: &Path, binary: &Path, name: &str) -> Result<(), String> {
    let lower = archive
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") {
        Err("zstd encoding is not supported; use .tar.gz or .zip".to_string())
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        write_tar_gz(archive, binary, name)
    } else if lower.ends_with(".tar") {
        write_tar(archive, binary, name)
    } else if lower.ends_with(".zip") {
        write_zip(archive, binary, name)
    } else {
        Err(format!(
            "unsupported archive extension for {} (expected .zip, .tar, .tar.gz/.tgz)",
            archive.display()
        ))
    }
}

fn create(archive: &Path) -> Result<File, String> {
    File::create(archive).map_err(|e| format!("create {}: {}", archive.display(), e))
}

fn append_tar<W: Write>(writer: W, binary: &Path, name: &str) -> Result<W, String> {
    let mut builder = tar::Builder::new(writer);
    builder
        .append_path_with_name(binary, name)
        .map_err(|e| format!("add {} to tar: {}", binary.display(), e))?;
    builder
        .into_inner()
        .map_err(|e| format!("finish tar: {}", e))
}

fn write_tar(archive: &Path, binary: &Path, name: &str) -> Result<(), String> {
    append_tar(create(archive)?, binary, name)?;
    Ok(())
}

fn write_tar_gz(archive: &Path, binary: &Path, name: &str) -> Result<(), String> {
    let encoder = flate2::write::GzEncoder::new(create(archive)?, flate2::Compression::default());
    let encoder = append_tar(encoder, binary, name)?;
    encoder
        .finish()
        .map_err(|e| format!("finish gzip {}: {}", archive.display(), e))?;
    Ok(())
}

fn write_zip(archive: &Path, binary: &Path, name: &str) -> Result<(), String> {
    let mut writer = zip::ZipWriter::new(create(archive)?);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);
    writer
        .start_file(name, options)
        .map_err(|e| format!("start zip entry {}: {}", name, e))?;
    let mut input =
        File::open(binary).map_err(|e| format!("open {}: {}", binary.display(), e))?;
    std::io::copy(&mut input, &mut writer)
        .map_err(|e| format!("write zip entry {}: {}", name, e))?;
    writer
        .finish()
        .map_err(|e| format!("finish zip {}: {}", archive.display(), e))?;
    Ok(())
}

/// Patch `release.assets[os][arch]` (and `release.tag` when absent) in the
/// manifest, preserving every other field, then write it atomically.
fn patch_manifest(
    manifest_path: &Path,
    platform: &Platform,
    asset_file: &str,
    sha256: &str,
    version: &str,
) -> Result<(), String> {
    let data = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("read {}: {}", manifest_path.display(), e))?;
    let mut value: serde_json::Value = serde_json::from_str(&data)
        .map_err(|e| format!("parse {}: {}", manifest_path.display(), e))?;

    let root = value
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", manifest_path.display()))?;
    let release = root
        .entry("release")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("manifest `release` is not an object")?;

    if release.get("tag").is_none_or(|v| v.is_null()) {
        release.insert("tag".to_string(), serde_json::json!(format!("v{}", version)));
    }

    let assets = release
        .entry("assets")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("manifest `release.assets` is not an object")?;
    let by_arch = assets
        .entry(platform.os.clone())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("manifest `release.assets.<os>` is not an object")?;
    by_arch.insert(
        platform.arch.clone(),
        serde_json::json!({ "file": asset_file, "sha256": sha256 }),
    );

    let rendered =
        serde_json::to_string_pretty(&value).map_err(|e| format!("serialize manifest: {}", e))?;
    atomic_write(manifest_path, rendered.as_bytes())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "manifest".to_string());
    let tmp = dir.join(format!(
        ".{}.tmp.{}.{}",
        file_name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {} -> {}: {}", tmp.display(), path.display(), e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{ArchiveExtractor, Extractor};
    use crate::resolve::{resolve, Overrides, Plan};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cockatiel_package_{}_{}_{}",
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

    fn write_manifest(dir: &Path, json: &str) -> PathBuf {
        let path = dir.join("cockatiel_module_info.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    fn write_binary(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn packages_tar_gz_and_round_trips() {
        let root = temp_dir("targz");
        let manifest = write_manifest(
            &root,
            r#"{
                "name": "tts-rs",
                "version": "1.2.3",
                "release": { "base_url": "https://example.com/releases/download" }
            }"#,
        );
        let payload = b"\x7fELF fake binary payload";
        let binary = write_binary(&root, "tts-rs", payload);
        let opts = PackageOptions {
            manifest_path: manifest,
            binary,
            out_dir: root.join("dist"),
            platform: Platform::new("linux", "x86_64"),
            version: None,
            write_manifest: false,
        };

        let output = package(&opts).unwrap();
        assert_eq!(output.asset_file, "tts-rs-1.2.3-linux-x86_64.tar.gz");
        assert!(output.archive.is_file());
        assert_eq!(output.sha256, sha256_file(&output.archive).unwrap());
        assert_eq!(output.asset.file.as_deref(), Some(output.asset_file.as_str()));
        assert_eq!(output.asset.sha256.as_deref(), Some(output.sha256.as_str()));
        assert!(output.asset.url.is_none());

        let dest = root.join("extracted");
        ArchiveExtractor.extract(&output.archive, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("tts-rs")).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn packages_windows_zip_and_round_trips() {
        let root = temp_dir("zip");
        let manifest = write_manifest(
            &root,
            r#"{
                "name": "tui",
                "version": "0.9.0",
                "release": { "base_url": "https://example.com/releases/download" }
            }"#,
        );
        let payload = b"MZ fake windows binary";
        let binary = write_binary(&root, "cockatiel-tui-v2.exe", payload);
        let opts = PackageOptions {
            manifest_path: manifest,
            binary,
            out_dir: root.join("dist"),
            platform: Platform::new("windows", "x86_64"),
            version: None,
            write_manifest: false,
        };

        let output = package(&opts).unwrap();
        assert_eq!(output.asset_file, "tui-0.9.0-windows-x86_64.zip");
        assert!(output.archive.is_file());
        assert_eq!(output.sha256, sha256_file(&output.archive).unwrap());

        let dest = root.join("extracted");
        ArchiveExtractor.extract(&output.archive, &dest).unwrap();
        assert_eq!(
            std::fs::read(dest.join("cockatiel-tui-v2.exe")).unwrap(),
            payload
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_zstd_asset_pattern() {
        let root = temp_dir("zst");
        let manifest = write_manifest(
            &root,
            r#"{
                "name": "tts-rs",
                "version": "1.2.3",
                "release": {
                    "base_url": "https://example.com/releases/download",
                    "asset_pattern": "{name}-{version}-{os}-{arch}.tar.zst"
                }
            }"#,
        );
        let binary = write_binary(&root, "tts-rs", b"bin");
        let opts = PackageOptions {
            manifest_path: manifest,
            binary,
            out_dir: root.join("dist"),
            platform: Platform::new("linux", "x86_64"),
            version: None,
            write_manifest: false,
        };

        let err = package(&opts).unwrap_err();
        assert!(err.contains("zstd encoding is not supported"), "{}", err);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_manifest_patches_assets_and_resolve_sees_it() {
        let root = temp_dir("write");
        let manifest = write_manifest(
            &root,
            r#"{
                "name": "tts-rs",
                "version": "1.2.3",
                "kind": "module",
                "release": {
                    "repo": "vulbyte/cockatiel_module-tts-rs",
                    "base_url": "https://example.com/releases/download",
                    "assets": {
                        "macos": { "aarch64": { "url": "https://old", "sha256": "old" } }
                    }
                }
            }"#,
        );
        let binary = write_binary(&root, "tts-rs", b"bin");
        let platform = Platform::new("linux", "x86_64");
        let opts = PackageOptions {
            manifest_path: manifest.clone(),
            binary,
            out_dir: root.join("dist"),
            platform: platform.clone(),
            version: None,
            write_manifest: true,
        };

        let output = package(&opts).unwrap();

        let patched = ComponentManifest::load(&manifest).unwrap();
        assert_eq!(patched.kind, "module");
        let release = patched.release.as_ref().unwrap();
        assert_eq!(release.tag.as_deref(), Some("v1.2.3"));
        assert_eq!(
            release.repo.as_deref(),
            Some("vulbyte/cockatiel_module-tts-rs")
        );
        let asset = release
            .assets
            .get("linux")
            .and_then(|a| a.get("x86_64"))
            .unwrap();
        assert_eq!(asset.file.as_deref(), Some(output.asset_file.as_str()));
        assert_eq!(asset.sha256.as_deref(), Some(output.sha256.as_str()));
        assert!(asset.url.is_none());

        let old = release
            .assets
            .get("macos")
            .and_then(|a| a.get("aarch64"))
            .unwrap();
        assert_eq!(old.url.as_deref(), Some("https://old"));
        assert_eq!(old.sha256.as_deref(), Some("old"));

        let plan = resolve(&patched, root.clone(), None, &platform, &Overrides::default());
        match plan {
            Plan::Download { sha256, .. } => {
                assert_eq!(sha256.as_deref(), Some(output.sha256.as_str()))
            }
            other => panic!("expected a download plan, got {:?}", other),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn version_override_changes_asset_name() {
        let root = temp_dir("version");
        let manifest = write_manifest(
            &root,
            r#"{
                "name": "engine",
                "version": "1.0.0",
                "release": { "base_url": "https://example.com/releases/download" }
            }"#,
        );
        let binary = write_binary(&root, "cockatiel-engine-rs", b"bin");
        let opts = PackageOptions {
            manifest_path: manifest,
            binary,
            out_dir: root.join("dist"),
            platform: Platform::new("macos", "aarch64"),
            version: Some("2.0.0".to_string()),
            write_manifest: false,
        };

        let output = package(&opts).unwrap();
        assert_eq!(output.asset_file, "engine-2.0.0-macos-aarch64.tar.gz");
        let _ = std::fs::remove_dir_all(&root);
    }
}
