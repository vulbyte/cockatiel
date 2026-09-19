use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILENAME: &str = "cockatiel_module_info.json";

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: ModuleManifest,
    pub directory: PathBuf,
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