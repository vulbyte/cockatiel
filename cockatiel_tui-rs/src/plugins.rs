use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILENAME: &str = "cockatiel_module_info.json";

/// Per-OS prebuilt binary routes: OS key → architecture key → relative path.
///
/// Accepted shapes:
///   nested:  `"binary": { "macos": { "aarch64": "target/release/foo", "x86_64": "..." } }`
///   legacy:  `"binary": { "macos": "target/release/foo" }`  (treated as any-arch, key "*")
#[derive(Debug, Clone, Default)]
pub struct BinaryRoutes(pub HashMap<String, HashMap<String, String>>);

impl<'de> Deserialize<'de> for BinaryRoutes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
        if let serde_json::Value::Object(map) = v {
            for (os, val) in map {
                match val {
                    serde_json::Value::String(p) => {
                        let mut m = HashMap::new();
                        m.insert("*".to_string(), p);
                        out.insert(os, m);
                    }
                    serde_json::Value::Object(arch_map) => {
                        let mut m = HashMap::new();
                        for (arch, p) in arch_map {
                            if let serde_json::Value::String(p) = p {
                                m.insert(arch, p);
                            }
                        }
                        out.insert(os, m);
                    }
                    _ => {}
                }
            }
        }
        Ok(BinaryRoutes(out))
    }
}

/// Manifest schema for a module. Most fields are consumed by the supervisor
/// (launch/build/terminal/binary); the rest are retained for completeness with
/// the manifest file but not yet read by the TUI (the engine owns credentials).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct CredentialField {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub list: bool,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ModuleManifest {
    pub name: String,

    #[serde(default)]
    pub description: String,

    #[serde(default)]
    pub version: String,

    #[serde(default)]
    pub capabilities: String,

    #[serde(default)]
    pub root_file: String,

    #[serde(default)]
    pub launch_command: String,

    #[serde(default)]
    pub command_flags: Vec<String>,

    #[serde(default)]
    pub terminal: bool,

    #[serde(default)]
    pub credentials: Vec<CredentialField>,

    /// Prebuilt binary paths per OS + CPU architecture (e.g. macos/aarch64,
/// linux/x86_64). Paths are relative to the module directory. When the path is
/// non-empty AND the file exists, the supervisor runs it directly instead of
/// compiling on every launch. Blank (or missing file) → build per
/// `build_command`/`build_flags` (or the launch command), then run — and a
/// successful build registers the OS/arch route back into this map
/// automatically.
    #[serde(default)]
    pub binary: BinaryRoutes,

    /// Explicit build command (e.g. "cargo") + flags (e.g. ["build", "--release"]).
    /// Falls back to `cargo build --release` when launch_command is cargo, or to
    /// the launch command itself for non-compiled runtimes.
    #[serde(default)]
    pub build_command: Option<String>,

    #[serde(default)]
    pub build_flags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: ModuleManifest,
    pub directory: PathBuf,
}

impl Plugin {
    /// Resolve the plugin's manifest path.
    #[allow(dead_code)]
    pub fn manifest_path(&self) -> PathBuf {
        self.directory.join(MANIFEST_FILENAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_and_legacy_binary_routes() {
        // Nested: os -> arch -> path
        let nested = r#"{"name":"x","binary": {"macos": {"aarch64": "target/release/foo"}}}"#;
        let m: ModuleManifest = serde_json::from_str(nested).unwrap();
        assert_eq!(m.binary.0["macos"]["aarch64"], "target/release/foo");

        // Legacy flat: os -> path (becomes the "*" any-arch route)
        let legacy = r#"{"name":"x","binary": {"linux": "target/release/foo"}}"#;
        let m: ModuleManifest = serde_json::from_str(legacy).unwrap();
        assert_eq!(m.binary.0["linux"]["*"], "target/release/foo");

        // Missing binary entirely → empty routes.
        let none = r#"{"name": "x"}"#;
        let m: ModuleManifest = serde_json::from_str(none).unwrap();
        assert!(m.binary.0.is_empty());
    }
}

/// Recursively walk `root` (and all descendants), collecting directories that
/// contain a `cockatiel_module_info.json`. Directories without the manifest are
/// assumed to be unrelated programs and skipped.
pub fn discover_plugins(root: &Path) -> Vec<Plugin> {
    let mut plugins = Vec::new();
    walk(root, &mut plugins);
    plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    plugins
}

fn walk(dir: &Path, out: &mut Vec<Plugin>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let manifest_path = path.join(MANIFEST_FILENAME);
            if manifest_path.exists() {
                if let Some(plugin) = load_plugin(&path) {
                    out.push(plugin);
                }
                // Do not recurse into a plugin directory's internals
                // (e.g. src/, target/) — a plugin is a leaf.
                continue;
            }
            walk(&path, out);
        }
    }
}

fn load_plugin(dir: &Path) -> Option<Plugin> {
    let manifest_path = dir.join(MANIFEST_FILENAME);
    let contents = std::fs::read_to_string(&manifest_path).ok()?;
    let manifest: ModuleManifest = serde_json::from_str(&contents).ok()?;
    if manifest.name.trim().is_empty() {
        return None;
    }
    Some(Plugin {
        manifest,
        directory: dir.to_path_buf(),
    })
}