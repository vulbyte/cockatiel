use serde::Serialize;
use std::path::PathBuf;

use crate::lockfile::LockedComponent;
use crate::manifest::{default_asset_pattern, interpolate, ComponentManifest};
use crate::platform::Platform;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Plan {
    Download {
        component: String,
        version: String,
        url: String,
        sha256: Option<String>,
    },
    Build {
        component: String,
        source: PathBuf,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub release_base: Option<String>,
    pub component_version: Option<String>,
    pub source: Option<PathBuf>,
}

pub fn resolve(
    manifest: &ComponentManifest,
    manifest_dir: PathBuf,
    _locked: Option<&LockedComponent>,
    platform: &Platform,
    overrides: &Overrides,
) -> Plan {
    let component = manifest.name.clone();

    if let Some(source) = &overrides.source {
        return Plan::Build {
            component,
            source: source.clone(),
        };
    }

    let version = overrides
        .component_version
        .clone()
        .unwrap_or_else(|| manifest.version.clone());

    let base_url = overrides
        .release_base
        .clone()
        .or_else(|| manifest.release.as_ref().and_then(|r| r.base_url.clone()));

    let Some(base_url) = base_url else {
        return Plan::Build {
            component,
            source: manifest_dir,
        };
    };

    let tag = manifest
        .release
        .as_ref()
        .map(|r| r.tag_for(&version))
        .unwrap_or_else(|| format!("v{}", version));

    if let Some(asset) = manifest
        .release
        .as_ref()
        .and_then(|r| r.assets.get(&platform.os))
        .and_then(|by_arch| by_arch.get(&platform.arch))
    {
        if let Some(url) = &asset.url {
            return Plan::Download {
                component,
                version,
                url: url.clone(),
                sha256: asset.sha256.clone(),
            };
        }
        if let Some(file) = &asset.file {
            let url = join_url(&base_url, &tag, file);
            return Plan::Download {
                component,
                version,
                url,
                sha256: asset.sha256.clone(),
            };
        }
    }

    let pattern = manifest
        .release
        .as_ref()
        .map(|r| r.pattern_for(platform))
        .unwrap_or_else(|| default_asset_pattern(platform));
    let asset_name = interpolate(&pattern, &manifest.name, &version, platform);
    let url = join_url(&base_url, &tag, &asset_name);

    Plan::Download {
        component,
        version,
        url,
        sha256: None,
    }
}

fn join_url(base: &str, tag: &str, file: &str) -> String {
    format!(
        "{}/{}/{}",
        base.trim_end_matches('/'),
        tag.trim_matches('/'),
        file.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_full() -> ComponentManifest {
        serde_json::from_str(
            r#"{
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
            }"#,
        )
        .unwrap()
    }

    fn manifest_plain() -> ComponentManifest {
        serde_json::from_str(
            r#"{
                "name": "tts-rs",
                "version": "0.1.0",
                "release": {
                    "base_url": "https://example.com/releases/download"
                }
            }"#,
        )
        .unwrap()
    }

    fn dir() -> PathBuf {
        PathBuf::from("/repo/modules/tts-rs")
    }

    #[test]
    fn source_override_wins() {
        let m = manifest_full();
        let p = Platform::new("macos", "aarch64");
        let ov = Overrides {
            source: Some(PathBuf::from("/tmp/src")),
            ..Default::default()
        };
        assert_eq!(
            resolve(&m, dir(), None, &p, &ov),
            Plan::Build {
                component: "tts-rs".to_string(),
                source: PathBuf::from("/tmp/src"),
            }
        );
    }

    #[test]
    fn explicit_file_asset_wins_over_pattern() {
        let m = manifest_full();
        let p = Platform::new("macos", "aarch64");
        assert_eq!(
            resolve(&m, dir(), None, &p, &Overrides::default()),
            Plan::Download {
                component: "tts-rs".to_string(),
                version: "0.1.0".to_string(),
                url: "https://github.com/vulbyte/cockatiel_module-tts-rs/releases/download/v0.1.0/tts-rs-0.1.0-macos-aarch64.tar.zst".to_string(),
                sha256: Some("abc".to_string()),
            }
        );
    }

    #[test]
    fn explicit_url_asset_used_verbatim() {
        let m = manifest_full();
        let p = Platform::new("windows", "x86_64");
        assert_eq!(
            resolve(&m, dir(), None, &p, &Overrides::default()),
            Plan::Download {
                component: "tts-rs".to_string(),
                version: "0.1.0".to_string(),
                url: "https://cdn.example.com/full.zip".to_string(),
                sha256: Some("def".to_string()),
            }
        );
    }

    #[test]
    fn pattern_download_when_base_url_present() {
        let m = manifest_full();
        let p = Platform::new("linux", "x86_64");
        assert_eq!(
            resolve(&m, dir(), None, &p, &Overrides::default()),
            Plan::Download {
                component: "tts-rs".to_string(),
                version: "0.1.0".to_string(),
                url: "https://github.com/vulbyte/cockatiel_module-tts-rs/releases/download/v0.1.0/tts-rs-0.1.0-linux-x86_64.tar.zst".to_string(),
                sha256: None,
            }
        );
    }

    #[test]
    fn pattern_uses_windows_zip_default() {
        let m = manifest_plain();
        let p = Platform::new("windows", "aarch64");
        assert_eq!(
            resolve(&m, dir(), None, &p, &Overrides::default()),
            Plan::Download {
                component: "tts-rs".to_string(),
                version: "0.1.0".to_string(),
                url: "https://example.com/releases/download/v0.1.0/tts-rs-0.1.0-windows-aarch64.zip".to_string(),
                sha256: None,
            }
        );
    }

    #[test]
    fn build_when_no_base_url() {
        let m: ComponentManifest =
            serde_json::from_str(r#"{"name":"tts-rs","version":"0.1.0"}"#).unwrap();
        let p = Platform::new("linux", "x86_64");
        assert_eq!(
            resolve(&m, dir(), None, &p, &Overrides::default()),
            Plan::Build {
                component: "tts-rs".to_string(),
                source: dir(),
            }
        );
    }

    #[test]
    fn build_when_release_has_no_base_url() {
        let m: ComponentManifest = serde_json::from_str(
            r#"{"name":"tts-rs","version":"0.1.0","release":{"tag":"v0.1.0"}}"#,
        )
        .unwrap();
        let p = Platform::new("linux", "x86_64");
        assert_eq!(
            resolve(&m, dir(), None, &p, &Overrides::default()),
            Plan::Build {
                component: "tts-rs".to_string(),
                source: dir(),
            }
        );
    }

    #[test]
    fn component_version_changes_url_and_tag() {
        let m = manifest_plain();
        let p = Platform::new("linux", "x86_64");
        let ov = Overrides {
            component_version: Some("0.2.0".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve(&m, dir(), None, &p, &ov),
            Plan::Download {
                component: "tts-rs".to_string(),
                version: "0.2.0".to_string(),
                url: "https://example.com/releases/download/v0.2.0/tts-rs-0.2.0-linux-x86_64.tar.gz".to_string(),
                sha256: None,
            }
        );
    }

    #[test]
    fn release_base_overrides_base_url() {
        let m = manifest_full();
        let p = Platform::new("linux", "x86_64");
        let ov = Overrides {
            release_base: Some("https://mirror.example.com/dl/".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve(&m, dir(), None, &p, &ov),
            Plan::Download {
                component: "tts-rs".to_string(),
                version: "0.1.0".to_string(),
                url: "https://mirror.example.com/dl/v0.1.0/tts-rs-0.1.0-linux-x86_64.tar.zst".to_string(),
                sha256: None,
            }
        );
    }
}
