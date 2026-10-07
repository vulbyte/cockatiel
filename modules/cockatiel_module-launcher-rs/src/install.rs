//! The installer: download-or-build each locked component and place it into
//! the install root atomically and idempotently.
//!
//! Nothing here is fatal per-component: a failed component is recorded in the
//! [`Report`] and the loop continues. Only setup errors (a missing/unreadable
//! lock, an uncreatable root) bubble up as `Err`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::build::{build_from_source, CommandRunner};
use crate::extract::Extractor;
use crate::fetch::{verify_sha256, Fetcher};
use crate::lockfile::Lock;
use crate::manifest::ComponentManifest;
use crate::paths::{Layout, Target};
use crate::platform::Platform;
use crate::resolve::{resolve, Overrides, Plan};

pub struct InstallOptions {
    pub root: PathBuf,
    pub lock_path: PathBuf,
    pub repo_root: PathBuf,
    pub only: Vec<String>,
    pub force: bool,
    pub assume_yes: bool,
    pub allow_brew: bool,
    /// May we prompt on the terminal? Set false for `--yes` or a non-TTY stdin.
    pub interactive: bool,
    pub overrides: Overrides,
    pub platform: Platform,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub installed: Vec<String>,
    pub skipped: Vec<String>,
    pub built: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl Report {
    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    components: BTreeMap<String, StateEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateEntry {
    version: String,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

impl State {
    fn load(path: &Path) -> State {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {}", parent.display(), e))?;
        }
        let tmp = path.with_file_name(format!(
            "{}.tmp-{}",
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "state".to_string()),
            rand_suffix()
        ));
        let data = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialise state: {}", e))?;
        std::fs::write(&tmp, data).map_err(|e| format!("write {}: {}", tmp.display(), e))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {}", tmp.display(), e))?;
        Ok(())
    }
}

enum Outcome {
    Installed,
    Built,
}

/// Install every locked component (optionally restricted to `opts.only`).
pub fn install_all(
    opts: &InstallOptions,
    fetcher: &dyn Fetcher,
    extractor: &dyn Extractor,
    runner: &dyn CommandRunner,
) -> Result<Report, String> {
    let layout = Layout::new(opts.root.clone());
    layout
        .ensure_dirs()
        .map_err(|e| format!("create install root {}: {}", opts.root.display(), e))?;

    let lock = Lock::load(&opts.lock_path)?;
    let state_path = state_path(&layout);
    let mut state = State::load(&state_path);
    let mut report = Report::default();

    let only: BTreeSet<&str> = opts.only.iter().map(String::as_str).collect();

    for (key, locked) in &lock.components {
        if !only.is_empty() && !only.contains(key.as_str()) {
            continue;
        }

        let manifest_path = opts
            .repo_root
            .join(&locked.path)
            .join("cockatiel_module_info.json");
        let manifest = match ComponentManifest::load(&manifest_path) {
            Ok(m) => m,
            Err(e) => {
                report.failed.push((key.clone(), e));
                continue;
            }
        };
        let manifest_dir = manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let plan = resolve(
            &manifest,
            manifest_dir,
            Some(locked),
            &opts.platform,
            &opts.overrides,
        );

        let (version, sha, url) = plan_identity(&plan, &manifest);

        if !opts.force {
            if let Some(entry) = state.components.get(key) {
                if entry.version == version && sha_matches(entry.sha.as_deref(), sha.as_deref()) {
                    report.skipped.push(key.clone());
                    continue;
                }
            }
        }

        match install_component(key, &locked.kind, &plan, &manifest, &layout, opts, fetcher, extractor, runner) {
            Ok(outcome) => {
                // macOS: strip the quarantine flag from the installed component
                // so Gatekeeper does not block an unsigned (un-notarized)
                // binary. Downloads made by curl/wget are not quarantined, but
                // a manually-extracted or pre-fetched archive may be. No-op when
                // the flag is absent.
                clear_quarantine(&layout.component_target(key, &locked.kind));
                state.components.insert(
                    key.clone(),
                    StateEntry {
                        version,
                        sha,
                        url,
                    },
                );
                if let Err(e) = state.save(&state_path) {
                    report.failed.push((key.clone(), e));
                    continue;
                }
                match outcome {
                    Outcome::Installed => report.installed.push(key.clone()),
                    Outcome::Built => report.built.push(key.clone()),
                }
            }
            Err(e) => report.failed.push((key.clone(), e)),
        }
    }

    Ok(report)
}

fn state_path(layout: &Layout) -> PathBuf {
    layout.root.join(".cockatiel-state.json")
}

/// Remove macOS's `com.apple.quarantine` attribute from an installed component
/// (best-effort; a no-op off macOS and when the attribute is absent). macOS
/// Gatekeeper blocks unsigned, un-notarized binaries that carry the flag; since
/// Cockatiel is not notarized, clearing it is the local workaround.
fn clear_quarantine(target: &Target) {
    #[cfg(target_os = "macos")]
    {
        let path = match target {
            Target::Dir(p) => p.clone(),
            Target::Bin { dir, name } => dir.join(name),
        };
        let _ = std::process::Command::new("xattr")
            .arg("-dr")
            .arg("com.apple.quarantine")
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = target;
    }
}

fn plan_identity(plan: &Plan, manifest: &ComponentManifest) -> (String, Option<String>, Option<String>) {
    match plan {
        Plan::Download {
            version,
            url,
            sha256,
            ..
        } => (version.clone(), sha256.clone(), Some(url.clone())),
        Plan::Build { .. } => (manifest.version.clone(), None, None),
    }
}

fn sha_matches(installed: Option<&str>, wanted: Option<&str>) -> bool {
    match (installed, wanted) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => true,
    }
}

