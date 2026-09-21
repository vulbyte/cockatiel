use std::collections::HashMap;
use cockatiel_client::proto::*;
use cockatiel_client::proto::container::Payload;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CredentialField {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    #[allow(dead_code)]
    pub list: bool,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone)]
pub struct ModuleStatus {
    pub name: String,
    pub description: String,
    pub status: String,
    pub position: String,
    pub credentials: Vec<CredentialField>,
    #[allow(dead_code)]
    pub directory: String,
    pub credential_values: HashMap<String, String>,
    #[allow(dead_code)]
    pub config_complete: bool,
    /// Engine-reported liveness (false once the probe window expired).
    pub alive: bool,
    /// Engine-reported last activity (ms epoch).
    #[allow(dead_code)]
    pub last_seen: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ConnectionInfo {
    pub ip: String,
    pub port: u16,
    pub pin: u32,
}

#[derive(Debug, Clone, Default)]
pub struct UserSummary {
    pub uuid7: String,
    pub username: String,
    pub is_sponsor: bool,
    pub is_moderator: bool,
    pub is_admin: bool,
    pub is_owner: bool,
    pub score: i64,
    pub commendations: i64,
    pub reprimands: i64,
    /// Channels as "platform:channel_id (handle)" display strings.
    pub channels: Vec<String>,
    pub flags: String,
    #[allow(dead_code)]
    pub created_at: i64,
    #[allow(dead_code)]
    pub updated_at: i64,
}

impl UserSummary {
    pub fn rank_tier(&self) -> &'static str {
        rank_tier(self.score)
    }
}

/// Rank tier derived from score: opal>=50, gold>=20, silver>=5, coal<=-5,
/// trash<=-20. Order matters (trash is checked before coal since -20<=-5).
pub fn rank_tier(score: i64) -> &'static str {
    if score >= 50 {
        "opal"
    } else if score >= 20 {
        "gold"
    } else if score >= 5 {
        "silver"
    } else if score <= -20 {
        "trash"
    } else if score <= -5 {
        "coal"
    } else {
        "neutral"
    }
}

