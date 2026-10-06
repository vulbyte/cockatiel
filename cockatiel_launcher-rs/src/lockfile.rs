use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Lock {
    #[allow(dead_code)]
    pub lock_version: u32,
    pub components: BTreeMap<String, LockedComponent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LockedComponent {
    #[allow(dead_code)]
    pub path: String,
    #[serde(default)]
    pub sha: Option<String>,
    pub version: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub repo: Option<String>,
    #[allow(dead_code)]
    pub kind: String,
}

impl Lock {
    pub fn load(path: &Path) -> Result<Lock, String> {
        let data = std::fs::read_to_string(path)
            .map_err(|e| format!("read {}: {}", path.display(), e))?;
        serde_json::from_str(&data).map_err(|e| format!("parse {}: {}", path.display(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"{
        "lock_version": 1,
        "components": {
            "engine":      { "path": "cockatiel_engine-rs", "sha": "bd120bf", "version": "0.1.0", "repo": "vulbyte/cockatiel_engine-rs", "kind": "engine" },
            "tui":         { "path": "cockatiel_tui_v2-rs", "sha": "a7a0017", "version": "0.1.0", "repo": "vulbyte/cockatiel_tui_v2-rs", "kind": "tui" },
            "user-db":     { "path": "cockatiel_user_database-rs", "sha": null, "version": "0.1.0", "repo": "vulbyte/cockatiel", "kind": "user-db" },
            "test-runner": { "path": "cockatiel_test_runner-rs", "sha": null, "version": "0.1.0", "repo": "vulbyte/cockatiel", "kind": "test-runner" },
            "tts-rs":      { "path": "modules/cockatiel_module-tts-rs", "sha": "1ab25e8", "version": "0.1.0", "repo": "vulbyte/cockatiel_module-tts-rs", "kind": "module" }
        }
    }"#;

    #[test]
    fn parses_full_lock() {
        let lock: Lock = serde_json::from_str(LOCK).unwrap();
        assert_eq!(lock.lock_version, 1);
        assert_eq!(lock.components.len(), 5);

        let engine = &lock.components["engine"];
        assert_eq!(engine.path, "cockatiel_engine-rs");
        assert_eq!(engine.sha.as_deref(), Some("bd120bf"));
        assert_eq!(engine.version, "0.1.0");
        assert_eq!(engine.repo.as_deref(), Some("vulbyte/cockatiel_engine-rs"));
        assert_eq!(engine.kind, "engine");

        let user_db = &lock.components["user-db"];
        assert_eq!(user_db.sha, None);

        let module = &lock.components["tts-rs"];
        assert_eq!(module.path, "modules/cockatiel_module-tts-rs");
        assert_eq!(module.kind, "module");
    }

    #[test]
    fn load_reads_from_disk() {
        let path = std::env::temp_dir().join("cockatiel_launcher_lock_test.json");
        std::fs::write(&path, LOCK).unwrap();
        let lock = Lock::load(&path).unwrap();
        assert_eq!(lock.lock_version, 1);
        assert!(lock.components.contains_key("tui"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_reports_missing_file() {
        let path = std::path::Path::new("/nonexistent/cockatiel.lock");
        let err = Lock::load(path).unwrap_err();
        assert!(err.contains("read"));
    }
}