#[allow(clippy::too_many_arguments)]
fn install_component(
    key: &str,
    kind: &str,
    plan: &Plan,
    manifest: &ComponentManifest,
    layout: &Layout,
    opts: &InstallOptions,
    fetcher: &dyn Fetcher,
    extractor: &dyn Extractor,
    runner: &dyn CommandRunner,
) -> Result<Outcome, String> {
    let target = layout.component_target(key, kind);
    match plan {
        Plan::Download { url, sha256, .. } => {
            let staging = layout.staging_dir();
            std::fs::create_dir_all(&staging)
                .map_err(|e| format!("create {}: {}", staging.display(), e))?;
            let archive = staging.join(archive_name(key, url));
            let extract_dir = staging.join(format!("{}.d", key));

            cleanup(&archive);
            cleanup(&extract_dir);

            let result = (|| -> Result<(), String> {
                fetcher.fetch(url, &archive)?;
                if let Some(expected) = sha256 {
                    verify_sha256(&archive, expected)?;
                } else {
                    // Phase 5 publishes checksums; until a component has one we
                    // install its archive unverified, but never silently.
                    eprintln!(
                        "warning: {} has no sha256 in its manifest — installing unverified ({})",
                        key, url
                    );
                }
                extractor.extract(&archive, &extract_dir)?;
                atomic_install(&target, &extract_dir)
            })();

            cleanup(&archive);
            cleanup(&extract_dir);
            result.map(|()| Outcome::Installed)
        }
        Plan::Build { source, .. } => {
            let binary = build_from_source(
                runner,
                source,
                manifest,
                opts.allow_brew,
                opts.assume_yes,
                opts.interactive,
            )?;
            let staging = layout.staging_dir();
            std::fs::create_dir_all(&staging)
                .map_err(|e| format!("create {}: {}", staging.display(), e))?;
            let stage_dir = staging.join(format!("{}.d", key));
            cleanup(&stage_dir);

            let result = (|| -> Result<(), String> {
                std::fs::create_dir_all(&stage_dir)
                    .map_err(|e| format!("create {}: {}", stage_dir.display(), e))?;
                let file_name = binary
                    .file_name()
                    .ok_or_else(|| format!("built binary {} has no file name", binary.display()))?;
                let staged_binary = stage_dir.join(file_name);
                std::fs::copy(&binary, &staged_binary).map_err(|e| {
                    format!(
                        "stage {} -> {}: {}",
                        binary.display(),
                        staged_binary.display(),
                        e
                    )
                })?;
                atomic_install(&target, &stage_dir)
            })();

            cleanup(&stage_dir);
            result.map(|()| Outcome::Built)
        }
    }
}

