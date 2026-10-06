use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::platform::Platform;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ComponentManifest {
    pub name: String,
    pub version: String,
    /// `engine` / `tui` / `user-db` / `test-runner` for the core components;
    /// empty or `module` for a launchable pipeline module. Mirrors the `kind`
    /// in `cockatiel.lock` and the TUI's plugin discriminator.
    #[allow(dead_code)]
    #[serde(default)]
    pub kind: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub build_command: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub build_flags: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub launch_command: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub root_file: String,
    /// Per-OS/per-arch path to the built binary, relative to the component
    /// source dir, e.g. `{"macos": {"aarch64": "target/release/cockatiel-tui-v2"}}`.
    #[serde(default)]
    pub binary: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub release: Option<ReleaseSpec>,
}

impl ComponentManifest {
    pub fn load(path: &Path) -> Result<ComponentManifest, String> {
        let data = std::fs::read_to_string(path)
            .map_err(|e| format!("read {}: {}", path.display(), e))?;
        serde_json::from_str(&data).map_err(|e| format!("parse {}: {}", path.display(), e))
    }

    /// The declared binary route for `platform`, if the manifest has one.
    pub fn binary_for(&self, platform: &Platform) -> Option<&str> {
        self.binary
            .get(&platform.os)
            .and_then(|by_arch| by_arch.get(&platform.arch))
            .map(String::as_str)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ReleaseSpec {
    #[allow(dead_code)]
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub asset_pattern: Option<String>,
    #[serde(default)]
    pub assets: BTreeMap<String, BTreeMap<String, ReleaseAsset>>,
}

impl ReleaseSpec {
    pub fn tag_for(&self, version: &str) -> String {
        self.tag
            .clone()
            .unwrap_or_else(|| format!("v{}", version))
    }

    pub fn pattern_for(&self, platform: &Platform) -> String {
        self.asset_pattern
            .clone()
            .unwrap_or_else(|| default_asset_pattern(platform))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ReleaseAsset {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
}

pub fn default_asset_pattern(platform: &Platform) -> String {
    if platform.os == "windows" {
        "{name}-{version}-{os}-{arch}.zip".to_string()
    } else {
        "{name}-{version}-{os}-{arch}.tar.gz".to_string()
    }
}

pub fn interpolate(pattern: &str, name: &str, version: &str, platform: &Platform) -> String {
    pattern
        .replace("{name}", name)
        .replace("{version}", version)
        .replace("{os}", &platform.os)
        .replace("{arch}", &platform.arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
        "name": "tts-rs",
        "version": "0.1.0",
        "release": {
            "repo": "vulbyte/cockatiel_module-tts-rs",
            "base_url": "https://github.com/vulbyte/cockatiel_module-tts-rs/releases/download",
            "tag": "v0.1.0",
            "asset_pattern": "{name}-{version}-{os}-{arch}.tar.zst",
            "assets": {
                "macos":   { "aarch64": { "file": "tts-rs-0.1.0-macos-aarch64.tar.zst", "sha256": "abc" } },
                "windows": { "x86_64":  { "url": "https://cdn.example.com/full.zip", "sha256": "def" } }
            }
        }
    }"#;

    #[test]
    fn parses_full_release_block() {
        let m: ComponentManifest = serde_json::from_str(FULL).unwrap();
        assert_eq!(m.name, "tts-rs");
        assert_eq!(m.version, "0.1.0");
        let r = m.release.expect("release");
        assert_eq!(r.repo.as_deref(), Some("vulbyte/cockatiel_module-tts-rs"));
        assert_eq!(
            r.base_url.as_deref(),
            Some("https://github.com/vulbyte/cockatiel_module-tts-rs/releases/download")
        );
        assert_eq!(r.tag.as_deref(), Some("v0.1.0"));
        assert_eq!(
            r.asset_pattern.as_deref(),
            Some("{name}-{version}-{os}-{arch}.tar.zst")
        );
        let mac = r.assets.get("macos").unwrap().get("aarch64").unwrap();
        assert_eq!(
            mac.file.as_deref(),
            Some("tts-rs-0.1.0-macos-aarch64.tar.zst")
        );
        assert_eq!(mac.sha256.as_deref(), Some("abc"));
        let win = r.assets.get("windows").unwrap().get("x86_64").unwrap();
        assert_eq!(win.url.as_deref(), Some("https://cdn.example.com/full.zip"));
        assert_eq!(win.sha256.as_deref(), Some("def"));
    }

    #[test]
    fn defaults_tag_and_pattern() {
        let r = ReleaseSpec::default();
        assert_eq!(r.tag_for("0.1.0"), "v0.1.0");
        let mac = Platform::new("macos", "aarch64");
        assert_eq!(r.pattern_for(&mac), "{name}-{version}-{os}-{arch}.tar.gz");
        let win = Platform::new("windows", "x86_64");
        assert_eq!(r.pattern_for(&win), "{name}-{version}-{os}-{arch}.zip");
    }

    #[test]
    fn missing_release_is_none() {
        let m: ComponentManifest =
            serde_json::from_str(r#"{"name":"x","version":"1.2.3"}"#).unwrap();
        assert!(m.release.is_none());
        assert_eq!(m.name, "x");
        assert_eq!(m.version, "1.2.3");
        assert!(m.build_command.is_none());
        assert!(m.build_flags.is_empty());
    }

    #[test]
    fn parses_launcher_fields() {
        let m: ComponentManifest = serde_json::from_str(
            r#"{
                "name":"a","version":"0.1.0",
                "build_command":"cargo","build_flags":["build","--release"],
                "launch_command":"cargo","root_file":"./src/main.rs"
            }"#,
        )
        .unwrap();
        assert_eq!(m.build_command.as_deref(), Some("cargo"));
        assert_eq!(m.build_flags, vec!["build", "--release"]);
        assert_eq!(m.launch_command, "cargo");
        assert_eq!(m.root_file, "./src/main.rs");
    }

    #[test]
    fn parses_kind_and_defaults_to_empty() {
        let core: ComponentManifest =
            serde_json::from_str(r#"{"name":"engine","version":"0.1.0","kind":"engine"}"#).unwrap();
        assert_eq!(core.kind, "engine");
        let module: ComponentManifest =
            serde_json::from_str(r#"{"name":"tts-rs","version":"0.1.0"}"#).unwrap();
        assert!(module.kind.is_empty());
    }

    #[test]
    fn parses_binary_routes_per_platform() {
        let m: ComponentManifest = serde_json::from_str(
            r#"{
                "name":"tui","version":"0.1.0",
                "binary": {
                    "macos": { "aarch64": "target/release/cockatiel-tui-v2" },
                    "windows": { "x86_64": "target/release/cockatiel-tui-v2.exe" }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            m.binary_for(&Platform::new("macos", "aarch64")),
            Some("target/release/cockatiel-tui-v2")
        );
        assert_eq!(
            m.binary_for(&Platform::new("windows", "x86_64")),
            Some("target/release/cockatiel-tui-v2.exe")
        );
        assert_eq!(m.binary_for(&Platform::new("linux", "x86_64")), None);
    }

    #[test]
    fn interpolates_all_placeholders() {
        let p = Platform::new("macos", "aarch64");
        assert_eq!(
            interpolate("{name}-{version}-{os}-{arch}.tar.zst", "tts-rs", "0.1.0", &p),
            "tts-rs-0.1.0-macos-aarch64.tar.zst"
        );
    }

    #[test]
    fn windows_default_pattern_is_zip() {
        let p = Platform::new("windows", "x86_64");
        assert_eq!(default_asset_pattern(&p), "{name}-{version}-{os}-{arch}.zip");
        assert_eq!(
            interpolate(&default_asset_pattern(&p), "a", "1", &p),
            "a-1-windows-x86_64.zip"
        );
    }

    #[test]
    fn non_windows_default_pattern_is_tar_gz() {
        let p = Platform::new("linux", "x86_64");
        assert_eq!(
            default_asset_pattern(&p),
            "{name}-{version}-{os}-{arch}.tar.gz"
        );
    }
}
