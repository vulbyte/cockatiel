use serde::{Deserialize, Serialize};

/// Settings for the stream leaderboard module, read from `config.json` →
/// `module_specific`. Every field is defaulted; missing keys are backfilled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LeaderboardConfig {
    /// How often the ranking is pushed to the engine (seconds).
    pub report_secs: u32,
    /// How often `channel_viewers` is polled to detect a new stream (seconds).
    pub poll_secs: u32,
    /// Maximum number of chatters in a report.
    pub limit: u32,
    /// Optional platform filter for the liveness poll (e.g. `twitch`). Empty =
    /// any platform going live resets the leaderboard.
    pub platform: String,
}

impl Default for LeaderboardConfig {
    fn default() -> Self {
        Self {
            report_secs: 2,
            poll_secs: 5,
            limit: 10,
            platform: String::new(),
        }
    }
}

impl LeaderboardConfig {
    /// Load from `config.json` → `module_specific`, backfilling missing keys so
    /// every setting is present and editable in the TUI's config editor.
    pub fn load_or_default_from<P: AsRef<std::path::Path>>(path: P) -> Self {
        let path = path.as_ref();
        let mut cfg = Self::default();
        let mut root: serde_json::Value = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}));

        if let Some(ms) = root.get("module_specific") {
            cfg = serde_json::from_value(ms.clone()).unwrap_or(cfg);
        } else if root.is_object() && !root.as_object().unwrap().is_empty() {
            cfg = serde_json::from_value(root.clone()).unwrap_or(cfg);
        }

        if !root.is_object() {
            root = serde_json::json!({});
        }
        root["module_specific"] = serde_json::to_value(&cfg).unwrap_or(serde_json::Value::Null);
        if let Ok(pretty) = serde_json::to_string_pretty(&root) {
            let _ = std::fs::write(path, pretty);
        }
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults_and_backfills() {
        let dir = std::env::temp_dir().join(format!("ckt-leaderboard-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let cfg = LeaderboardConfig::load_or_default_from(&path);
        assert_eq!(cfg.report_secs, 2);
        assert_eq!(cfg.poll_secs, 5);
        assert_eq!(cfg.limit, 10);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["module_specific"]["report_secs"], 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overrides_are_read_and_connection_fields_survive() {
        let dir = std::env::temp_dir().join(format!("ckt-leaderboard-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"ip":"127.0.0.1","module_name":"stream-leaderboard","module_specific":{"limit":25,"platform":"twitch"}}"#,
        )
        .unwrap();
        let cfg = LeaderboardConfig::load_or_default_from(&path);
        assert_eq!(cfg.limit, 25);
        assert_eq!(cfg.platform, "twitch");
        assert_eq!(cfg.report_secs, 2);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["ip"], "127.0.0.1");
        assert_eq!(written["module_name"], "stream-leaderboard");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