/// The staging archive name. The `<key>.archive` prefix is the contract; the
/// download URL's extension is appended so the extractor can dispatch on it.
fn archive_name(key: &str, url: &str) -> String {
    let lower = url
        .split('?')
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    for ext in [".tar.zst", ".tar.gz", ".tgz", ".tzst", ".zip", ".tar"] {
        if lower.ends_with(ext) {
            return format!("{}.archive{}", key, ext);
        }
    }
    format!("{}.archive", key)
}

fn atomic_install(target: &Target, staged: &Path) -> Result<(), String> {
    match target {
        Target::Dir(dest) => install_dir(dest, staged),
        Target::Bin { dir, name } => install_bin(dir, name, staged),
    }
}

fn install_dir(dest: &Path, staged: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }

    let backup = if dest.exists() {
        let backup = unique_sibling(dest, "old");
        std::fs::rename(dest, &backup)
            .map_err(|e| format!("move aside {}: {}", dest.display(), e))?;
        Some(backup)
    } else {
        None
    };

    match std::fs::rename(staged, dest) {
        Ok(()) => {
            if let Some(backup) = backup {
                let _ = std::fs::remove_dir_all(&backup);
            }
            Ok(())
        }
        Err(e) => {
            if let Some(backup) = &backup {
                let _ = std::fs::rename(backup, dest);
            }
            Err(format!("install {}: {}", dest.display(), e))
        }
    }
}

fn install_bin(dir: &Path, name: &str, staged: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {}", dir.display(), e))?;
    let found = find_file(staged, name)
        .ok_or_else(|| format!("no file named `{}` found under {}", name, staged.display()))?;

    let out = dir.join(name);
    let tmp = dir.join(format!(".{}.tmp-{}", name, rand_suffix()));
    std::fs::copy(&found, &tmp)
        .map_err(|e| format!("stage {} -> {}: {}", found.display(), tmp.display(), e))?;
    set_executable(&tmp)?;

    let backup = if out.exists() {
        let backup = unique_sibling(&out, "old");
        std::fs::rename(&out, &backup)
            .map_err(|e| format!("move aside {}: {}", out.display(), e))?;
        Some(backup)
    } else {
        None
    };

    match std::fs::rename(&tmp, &out) {
        Ok(()) => {
            if let Some(backup) = backup {
                let _ = std::fs::remove_file(&backup);
            }
            Ok(())
        }
        Err(e) => {
            if let Some(backup) = &backup {
                let _ = std::fs::rename(backup, &out);
            }
            let _ = std::fs::remove_file(&tmp);
            Err(format!("install {}: {}", out.display(), e))
        }
    }
}

fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| format!("stat {}: {}", path.display(), e))?
        .permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms).map_err(|e| format!("chmod {}: {}", path.display(), e))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn cleanup(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

fn unique_sibling(path: &Path, tag: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "component".to_string());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{}.{}-{}", name, tag, rand_suffix()))
}