/// A single per-user key/value from the user DB (ban, timeout, name_color,
/// notes, ...). Stored as plain strings; the window renders known JSON shapes.
#[derive(Debug, Clone, Default)]
pub struct UserValue {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct GlobalStats {
    pub total_messages: u64,
    pub total_users: u64,
    pub total_commands: u64,
    #[allow(dead_code)]
    pub mod_actions: u64,
    pub platform_counts: HashMap<String, u64>,
    pub platform_errors: HashMap<String, u64>,
    pub chart_data: Vec<TimeBucket>,
    pub db_size_mb: f64,
    pub db_target_mb: u64,
    pub engine_status: String,
    pub module_entries: Vec<ModuleStatus>,
    pub connection: ConnectionInfo,
    /// Whether a backup DB is configured for the timeline and the user DB.
    pub timeline_backup: bool,
    pub userdb_backup: bool,
    /// The user database, polled via `userdb_list_users` and rendered by the
    /// detached users window. Sorted by the engine (score DESC).
    pub users: Vec<UserSummary>,
    /// The most recently fetched user detail (`userdb_get_user`).
    pub user_detail: Option<UserSummary>,
    /// uuid7 that `user_detail` belongs to.
    pub user_detail_for: Option<String>,
    /// Monotonic counter bumped on every `userdb_get_user` response, so the
    /// users window can tell a fresh detail from a stale one.
    pub user_detail_epoch: u64,
    /// Per-user values (`userdb_list_user_values`), e.g. ban/timeout/notes.
    pub user_values: Vec<UserValue>,
    /// Monotonic counter bumped on every `userdb_list_user_values` response
    /// (and on a successful `userdb_write_user_value`).
    pub user_values_epoch: u64,
    /// The last userdb error surfaced to the window (query failures, denials).
    pub user_last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TimeBucket {
    pub timestamp: u64,
    pub counts: HashMap<String, u64>,
    #[allow(dead_code)]
    pub errors: u64,
}

impl Default for GlobalStats {
    fn default() -> Self {
        Self {
            total_messages: 0,
            total_users: 0,
            total_commands: 0,
            mod_actions: 0,
            platform_counts: HashMap::new(),
            platform_errors: HashMap::new(),
            chart_data: Vec::new(),
            db_size_mb: 0.0,
            db_target_mb: 50,
            engine_status: "disconnected".to_string(),
            module_entries: Vec::new(),
            connection: ConnectionInfo::default(),
            timeline_backup: false,
            userdb_backup: false,
            users: Vec::new(),
            user_detail: None,
            user_detail_for: None,
            user_detail_epoch: 0,
            user_values: Vec::new(),
            user_values_epoch: 0,
            user_last_error: None,
        }
    }
}

#[allow(dead_code)]
pub fn make_query(sql: &str, query_id: &str) -> Container {
    Container {
        version: 1,
        auth_token: String::new(),
        module_name: "cockatiel-tui".into(),
        module_instance_uuid7: String::new(),
        payload: Some(Payload::DatabaseQuery(DatabaseQuery {
            query_id: query_id.to_string(),
            sql: sql.to_string(),
            params: Vec::new(),
        })),
    }
}

pub fn parse_query_result(result: &DatabaseQueryResult) -> Option<Vec<HashMap<String, serde_json::Value>>> {
    if !result.success {
        return None;
    }
    let blob = String::from_utf8_lossy(&result.result_blob);
    serde_json::from_str(&blob).ok()
}

pub fn update_stats_from_query(stats: &mut GlobalStats, query_id: &str, result: &DatabaseQueryResult) {
    // db_status is a JSON object (not a row array), so parse it directly.
    if query_id == "db_status" && result.success {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&result.result_blob) {
            stats.timeline_backup = v.get("timeline_backup").and_then(|b| b.as_bool()).unwrap_or(false);
            stats.userdb_backup = v.get("userdb_backup").and_then(|b| b.as_bool()).unwrap_or(false);
        }
        return;
    }
    // userdb_* responses carry the engine's JSON envelope
    // {success, error, user, users, message, value, values} — not SQL rows.
    if query_id.starts_with("userdb_") {
        if !result.success {
            stats.user_last_error = if result.error.is_empty() {
                Some(format!("userdb {} failed", query_id))
            } else {
                Some(result.error.clone())
            };
            return;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&result.result_blob) {
            let success = v.get("success").and_then(|b| b.as_bool()).unwrap_or(false);
            if !success {
                stats.user_last_error = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| Some(format!("userdb {} failed", query_id)));
            } else {
                stats.user_last_error = None;
                match query_id {
                    "userdb_list_users" => {
                        stats.users = v
                            .get("users")
                            .and_then(|a| a.as_array())
                            .map(|arr| arr.iter().filter_map(parse_user).collect())
                            .unwrap_or_default();
                    }
                    "userdb_list_user_values" => {
                        stats.user_values = v
                            .get("values")
                            .and_then(|a| a.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|x| {
                                        let key = x.get("key").and_then(|k| k.as_str())?.to_string();
                                        let value = x.get("value").and_then(|k| k.as_str()).unwrap_or("").to_string();
                                        Some(UserValue { key, value })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        stats.user_values_epoch = stats.user_values_epoch.wrapping_add(1);
                    }
                    "userdb_write_user_value" => {
                        // Upsert the written value (e.g. notes) into the local view.
                        if let Some(val) = v.get("value").and_then(|x| x.as_object()) {
                            if let (Some(k), Some(value)) = (
                                val.get("key").and_then(|k| k.as_str()),
                                val.get("value").and_then(|k| k.as_str()),
                            ) {
                                if let Some(existing) = stats.user_values.iter_mut().find(|uv| uv.key == k) {
                                    existing.value = value.to_string();
                                } else {
                                    stats.user_values.push(UserValue {
                                        key: k.to_string(),
                                        value: value.to_string(),
                                    });
                                }
                                stats.user_values_epoch = stats.user_values_epoch.wrapping_add(1);
                            }
                        }
                    }
                    _ => {}
                }
                // Any userdb response that returns the updated user object
                // (score/roles/flags mutations, get_user) refreshes the view.
                if let Some(u) = v.get("user").and_then(parse_user) {
                    apply_user(stats, u);
                    stats.user_detail_epoch = stats.user_detail_epoch.wrapping_add(1);
                }
            }
        }
        return;
    }
    if let Some(rows) = parse_query_result(result) {
        match query_id {
            "total_messages" => {
                if let Some(row) = rows.first() {
                    stats.total_messages = row.get("COUNT(*)").or_else(|| row.get("count")).and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }
            "total_users" => {
                if let Some(row) = rows.first() {
                    stats.total_users = row.get("COUNT(DISTINCT user_uuid7)").or_else(|| row.get("count")).and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }
            "total_commands" => {
                if let Some(row) = rows.first() {
                    stats.total_commands = row.get("COUNT(*)").or_else(|| row.get("count")).and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }
            "platform_counts" => {
                stats.platform_counts.clear();
                for row in &rows {
                    if let (Some(platform), Some(count)) = (
                        row.get("platform").and_then(|v| v.as_str()),
                        row.get("COUNT(*)").or_else(|| row.get("count")).and_then(|v| v.as_u64()),
                    ) {
                        stats.platform_counts.insert(platform.to_string(), count);
                    }
                }
            }
            "platform_errors" => {
                stats.platform_errors.clear();
                for row in &rows {
                    if let (Some(platform), Some(count)) = (
                        row.get("platform").and_then(|v| v.as_str()),
                        row.get("COUNT(*)").or_else(|| row.get("count")).and_then(|v| v.as_u64()),
                    ) {
                        stats.platform_errors.insert(platform.to_string(), count);
                    }
                }
            }
            "chart_data" => {
                stats.chart_data.clear();
                let mut buckets: HashMap<i64, TimeBucket> = HashMap::new();
                for row in &rows {
                    if let (Some(bucket_ts), Some(platform), Some(count)) = (
                        row.get("bucket").and_then(|v| v.as_i64()),
                        row.get("platform").and_then(|v| v.as_str()),
                        row.get("COUNT(*)").or_else(|| row.get("count")).and_then(|v| v.as_u64()),
                    ) {
                        let entry = buckets.entry(bucket_ts).or_insert_with(|| TimeBucket {
                            timestamp: bucket_ts as u64,
                            counts: HashMap::new(),
                            errors: 0,
                        });
                        entry.counts.insert(platform.to_string(), count);
                    }
                }
                let mut chart_data: Vec<TimeBucket> = buckets.into_values().collect();
                chart_data.sort_by_key(|b| b.timestamp);
                stats.chart_data = chart_data;
            }
            "module_list" => {
                stats.module_entries.clear();
                for row in &rows {
                    if let Some(name) = row.get("name").and_then(|v| v.as_str()) {
                        let description = row.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let connected_at = row.get("connected_at").and_then(|v| v.as_i64());
                        let shutdown_at = row.get("shutdown_at").and_then(|v| v.as_i64());
                        let position = row.get("position").and_then(|v| v.as_str()).unwrap_or("unknown");

                        // A session is: offline if it never connected, connected if
                        // it connected and has NOT cleanly shut down (the engine sets
                        // shutdown_at on every disconnect), and disconnected otherwise.
                        // NOTE: do NOT treat a long-lived connection as "crashed" —
                        // a live session has connected_at but no shutdown_at.
                        let status = match (connected_at, shutdown_at) {
                            (None, _) => "offline".to_string(),
                            (Some(_), Some(_)) => "disconnected".to_string(),
                            (Some(_), None) => "connected".to_string(),
                        };

                        let credentials: Vec<CredentialField> = row
                            .get("credentials")
                            .and_then(|v| serde_json::from_value::<Vec<CredentialField>>(v.clone()).ok())
                            .unwrap_or_default();
                        let directory = row.get("directory").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let credential_values: HashMap<String, String> = row
                            .get("credential_values")
                            .and_then(|v| v.as_object())
                            .map(|obj| {
                                obj.iter()
                                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let config_complete = row.get("config_complete").and_then(|v| v.as_bool()).unwrap_or(false);
                        let alive = row.get("alive").and_then(|v| v.as_bool()).unwrap_or(true);
                        let last_seen = row.get("last_seen").and_then(|v| v.as_i64()).unwrap_or(0);

                        stats.module_entries.push(ModuleStatus {
                            name: name.to_string(),
                            description,
                            status,
                            position: position.to_string(),
                            credentials,
                            directory,
                            credential_values,
                            config_complete,
                            alive,
                            last_seen,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn get_pending_queries() -> Vec<(&'static str, String)> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let five_min_ago = now_ms - (5 * 60 * 1000);
    let bucket_ms = 10_000i64;

    vec![
        ("total_messages", "SELECT COUNT(*) FROM timeline_events WHERE pipeline_status != 'audit'".to_string()),
        ("total_users", "SELECT COUNT(DISTINCT user_uuid7) FROM timeline_events WHERE user_uuid7 != '' AND user_uuid7 IS NOT NULL AND pipeline_status != 'audit'".to_string()),
        ("total_commands", "SELECT COUNT(*) FROM timeline_events WHERE command != '' AND command IS NOT NULL AND pipeline_status != 'audit'".to_string()),
        ("platform_counts", "SELECT platform, COUNT(*) FROM timeline_events WHERE pipeline_status != 'audit' GROUP BY platform".to_string()),
        ("platform_errors", "SELECT platform, COUNT(*) FROM timeline_events WHERE pipeline_status = 'failed' GROUP BY platform".to_string()),
        ("chart_data", format!(
            "SELECT (persisted_at / {bucket}) * {bucket} AS bucket, platform, COUNT(*) FROM timeline_events WHERE persisted_at > {since} AND pipeline_status != 'audit' GROUP BY bucket, platform ORDER BY bucket ASC",
            bucket = bucket_ms,
            since = five_min_ago,
        )),
        ("module_list", "SELECT 1".to_string()),  // virtual query, engine returns module list
        ("db_status", "SELECT 1".to_string()),  // virtual query: timeline/userdb backup status
        ("userdb_list_users", r#"{"limit":500}"#.to_string()),  // virtual query: user DB list (score DESC)
    ]
}

/// Parse a user object from the engine's `{success, user, users, ...}` envelope.
fn parse_user(v: &serde_json::Value) -> Option<UserSummary> {
    let uuid7 = v.get("uuid7").and_then(|x| x.as_str())?.to_string();
    let channels = v
        .get("channels")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let platform = c.get("platform").and_then(|x| x.as_str()).unwrap_or("");
                    let channel_id = c.get("channel_id").and_then(|x| x.as_str()).unwrap_or("");
                    let handle = c.get("handle").and_then(|x| x.as_str()).unwrap_or("");
                    if platform.is_empty() && channel_id.is_empty() && handle.is_empty() {
                        None
                    } else {
                        Some(format!("{}:{} ({})", platform, channel_id, handle))
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Some(UserSummary {
        uuid7,
        username: v.get("username").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        is_sponsor: v.get("is_sponsor").and_then(|x| x.as_bool()).unwrap_or(false),
        is_moderator: v.get("is_moderator").and_then(|x| x.as_bool()).unwrap_or(false),
        is_admin: v.get("is_admin").and_then(|x| x.as_bool()).unwrap_or(false),
        is_owner: v.get("is_owner").and_then(|x| x.as_bool()).unwrap_or(false),
        score: v.get("score").and_then(|x| x.as_i64()).unwrap_or(0),
        commendations: v.get("commendations").and_then(|x| x.as_i64()).unwrap_or(0),
        reprimands: v.get("reprimands").and_then(|x| x.as_i64()).unwrap_or(0),
        channels,
        flags: v.get("flags").and_then(|x| x.as_str()).unwrap_or("{}").to_string(),
        created_at: v.get("created_at").and_then(|x| x.as_i64()).unwrap_or(0),
        updated_at: v.get("updated_at").and_then(|x| x.as_i64()).unwrap_or(0),
    })
}

/// Merge a fresh user object into the list + detail view.
fn apply_user(stats: &mut GlobalStats, user: UserSummary) {
    if let Some(existing) = stats.users.iter_mut().find(|x| x.uuid7 == user.uuid7) {
        *existing = user.clone();
    }
    stats.user_detail_for = Some(user.uuid7.clone());
    stats.user_detail = Some(user);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn userdb_result(query_id: &str, success: bool, error: &str, envelope: &serde_json::Value) -> DatabaseQueryResult {
        DatabaseQueryResult {
            query_id: query_id.to_string(),
            success,
            error: error.to_string(),
            result_blob: if success { envelope.to_string().into_bytes() } else { Vec::new() },
        }
    }

    #[test]
    fn parses_userdb_list_users_into_stats() {
        let mut stats = GlobalStats::default();
        let envelope = serde_json::json!({
            "success": true,
            "users": [
                {
                    "uuid7": "u1", "username": "alice",
                    "is_sponsor": true, "is_moderator": false, "is_admin": true, "is_owner": false,
                    "score": 60, "commendations": 10, "reprimands": 2,
                    "channels": [{"platform": "twitch", "channel_id": "c1", "handle": "alice"}],
                    "flags": "{}", "created_at": 1, "updated_at": 2,
                },
                {
                    "uuid7": "u2", "username": "bob",
                    "is_sponsor": false, "is_moderator": false, "is_admin": false, "is_owner": false,
                    "score": -20, "commendations": 0, "reprimands": 5,
                    "channels": [], "flags": "{}", "created_at": 1, "updated_at": 2,
                }
            ]
        });
        let result = userdb_result("userdb_list_users", true, "", &envelope);
        update_stats_from_query(&mut stats, "userdb_list_users", &result);

        assert_eq!(stats.users.len(), 2);
        assert_eq!(stats.users[0].username, "alice");
        assert_eq!(stats.users[0].score, 60);
        assert!(stats.users[0].is_sponsor);
        assert_eq!(stats.users[0].channels.len(), 1);
        assert_eq!(stats.users[0].rank_tier(), "opal");
        assert_eq!(stats.users[1].rank_tier(), "trash");
        assert!(stats.user_last_error.is_none());
    }

    #[test]
    fn parses_userdb_get_user_and_values() {
        let mut stats = GlobalStats::default();
        let detail = userdb_result(
            "userdb_get_user",
            true,
            "",
            &serde_json::json!({
                "success": true,
                "user": {
                    "uuid7": "u1", "username": "alice",
                    "is_sponsor": false, "is_moderator": false, "is_admin": false, "is_owner": false,
                    "score": 12, "commendations": 3, "reprimands": 1,
                    "channels": [], "flags": "{}", "created_at": 1, "updated_at": 2,
                }
            }),
        );
        update_stats_from_query(&mut stats, "userdb_get_user", &detail);
        assert_eq!(stats.user_detail.as_ref().unwrap().username, "alice");
        assert_eq!(stats.user_detail_for.as_deref(), Some("u1"));
        let epoch = stats.user_detail_epoch;
        assert!(epoch > 0);

        let values = userdb_result(
            "userdb_list_user_values",
            true,
            "",
            &serde_json::json!({
                "success": true,
                "values": [
                    {"key": "notes", "value": "hello"},
                    {"key": "name_color", "value": "#ff00aa"},
                ]
            }),
        );
        update_stats_from_query(&mut stats, "userdb_list_user_values", &values);
        assert_eq!(stats.user_values.len(), 2);
        assert_eq!(stats.user_values[0].key, "notes");
        assert!(stats.user_values_epoch > 0);
    }

    #[test]
    fn surfaces_userdb_errors() {
        let mut stats = GlobalStats::default();
        let fail = userdb_result("userdb_get_user", false, "User not found", &serde_json::json!({}));
        update_stats_from_query(&mut stats, "userdb_get_user", &fail);
        assert_eq!(stats.user_last_error.as_deref(), Some("User not found"));
    }

    #[test]
    fn writes_upsert_values_locally() {
        let mut stats = GlobalStats::default();
        stats.user_values = vec![UserValue { key: "notes".into(), value: "old".into() }];
        let written = userdb_result(
            "userdb_write_user_value",
            true,
            "",
            &serde_json::json!({
                "success": true,
                "value": {"key": "notes", "value": "new"}
            }),
        );
        update_stats_from_query(&mut stats, "userdb_write_user_value", &written);
        assert_eq!(stats.user_values[0].value, "new");
    }

    #[test]
    fn pending_queries_include_userdb_list() {
        let qs = get_pending_queries();
        let userdb = qs.iter().find(|(id, _)| *id == "userdb_list_users").expect("userdb poll");
        let payload: serde_json::Value = serde_json::from_str(&userdb.1).unwrap();
        assert_eq!(payload["limit"], 500);
    }
}
