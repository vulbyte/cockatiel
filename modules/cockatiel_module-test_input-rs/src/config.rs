use serde::{Deserialize, Serialize};

/// Every value the test-input adapter injects into a synthetic chat message.
///
/// Read from the module's `config.json` → `module_specific`, so the operator can
/// tune any field (via the TUI's config editor or by hand) without a rebuild.
/// Every field is defaulted; missing keys are backfilled into `config.json`.
///
/// The `rank`/`score`/role fields are attached to the message as the adapter's
/// own `user_data` (styling + roles). The engine only overwrites them when the
/// user-db has a record for `user_uuid7`; with a test UUID the adapter's values
/// are what every downstream module sees — which is the whole point of this
/// module.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TestInputConfig {
    // ── The message ──────────────────────────────────────────────────
    /// Display name shown by chat displays (term-chat's `[username | platform]`).
    pub username: String,
    /// The raw chat text to inject. A command like `!help` is parsed by the
    /// engine exactly as a real adapter message would be.
    pub message: String,
    /// Platform label (e.g. `twitch`, `kick`, `youtube`, `discord`, or anything
    /// custom). Drives the platform badge and any platform-specific routing.
    pub platform: String,
    /// Source channel id, used by the engine to target replies (e.g. `!help`)
    /// back to the "channel".
    pub channel_id: String,
    /// The user's UUID. A stable value lets repeated injections look like the
    /// same user. Keep it OUT of the user-db if you want the adapter's rank/score
    /// to be authoritative (the engine overwrites `user_data` for known users).
    pub user_uuid7: String,

    // ── The user's rank / score / styling ────────────────────────────
    /// Numeric 0-1 rank. This is what numeric gates compare (e.g. term-chat's
    /// `image_min_rank`).
    pub rank: f32,
    /// Display rank/tier NAME (e.g. `gold`, `owner`). Empty = leave it to the
    /// consumer's own chart lookup.
    pub rank_name: String,
    /// The user's score.
    pub score: i32,
    pub commendations: i32,
    pub reprimands: i32,
    /// Name colour (an ANSI colour name or `#rrggbb`), empty = default.
    pub name_color: String,

    // ── The user's roles ─────────────────────────────────────────────
    pub is_sponsor: bool,
    pub is_moderator: bool,
    pub is_admin: bool,
    pub is_owner: bool,

    // ── When to inject ───────────────────────────────────────────────
    /// Wait this long after authenticating before the first injection. The
    /// engine drains frames that arrive while authorization is still settling
    /// (its "discarded payload sent before authorization" window), so a send
    /// fired the instant the handshake returns is dropped. 300ms clears it.
    pub post_connect_settle_ms: u32,
    /// Send once on (re)connect.
    pub send_on_start: bool,
    /// Re-send every N seconds while connected. `0` = only on start + config
    /// change (the normal interactive mode).
    pub interval_secs: u32,
    /// Re-read `config.json` and re-inject whenever the file changes — so you
    /// can edit any value (in the TUI's config editor or by hand) and watch the
    /// system react without restarting the module.
    pub watch_config: bool,
}

impl Default for TestInputConfig {
    fn default() -> Self {
        Self {
            username: "TestUser".to_string(),
            message: "hello from test-input".to_string(),
            platform: "test".to_string(),
            channel_id: "test-channel".to_string(),
            // A fixed test UUID (v7-shaped). Not in the user-db, so the engine
            // keeps the adapter's rank/score instead of overwriting them.
            user_uuid7: "00000000-0000-7000-8000-0000000000aa".to_string(),

            rank: 0.5,
            rank_name: String::new(),
            score: 0,
            commendations: 0,
            reprimands: 0,
            name_color: String::new(),

            is_sponsor: false,
            is_moderator: false,
            is_admin: false,
            is_owner: false,

            post_connect_settle_ms: 300,
            send_on_start: true,
            interval_secs: 0,
            watch_config: true,
        }
    }
}

impl TestInputConfig {
    /// Load from `config.json` → `module_specific`, backfilling any missing keys
    /// back into the file so every setting is always present and editable.
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
            // A bare settings file (no connection wrapper) still works.
            cfg = serde_json::from_value(root.clone()).unwrap_or(cfg);
        }

        // Backfill: write the fully-defaulted struct back under `module_specific`,
        // preserving the connection fields (ip/port/pin/module_name/...).
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

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ckt-testinput-{}-{}", tag, uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    #[test]
    fn missing_file_yields_defaults_and_backfills() {
        let path = tmp("missing");
        let cfg = TestInputConfig::load_or_default_from(&path);
        assert_eq!(cfg.username, "TestUser");
        assert_eq!(cfg.rank, 0.5);
        assert!(cfg.send_on_start);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["module_specific"]["username"], "TestUser");
        assert_eq!(written["module_specific"]["rank"], 0.5);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn preserves_connection_fields_and_overrides() {
        let path = tmp("preserve");
        std::fs::write(
            &path,
            r#"{"ip":"127.0.0.1","port":9734,"module_name":"test-input","module_specific":{"username":"Bob","rank":0.9}}"#,
        )
        .unwrap();
        let cfg = TestInputConfig::load_or_default_from(&path);
        assert_eq!(cfg.username, "Bob");
        assert_eq!(cfg.rank, 0.9);
        // A missing field is defaulted...
        assert_eq!(cfg.message, "hello from test-input");
        // ...and the connection fields survive the backfill.
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["ip"], "127.0.0.1");
        assert_eq!(written["module_name"], "test-input");
        assert_eq!(written["module_specific"]["username"], "Bob");
        assert_eq!(written["module_specific"]["message"], "hello from test-input");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