fn rand_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::sync::Mutex;

    // ---- fakes -----------------------------------------------------------

    #[derive(Default)]
    struct FakeFetcher {
        payloads: Mutex<BTreeMap<String, Vec<u8>>>,
        fail: Mutex<BTreeSet<String>>,
    }

    impl FakeFetcher {
        fn with_payload(url: &str, bytes: &[u8]) -> Self {
            let mut payloads = BTreeMap::new();
            payloads.insert(url.to_string(), bytes.to_vec());
            Self {
                payloads: Mutex::new(payloads),
                fail: Mutex::new(BTreeSet::new()),
            }
        }
    }

    impl Fetcher for FakeFetcher {
        fn fetch(&self, url: &str, dest: &Path) -> Result<(), String> {
            if self.fail.lock().unwrap().contains(url) {
                return Err(format!("fake fetch failure for {}", url));
            }
            let bytes = self
                .payloads
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .unwrap_or_default();
            std::fs::write(dest, bytes).map_err(|e| e.to_string())
        }
    }

    type ExtractorEntries = Mutex<BTreeMap<String, Vec<(String, Vec<u8>)>>>;

    #[derive(Default)]
    struct FakeExtractor {
        entries: ExtractorEntries,
        fail: Mutex<BTreeSet<String>>,
    }

    impl FakeExtractor {
        fn with_entries(key: &str, entries: &[(&str, &[u8])]) -> Self {
            let mut map = BTreeMap::new();
            map.insert(
                key.to_string(),
                entries
                    .iter()
                    .map(|(p, d)| (p.to_string(), d.to_vec()))
                    .collect(),
            );
            Self {
                entries: Mutex::new(map),
                fail: Mutex::new(BTreeSet::new()),
            }
        }
    }

    impl Extractor for FakeExtractor {
        fn extract(&self, archive: &Path, dest: &Path) -> Result<(), String> {
            let file = archive
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let key = file
                .split(".archive")
                .next()
                .unwrap_or("")
                .to_string();
            if self.fail.lock().unwrap().contains(&key) {
                return Err(format!("fake extract failure for {}", key));
            }
            std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
            let entries = self.entries.lock().unwrap().get(&key).cloned().unwrap_or_default();
            for (rel, bytes) in entries {
                let out = dest.join(&rel);
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&out, bytes).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        present: Mutex<BTreeSet<String>>,
        create: Mutex<Option<(String, Vec<u8>)>>,
    }

    impl FakeRunner {
        fn building(rel: &str, bytes: &[u8]) -> Self {
            let mut present = BTreeSet::new();
            present.insert("cargo".to_string());
            present.insert("cmake".to_string());
            Self {
                present: Mutex::new(present),
                create: Mutex::new(Some((rel.to_string(), bytes.to_vec()))),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn which(&self, program: &str) -> Option<PathBuf> {
            self.present
                .lock()
                .unwrap()
                .contains(program)
                .then(|| PathBuf::from(program))
        }

        fn run(&self, _program: &str, _args: &[String], cwd: &Path) -> Result<(), String> {
            if let Some((rel, bytes)) = self.create.lock().unwrap().clone() {
                let path = cwd.join(rel);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(path, bytes).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }

    // ---- helpers ---------------------------------------------------------

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cockatiel_install_{}_{}_{}",
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

    fn sha256(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    fn platform() -> Platform {
        Platform::current()
    }

    fn download_manifest(file: &str, sha: &str) -> String {
        let os = platform().os;
        let arch = platform().arch;
        format!(
            r#"{{
                "name": "comp", "version": "1.0.0",
                "release": {{
                    "base_url": "https://example.com/releases/download",
                    "assets": {{ "{os}": {{ "{arch}": {{ "file": "{file}", "sha256": "{sha}" }} }} }}
                }}
            }}"#
        )
    }

    fn build_manifest(rel: &str) -> String {
        let os = platform().os;
        let arch = platform().arch;
        format!(
            r#"{{
                "name": "comp", "version": "1.0.0",
                "build_command": "cargo", "build_flags": ["build", "--release"],
                "binary": {{ "{os}": {{ "{arch}": "{rel}" }} }}
            }}"#
        )
    }

    fn write_component(repo_root: &Path, dir: &str, manifest: &str) {
        let dir = repo_root.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cockatiel_module_info.json"), manifest).unwrap();
    }

    fn write_lock(path: &Path, entries: &[(&str, &str, &str, &str)]) {
        let mut components = serde_json::Map::new();
        for (key, dir, kind, version) in entries {
            components.insert(
                key.to_string(),
                serde_json::json!({
                    "path": dir,
                    "sha": null,
                    "version": version,
                    "kind": kind,
                }),
            );
        }
        let lock = serde_json::json!({ "lock_version": 1, "components": components });
        std::fs::write(path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();
    }

    fn options(root: &Path, lock: &Path, repo: &Path) -> InstallOptions {
        InstallOptions {
            root: root.to_path_buf(),
            lock_path: lock.to_path_buf(),
            repo_root: repo.to_path_buf(),
            only: Vec::new(),
            force: false,
            assume_yes: false,
            allow_brew: false,
            interactive: false,
            overrides: Overrides::default(),
            platform: platform(),
        }
    }

    fn no_old_backups(root: &Path) -> bool {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.contains(".old-"))
                    .unwrap_or(false)
                {
                    return false;
                }
            }
        }
        true
    }

    // ---- tests -----------------------------------------------------------

    #[test]
    fn download_installs_dir_target() {
        let root = temp_root("dl_dir");
        let repo = temp_root("dl_dir_repo");
        let lock = root.join("cockatiel.lock");
        let payload = b"archive-bytes";
        let url = "https://example.com/releases/download/v1.0.0/comp-1.0.0.tar.zst";
        write_component(
            &repo,
            "comp",
            &download_manifest("comp-1.0.0.tar.zst", &sha256(payload)),
        );
        write_lock(&lock, &[("m1", "comp", "module", "1.0.0")]);

        let opts = options(&root, &lock, &repo);
        let fetcher = FakeFetcher::with_payload(url, payload);
        let extractor = FakeExtractor::with_entries("m1", &[("m1-binary", b"bin")]);
        let runner = FakeRunner::default();

        let report = install_all(&opts, &fetcher, &extractor, &runner).unwrap();
        assert_eq!(report.installed, vec!["m1"]);
        assert!(report.failed.is_empty());
        assert_eq!(
            std::fs::read(root.join("modules/m1/m1-binary")).unwrap(),
            b"bin"
        );
        assert!(root.join(".cockatiel-state.json").is_file());
        assert!(no_old_backups(&root));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn download_installs_bin_target() {
        let root = temp_root("dl_bin");
        let repo = temp_root("dl_bin_repo");
        let lock = root.join("cockatiel.lock");
        let payload = b"tui-archive";
        let url = "https://example.com/releases/download/v1.0.0/tui-1.0.0.tar.zst";
        write_component(
            &repo,
            "comp",
            &download_manifest("tui-1.0.0.tar.zst", &sha256(payload)),
        );
        write_lock(&lock, &[("tui", "comp", "tui", "1.0.0")]);

        let opts = options(&root, &lock, &repo);
        let fetcher = FakeFetcher::with_payload(url, payload);
        let extractor =
            FakeExtractor::with_entries("tui", &[("nested/cockatiel-tui-v2", b"tui-bin")]);
        let runner = FakeRunner::default();

        let report = install_all(&opts, &fetcher, &extractor, &runner).unwrap();
        assert_eq!(report.installed, vec!["tui"]);
        let name = format!("cockatiel-tui-v2{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(std::fs::read(root.join("bin").join(&name)).unwrap(), b"tui-bin");
        assert!(no_old_backups(&root));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn sha_mismatch_fails_without_installing() {
        let root = temp_root("sha_bad");
        let repo = temp_root("sha_bad_repo");
        let lock = root.join("cockatiel.lock");
        let payload = b"archive-bytes";
        let url = "https://example.com/releases/download/v1.0.0/comp-1.0.0.tar.zst";
        write_component(
            &repo,
            "comp",
            &download_manifest("comp-1.0.0.tar.zst", "deadbeef"),
        );
        write_lock(&lock, &[("m1", "comp", "module", "1.0.0")]);

        let opts = options(&root, &lock, &repo);
        let fetcher = FakeFetcher::with_payload(url, payload);
        let extractor = FakeExtractor::with_entries("m1", &[("m1-binary", b"bin")]);
        let runner = FakeRunner::default();

        let report = install_all(&opts, &fetcher, &extractor, &runner).unwrap();
        assert!(report.installed.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "m1");
        assert!(report.failed[0].1.contains("sha256 mismatch"));
        assert!(!root.join("modules/m1").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn build_fallback_stages_binary() {
        let root = temp_root("build");
        let repo = temp_root("build_repo");
        let lock = root.join("cockatiel.lock");
        let source = temp_root("build_src");
        write_component(&repo, "comp", &build_manifest("target/release/comp-bin"));
        write_lock(&lock, &[("m1", "comp", "module", "1.0.0")]);

        let mut opts = options(&root, &lock, &repo);
        opts.overrides.source = Some(source.clone());
        let fetcher = FakeFetcher::default();
        let extractor = FakeExtractor::default();
        let runner = FakeRunner::building("target/release/comp-bin", b"built");

        let report = install_all(&opts, &fetcher, &extractor, &runner).unwrap();
        assert_eq!(report.built, vec!["m1"]);
        assert!(report.failed.is_empty());
        assert_eq!(
            std::fs::read(root.join("modules/m1/comp-bin")).unwrap(),
            b"built"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn second_run_skips_and_force_reinstalls() {
        let root = temp_root("idem");
        let repo = temp_root("idem_repo");
        let lock = root.join("cockatiel.lock");
        let payload = b"archive-bytes";
        let url = "https://example.com/releases/download/v1.0.0/comp-1.0.0.tar.zst";
        write_component(
            &repo,
            "comp",
            &download_manifest("comp-1.0.0.tar.zst", &sha256(payload)),
        );
        write_lock(&lock, &[("m1", "comp", "module", "1.0.0")]);

        let opts = options(&root, &lock, &repo);
        let fetcher = FakeFetcher::with_payload(url, payload);
        let extractor = FakeExtractor::with_entries("m1", &[("m1-binary", b"bin")]);
        let runner = FakeRunner::default();

        install_all(&opts, &fetcher, &extractor, &runner).unwrap();
        let second = install_all(&opts, &fetcher, &extractor, &runner).unwrap();
        assert_eq!(second.skipped, vec!["m1"]);
        assert!(second.installed.is_empty());

        let mut forced = options(&root, &lock, &repo);
        forced.force = true;
        let third = install_all(&forced, &fetcher, &extractor, &runner).unwrap();
        assert_eq!(third.installed, vec!["m1"]);
        assert!(no_old_backups(&root));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn failure_does_not_stop_other_components() {
        let root = temp_root("partial");
        let repo = temp_root("partial_repo");
        let lock = root.join("cockatiel.lock");
        let payload = b"good-archive";
        let good_url = "https://example.com/releases/download/v1.0.0/good-1.0.0.tar.zst";
        let bad_url = "https://example.com/releases/download/v1.0.0/bad-1.0.0.tar.zst";

        write_component(
            &repo,
            "good",
            &download_manifest("good-1.0.0.tar.zst", &sha256(payload)).replace("\"comp\"", "\"good\""),
        );
        write_component(
            &repo,
            "bad",
            &download_manifest("bad-1.0.0.tar.zst", &sha256(b"x")).replace("\"comp\"", "\"bad\""),
        );
        write_lock(
            &lock,
            &[("good", "good", "module", "1.0.0"), ("bad", "bad", "module", "1.0.0")],
        );

        let opts = options(&root, &lock, &repo);
        let fetcher = FakeFetcher {
            payloads: Mutex::new(BTreeMap::from([(good_url.to_string(), payload.to_vec())])),
            fail: Mutex::new(BTreeSet::from([bad_url.to_string()])),
        };
        let extractor = FakeExtractor::with_entries("good", &[("good-binary", b"good")]);
        let runner = FakeRunner::default();

        let report = install_all(&opts, &fetcher, &extractor, &runner).unwrap();
        assert_eq!(report.installed, vec!["good"]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "bad");
        assert!(root.join("modules/good/good-binary").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn only_filter_restricts_components() {
        let root = temp_root("only");
        let repo = temp_root("only_repo");
        let lock = root.join("cockatiel.lock");
        let payload = b"p";
        let url_a = "https://example.com/releases/download/v1.0.0/a-1.0.0.tar.zst";
        let url_b = "https://example.com/releases/download/v1.0.0/b-1.0.0.tar.zst";
        write_component(
            &repo,
            "a",
            &download_manifest("a-1.0.0.tar.zst", &sha256(payload)).replace("\"comp\"", "\"a\""),
        );
        write_component(
            &repo,
            "b",
            &download_manifest("b-1.0.0.tar.zst", &sha256(payload)).replace("\"comp\"", "\"b\""),
        );
        write_lock(
            &lock,
            &[("a", "a", "module", "1.0.0"), ("b", "b", "module", "1.0.0")],
        );

        let mut opts = options(&root, &lock, &repo);
        opts.only = vec!["a".to_string()];
        let fetcher = FakeFetcher {
            payloads: Mutex::new(BTreeMap::from([
                (url_a.to_string(), payload.to_vec()),
                (url_b.to_string(), payload.to_vec()),
            ])),
            fail: Mutex::new(BTreeSet::new()),
        };
        let extractor = FakeExtractor::with_entries("a", &[("a-binary", b"a")]);
        let runner = FakeRunner::default();

        let report = install_all(&opts, &fetcher, &extractor, &runner).unwrap();
        assert_eq!(report.installed, vec!["a"]);
        assert!(!root.join("modules/b").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
