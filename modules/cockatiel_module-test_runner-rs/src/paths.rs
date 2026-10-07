//! Relocatable path resolution for the test-runner.
//!
//! An installed Cockatiel tree is self-contained:
//!
//! ```text
//! <root>/bin/            # this binary
//! <root>/engine/         # config.json / .env / modules.json / tls/cockatiel-cert.pem
//! <root>/modules/<name>/
//! <root>/rank_chart.json
//! ```
//!
//! When no install root is known we fall back to the monorepo checkout layout
//! (`cwd/../modules`, `cwd/../cockatiel_engine-rs`) so existing behaviour is
//! preserved. All helpers are pure given the CLI + filesystem; the only
//! environment access is `COCKATIEL_HOME` and the `cwd` legacy fallback.

use std::path::PathBuf;

use crate::Cli;

/// The install root: `--install-root`, else the `COCKATIEL_HOME` environment
/// variable. `None` when neither is set (or the env var is empty).
pub fn install_root(cli: &Cli) -> Option<PathBuf> {
    if let Some(root) = &cli.install_root {
        return Some(root.clone());
    }
    std::env::var_os("COCKATIEL_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Modules directory: `--modules-dir` wins; else `<install_root>/modules` when
/// it exists; else the legacy layout — the test-runner now lives at
/// `<repo>/modules/cockatiel_module-test_runner-rs`, so the modules dir is its
/// parent.
pub fn modules_dir(cli: &Cli) -> PathBuf {
    if let Some(dir) = &cli.modules_dir {
        return dir.clone();
    }
    if let Some(root) = install_root(cli) {
        let candidate = root.join("modules");
        if candidate.exists() {
            return candidate;
        }
    }
    legacy_cwd().join("..")
}

/// Engine config directory: `COCKATIEL_ENGINE_DIR` (set by `cockatiel doctor`
/// to point at its isolated sandbox) when non-empty; else
/// `<install_root>/engine` when it exists; else `<install_root>/cockatiel_engine-rs`
/// (a legacy-named install) when it exists; else the monorepo
/// `cwd/../cockatiel_engine-rs`.
pub fn engine_dir(cli: &Cli) -> PathBuf {
    if let Some(dir) = env_dir("COCKATIEL_ENGINE_DIR") {
        return dir;
    }
    if let Some(root) = install_root(cli) {
        let engine = root.join("engine");
        if engine.exists() {
            return engine;
        }
        let legacy_named = root.join("cockatiel_engine-rs");
        if legacy_named.exists() {
            return legacy_named;
        }
    }
    // Two levels up from <repo>/modules/cockatiel_module-test_runner-rs.
    legacy_cwd().join("..").join("..").join("cockatiel_engine-rs")
}

/// The engine's self-signed TLS certificate, when present.
///
/// `COCKATIEL_TLS_CERT` (set by `cockatiel doctor` for its sandbox) wins when
/// non-empty — the test-runner already trusts that variable for its OWN
/// connection, and modules it launches must pin the same cert. Otherwise the
/// cert is looked up under the resolved engine dir.
pub fn tls_cert(cli: &Cli) -> Option<PathBuf> {
    if let Some(path) = env_dir("COCKATIEL_TLS_CERT") {
        return path.exists().then_some(path);
    }
    let path = engine_dir(cli).join("tls").join("cockatiel-cert.pem");
    path.exists().then_some(path)
}

/// A non-empty environment variable interpreted as a path.
fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The engine's `modules.json` registry, when present.
pub fn engine_modules_json(cli: &Cli) -> Option<PathBuf> {
    let path = engine_dir(cli).join("modules.json");
    path.exists().then_some(path)
}

fn legacy_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    /// Serialises the tests that mutate process-global environment variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn cli_with(install_root: Option<PathBuf>, modules_dir: Option<PathBuf>) -> Cli {
        Cli {
            suite: "all".to_string(),
            module: None,
            iterations: 100,
            duration_secs: 30,
            json: false,
            ip: "127.0.0.1".to_string(),
            port: 9734,
            pin: 0,
            install_root,
            modules_dir,
        }
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cockatiel_paths_{}_{}_{}",
            tag,
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    #[test]
    fn legacy_modules_dir_falls_back_to_cwd_parent() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("COCKATIEL_HOME");
        let cli = cli_with(None, None);
        // The modules dir is the test-runner's parent (it lives under modules/).
        let expected = std::env::current_dir().unwrap().join("..");
        assert_eq!(modules_dir(&cli), expected);
    }

    #[test]
    fn explicit_modules_dir_override_wins() {
        let root = temp_root("override_root");
        std::fs::create_dir_all(root.join("modules")).unwrap();
        let explicit = temp_root("override_modules");
        let cli = cli_with(Some(root.clone()), Some(explicit.clone()));
        assert_eq!(modules_dir(&cli), explicit);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&explicit);
    }

    #[test]
    fn install_root_layout_yields_modules_and_engine() {
        let root = temp_root("layout");
        std::fs::create_dir_all(root.join("modules")).unwrap();
        std::fs::create_dir_all(root.join("engine")).unwrap();
        let cli = cli_with(Some(root.clone()), None);
        assert_eq!(modules_dir(&cli), root.join("modules"));
        assert_eq!(engine_dir(&cli), root.join("engine"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_root_legacy_engine_name_is_recognised() {
        let root = temp_root("legacy_engine");
        std::fs::create_dir_all(root.join("modules")).unwrap();
        std::fs::create_dir_all(root.join("cockatiel_engine-rs")).unwrap();
        let cli = cli_with(Some(root.clone()), None);
        assert_eq!(engine_dir(&cli), root.join("cockatiel_engine-rs"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tls_cert_is_none_absent_and_some_when_present() {
        let root = temp_root("tls");
        std::fs::create_dir_all(root.join("engine")).unwrap();
        let cli = cli_with(Some(root.clone()), None);
        assert_eq!(tls_cert(&cli), None);
        let tls = root.join("engine").join("tls");
        std::fs::create_dir_all(&tls).unwrap();
        let cert = tls.join("cockatiel-cert.pem");
        std::fs::write(&cert, b"-----BEGIN CERTIFICATE-----").unwrap();
        assert_eq!(tls_cert(&cli), Some(cert));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn engine_dir_env_override_wins() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_root("engine_env");
        std::env::set_var("COCKATIEL_ENGINE_DIR", &dir);
        let cli = cli_with(None, None);
        assert_eq!(engine_dir(&cli), dir);
        std::env::remove_var("COCKATIEL_ENGINE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_cert_env_override_used_when_present() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_root("tls_env");
        let cert = dir.join("cockatiel-cert.pem");
        std::fs::write(&cert, b"x").unwrap();
        std::env::set_var("COCKATIEL_TLS_CERT", &cert);
        let cli = cli_with(None, None);
        assert_eq!(tls_cert(&cli), Some(cert.clone()));
        std::env::remove_var("COCKATIEL_TLS_CERT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_modules_json_is_none_absent_and_some_when_present() {
        let root = temp_root("modules_json");
        std::fs::create_dir_all(root.join("engine")).unwrap();
        let cli = cli_with(Some(root.clone()), None);
        assert_eq!(engine_modules_json(&cli), None);
        let json = root.join("engine").join("modules.json");
        std::fs::write(&json, b"[]").unwrap();
        assert_eq!(engine_modules_json(&cli), Some(json));
        let _ = std::fs::remove_dir_all(&root);
    }
}
