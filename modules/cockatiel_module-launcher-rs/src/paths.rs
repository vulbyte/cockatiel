//! Install-root resolution and the on-disk layout of an installed Cockatiel
//! tree (Phase 1 contract).
//!
//! ```text
//! <root>/bin/            # launcher + TUI + test-runner binaries
//! <root>/engine/         # engine binary + config
//! <root>/user-db/        # user-db binary
//! <root>/modules/<name>/ # module dirs
//! <root>/config/ tls/ data/ logs/ voices/
//! <root>/.staging/       # temporary downloads/extractions
//! ```
//!
//! Root precedence: `--root` (handled by the CLI) > `COCKATIEL_HOME` env >
//! per-OS default.

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// A path the installer writes into. `Dir` replaces a whole directory; `Bin`
/// copies a single executable into an existing directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Dir(PathBuf),
    Bin { dir: PathBuf, name: String },
}

/// The resolved install root together with the standard subdirectories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn engine_dir(&self) -> PathBuf {
        self.root.join("engine")
    }

    pub fn user_db_dir(&self) -> PathBuf {
        self.root.join("user-db")
    }

    pub fn modules_dir(&self) -> PathBuf {
        self.root.join("modules")
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    pub fn tls_dir(&self) -> PathBuf {
        self.root.join("tls")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn voices_dir(&self) -> PathBuf {
        self.root.join("voices")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(".staging")
    }

    /// Where a component with `key` and lock `kind` gets installed. The core
    /// components are keyed by `kind`; anything else is a module directory
    /// named after its lock key.
    pub fn component_target(&self, key: &str, kind: &str) -> Target {
        self.component_target_with_suffix(key, kind, env::consts::EXE_SUFFIX)
    }

    /// Same as [`component_target`](Self::component_target) but with an
    /// explicit executable suffix, so Windows naming is testable off-Windows.
    pub fn component_target_with_suffix(&self, key: &str, kind: &str, suffix: &str) -> Target {
        match kind {
            "engine" => Target::Dir(self.engine_dir()),
            "user-db" => Target::Dir(self.user_db_dir()),
            "tui" => Target::Bin {
                dir: self.bin_dir(),
                name: format!("cockatiel-tui-v2{}", suffix),
            },
            "test-runner" => Target::Bin {
                dir: self.bin_dir(),
                name: format!("cockatiel-test-runner{}", suffix),
            },
            _ => Target::Dir(self.modules_dir().join(key)),
        }
    }

    /// Create every standard directory (except `.staging`, which is created on
    /// demand by the installer).
    pub fn ensure_dirs(&self) -> io::Result<()> {
        for dir in [
            self.bin_dir(),
            self.engine_dir(),
            self.user_db_dir(),
            self.modules_dir(),
            self.config_dir(),
            self.tls_dir(),
            self.data_dir(),
            self.logs_dir(),
            self.voices_dir(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        Ok(())
    }
}

/// Resolve the install root using the environment of the running process.
pub fn default_install_root() -> PathBuf {
    install_root_for(env::consts::OS, &|key| env::var_os(key))
}

/// Resolve the install root for an explicit `os` and environment lookup, so
/// tests can exercise every platform without mutating the process env.
///
/// `os` is a Rust target OS name (`macos` / `linux` / `windows`).
pub fn install_root_for<F>(os: &str, get: &F) -> PathBuf
where
    F: Fn(&str) -> Option<OsString>,
{
    if let Some(value) = non_empty(get("COCKATIEL_HOME")) {
        return PathBuf::from(value);
    }

    match os {
        "windows" => {
            if let Some(appdata) = non_empty(get("APPDATA")) {
                PathBuf::from(appdata).join("cockatiel")
            } else if let Some(profile) = home_dir(get) {
                profile.join("AppData").join("Roaming").join("cockatiel")
            } else {
                PathBuf::from("cockatiel")
            }
        }
        "macos" => match home_dir(get) {
            Some(home) => home.join("Library").join("Application Support").join("cockatiel"),
            None => PathBuf::from("cockatiel"),
        },
        _ => {
            if let Some(xdg) = non_empty(get("XDG_DATA_HOME")) {
                PathBuf::from(xdg).join("cockatiel")
            } else if let Some(home) = home_dir(get) {
                home.join(".local").join("share").join("cockatiel")
            } else {
                PathBuf::from("cockatiel")
            }
        }
    }
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|v| !v.is_empty())
}

fn home_dir<F>(get: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<OsString>,
{
    non_empty(get("HOME"))
        .or_else(|| non_empty(get("USERPROFILE")))
        .map(PathBuf::from)
}

/// Whether `candidate` is contained within `base` (after lexical
/// normalisation). Used by the extractor's traversal guard.
pub fn is_within(base: &Path, candidate: &Path) -> bool {
    let mut depth: i32 = 0;
    for component in candidate.components() {
        match component {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return false,
            std::path::Component::CurDir => {}
        }
    }
    let _ = base;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(*v))
        }
    }

    #[test]
    fn env_override_wins_on_every_os() {
        let env = env_from(&[("COCKATIEL_HOME", "/custom/root")]);
        assert_eq!(install_root_for("macos", &env), PathBuf::from("/custom/root"));
        assert_eq!(install_root_for("linux", &env), PathBuf::from("/custom/root"));
        assert_eq!(
            install_root_for("windows", &env),
            PathBuf::from("/custom/root")
        );
    }

    #[test]
    fn empty_env_override_is_ignored() {
        let env = env_from(&[("COCKATIEL_HOME", ""), ("HOME", "/home/u")]);
        assert_eq!(
            install_root_for("macos", &env),
            PathBuf::from("/home/u/Library/Application Support/cockatiel")
        );
    }

    #[test]
    fn macos_default_uses_home() {
        let env = env_from(&[("HOME", "/Users/u")]);
        assert_eq!(
            install_root_for("macos", &env),
            PathBuf::from("/Users/u/Library/Application Support/cockatiel")
        );
    }

    #[test]
    fn linux_default_prefers_xdg_then_home() {
        let xdg = env_from(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/u")]);
        assert_eq!(
            install_root_for("linux", &xdg),
            PathBuf::from("/xdg/cockatiel")
        );
        let home = env_from(&[("HOME", "/home/u")]);
        assert_eq!(
            install_root_for("linux", &home),
            PathBuf::from("/home/u/.local/share/cockatiel")
        );
    }

    #[test]
    fn windows_default_uses_appdata() {
        let env = env_from(&[("APPDATA", "C:/Users/u/AppData/Roaming")]);
        assert_eq!(
            install_root_for("windows", &env),
            PathBuf::from("C:/Users/u/AppData/Roaming").join("cockatiel")
        );
    }

    #[test]
    fn windows_default_falls_back_to_userprofile() {
        let env = env_from(&[("USERPROFILE", "C:/Users/u")]);
        assert_eq!(
            install_root_for("windows", &env),
            PathBuf::from("C:/Users/u/AppData/Roaming/cockatiel")
        );
    }

    #[test]
    fn target_mapping_per_kind() {
        let layout = Layout::new(PathBuf::from("/r"));
        assert_eq!(
            layout.component_target("engine", "engine"),
            Target::Dir(PathBuf::from("/r/engine"))
        );
        assert_eq!(
            layout.component_target("user-db", "user-db"),
            Target::Dir(PathBuf::from("/r/user-db"))
        );
        assert_eq!(
            layout.component_target("tui", "tui"),
            Target::Bin {
                dir: PathBuf::from("/r/bin"),
                name: "cockatiel-tui-v2".to_string(),
            }
        );
        assert_eq!(
            layout.component_target("test-runner", "test-runner"),
            Target::Bin {
                dir: PathBuf::from("/r/bin"),
                name: "cockatiel-test-runner".to_string(),
            }
        );
        assert_eq!(
            layout.component_target("tts-rs", "module"),
            Target::Dir(PathBuf::from("/r/modules/tts-rs"))
        );
    }

    #[test]
    fn windows_bin_names_get_exe_suffix() {
        let layout = Layout::new(PathBuf::from("C:/r"));
        assert_eq!(
            layout.component_target_with_suffix("tui", "tui", ".exe"),
            Target::Bin {
                dir: PathBuf::from("C:/r/bin"),
                name: "cockatiel-tui-v2.exe".to_string(),
            }
        );
        assert_eq!(
            layout.component_target_with_suffix("test-runner", "test-runner", ".exe"),
            Target::Bin {
                dir: PathBuf::from("C:/r/bin"),
                name: "cockatiel-test-runner.exe".to_string(),
            }
        );
    }

    #[test]
    fn directory_accessors_match_layout() {
        let layout = Layout::new(PathBuf::from("/r"));
        assert_eq!(layout.bin_dir(), PathBuf::from("/r/bin"));
        assert_eq!(layout.engine_dir(), PathBuf::from("/r/engine"));
        assert_eq!(layout.user_db_dir(), PathBuf::from("/r/user-db"));
        assert_eq!(layout.modules_dir(), PathBuf::from("/r/modules"));
        assert_eq!(layout.config_dir(), PathBuf::from("/r/config"));
        assert_eq!(layout.tls_dir(), PathBuf::from("/r/tls"));
        assert_eq!(layout.data_dir(), PathBuf::from("/r/data"));
        assert_eq!(layout.logs_dir(), PathBuf::from("/r/logs"));
        assert_eq!(layout.voices_dir(), PathBuf::from("/r/voices"));
        assert_eq!(layout.staging_dir(), PathBuf::from("/r/.staging"));
    }

    #[test]
    fn ensure_dirs_creates_the_standard_tree() {
        let root = std::env::temp_dir().join(format!(
            "cockatiel_paths_ensure_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let layout = Layout::new(root.clone());
        layout.ensure_dirs().unwrap();
        for dir in [
            layout.bin_dir(),
            layout.engine_dir(),
            layout.user_db_dir(),
            layout.modules_dir(),
            layout.config_dir(),
            layout.tls_dir(),
            layout.data_dir(),
            layout.logs_dir(),
            layout.voices_dir(),
        ] {
            assert!(dir.is_dir(), "{} should exist", dir.display());
        }
        assert!(!layout.staging_dir().exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn is_within_rejects_escapes() {
        let base = Path::new("/dest");
        assert!(is_within(base, Path::new("a/b")));
        assert!(is_within(base, Path::new("./a")));
        assert!(!is_within(base, Path::new("../evil")));
        assert!(!is_within(base, Path::new("a/../../evil")));
        assert!(!is_within(base, Path::new("/abs")));
    }
}
