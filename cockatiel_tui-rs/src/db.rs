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
    /// Rolling average processing time (ms) for the module's last 8 messages,
    /// reported by the engine. `None` until it has completed a message.
    pub avg_ms: Option<f64>,
    /// Whether the module is set to start automatically (from its manifest).
    pub autostart: bool,
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
    /// Lifetime score earned (never reduced by spending).
    pub total_score: i64,
    /// Chat messages this user has sent.
    pub messages_sent: i64,
    /// The user's current numeric rank (computed server-side by the user db).
    pub rank: i64,
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
    /// The operator removed the engine from this TUI (`Action::RemoveEngine`).
    ///
    /// NOT the same as `connected: false`, and it has to be a separate fact: a
    /// disconnected engine is coming back (the client reconnects on its own
    /// backoff), whereas a removed one is not — the client was told to stop and
    /// the TUI is running standalone. Without this flag the modules window
    /// would go on drawing a connection that no longer exists, and the row's
    /// actions would still be advertised.
    pub engine_removed: bool,
    pub module_entries: Vec<ModuleStatus>,
    pub connection: ConnectionInfo,
    /// Whether a backup DB is configured for the timeline and the user DB.
    pub timeline_backup: bool,
    pub userdb_backup: bool,
    /// Whether the engine's dispatch gate is holding the message backlog
    /// (`db_status`'s `pipeline_paused`, plus the `pipeline_set_paused`
    /// response). The engine boots paused, so this is the state that decides
    /// whether anything is being dispatched at all.
    ///
    /// Defaults to `true` because the engine's own default is to boot paused.
    /// Assuming the opposite is the one failure direction that matters here:
    /// before the first `db_status` poll lands (up to 2s after connect) a
    /// `false` default would hide the PAUSED badge, and the operator's first
    /// `p` press would then be derived from the wrong belief and send
    /// `{"paused": true}` — a no-op against an already-paused engine, so the
    /// press that is supposed to start the system does nothing and looks
    /// broken. A `true` default can only over-report for that same 2s, and it
    /// over-reports the state that is actually true.
    pub pipeline_paused: bool,
    /// Redraw tick driving the PAUSED indicator's blink phase (see
    /// [`crate::app::pause_flash_on`]). Advanced by the draw loop, not by the
    /// engine: it lives in stats because that is the only per-frame channel
    /// every window's `render` already receives.
    pub pause_flash_tick: u64,
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
            engine_removed: false,
            module_entries: Vec::new(),
            connection: ConnectionInfo::default(),
            timeline_backup: false,
            userdb_backup: false,
            pipeline_paused: true,
            pause_flash_tick: 0,
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

impl GlobalStats {
    /// Forget everything the TUI learned from the ENGINE: its connection
    /// details, its status, the pause state, and every statistic it reported —
    /// the database sizes, the module list, the platform counts, the message
    /// totals, the chart, and the user-database cache (which only ever arrives
    /// through the engine's query surface).
    ///
    /// TUI-side only, deliberately. The engine's own `config.json` / `.env` are
    /// the operator's settings and are NOT touched: the config editor still
    /// points at that directory, and the operator may well re-attach or
    /// relaunch. "Remove the engine" means this TUI stops pretending it has one.
    ///
    /// `pause_flash_tick` is carried across because it is the DRAW LOOP's clock
    /// and not the engine's — resetting it would restart the PAUSED blink's
    /// phase from a row that is no longer on screen.
    ///
    /// What is deliberately NOT cleared is `AppState::module_runs`: those are
    /// the supervisor's own process statuses, and the modules they describe are
    /// still running under the TUI. Dropping them would make a locally-launched
    /// module that dies while the engine is gone crash SILENTLY (the monitor
    /// reads that map to decide whether a death was a crash).
    pub fn forget_engine(&mut self) {
        let blink = self.pause_flash_tick;
        *self = GlobalStats {
            engine_removed: true,
            // Back to the safe default rather than to whatever the last poll
            // said. With no engine we know nothing, `paused` defaults to true
            // for exactly that reason (see the field doc), and the badge is
            // already hidden by `connected == false` — so this can only
            // over-report the state a fresh engine would boot into.
            pipeline_paused: true,
            pause_flash_tick: blink,
            ..Default::default()
        };
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
            // A keyless blob keeps the belief we already hold rather than
            // resetting it. The two backup flags are safe to default to false
            // because a false reading only withholds a warning, but flipping
            // the pause flag to false on absent data would hide the PAUSED
            // badge and re-break the first `p` press — the dangerous direction
            // again. The engine always sends the key, so this only guards a
            // shape change.
            stats.pipeline_paused = v
                .get("pipeline_paused")
                .and_then(|b| b.as_bool())
                .unwrap_or(stats.pipeline_paused);
        }
        return;
    }
    // The pause toggle's own response. Adopt the engine's reported state rather
    // than assuming the toggle took: the gate is idempotent, so a re-toggle
    // answers `changed: false` and leaves the state where it was. A failure
    // (e.g. the control-surface denial) is left to the 2s db_status poll to
    // correct, so the UI can never drift from the engine for longer than a poll.
    if query_id == "pipeline_set_paused" {
        if result.success {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&result.result_blob) {
                stats.pipeline_paused = v
                    .get("paused")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(stats.pipeline_paused);
            }
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
                        let avg_ms = row.get("avg_ms").and_then(|v| v.as_f64());
                        let autostart = row.get("autostart").and_then(|v| v.as_bool()).unwrap_or(false);

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
                            avg_ms,
                            autostart,
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
        ("db_status", "SELECT 1".to_string()),  // virtual query: timeline/userdb backup status + pipeline_paused
        ("userdb_list_users", r#"{"limit":500}"#.to_string()),  // virtual query: user DB list (score DESC)
    ]
}

/// A one-line, honest summary of a `pipeline_set_paused` response for the log
/// window. `held_messages` / `resumed_messages` are backlog SIZES — the release
/// after a resume is detached and keeps running in the background — so neither
/// is ever reported as work completed.
pub fn pipeline_pause_note(result: &DatabaseQueryResult) -> Option<String> {
    if !result.success {
        return Some(if result.error.is_empty() {
            "pipeline pause toggle failed (no error detail)".to_string()
        } else {
            format!("pipeline pause toggle failed: {}", result.error)
        });
    }
    let v: serde_json::Value = serde_json::from_slice(&result.result_blob).ok()?;
    let paused = v.get("paused").and_then(|b| b.as_bool()).unwrap_or(false);
    if v.get("changed").and_then(|b| b.as_bool()).unwrap_or(false) {
        if paused {
            let held = v.get("held_messages").and_then(|n| n.as_u64()).unwrap_or(0);
            Some(format!("pipeline PAUSED — holding {} queued message(s)", held))
        } else {
            let released = v.get("resumed_messages").and_then(|n| n.as_u64()).unwrap_or(0);
            Some(format!(
                "pipeline RESUMED — releasing {} held message(s) in the background",
                released
            ))
        }
    } else {
        // Idempotent re-toggle: the gate was already in the requested state.
        Some(if paused { "pipeline already paused" } else { "pipeline already running" }.to_string())
    }
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
        total_score: v.get("total_score").and_then(|x| x.as_i64()).unwrap_or(0),
        messages_sent: v.get("messages_sent").and_then(|x| x.as_i64()).unwrap_or(0),
        rank: v.get("rank").and_then(|x| x.as_i64()).unwrap_or(0),
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

    fn db_status_result(blob: &str) -> DatabaseQueryResult {
        DatabaseQueryResult {
            query_id: "db_status".to_string(),
            success: true,
            error: String::new(),
            result_blob: blob.as_bytes().to_vec(),
        }
    }

    #[test]
    fn db_status_carries_the_pipeline_pause_gate() {
        let mut stats = GlobalStats::default();
        // Defaults to PAUSED, because that is the engine's own boot default.
        // The failure this guards against is the operator pressing `p` to start
        // the system during the first 2s poll window, having been shown no
        // badge, and having the press derived from a false "running" belief.
        assert!(
            stats.pipeline_paused,
            "the UI must start believing what the engine does at boot, so the \
             first `p` press is derived from the truth"
        );

        update_stats_from_query(&mut stats, "db_status", &db_status_result(r#"{"pipeline_paused":true}"#));
        assert!(stats.pipeline_paused, "paused=true must land");

        // The 2s poll is the authority, so a running engine must clear a stale
        // local pause (and vice versa) rather than only ever setting it.
        update_stats_from_query(&mut stats, "db_status", &db_status_result(r#"{"pipeline_paused":false}"#));
        assert!(!stats.pipeline_paused, "paused=false must clear a local pause");

        // Alongside the other db_status flags, not instead of them.
        update_stats_from_query(
            &mut stats,
            "db_status",
            &db_status_result(r#"{"timeline_backup":true,"userdb_backup":false,"pipeline_paused":true}"#),
        );
        assert!(stats.timeline_backup && !stats.userdb_backup && stats.pipeline_paused);
    }

    #[test]
    fn db_status_without_the_field_keeps_the_last_known_pause_state() {
        let mut stats = GlobalStats { pipeline_paused: true, ..Default::default() };
        // An older engine (or a partial blob) has no such key. Holding the last
        // known state is the safe direction: the alternative is clearing the
        // badge and deriving the operator's first `p` press from "running",
        // which sends a no-op against an engine that is actually paused.
        update_stats_from_query(&mut stats, "db_status", &db_status_result(r#"{"timeline_backup":true}"#));
        assert!(stats.pipeline_paused, "a keyless blob must not clear a known pause");
        // A non-bool of the right name is unusable, so it also holds.
        update_stats_from_query(&mut stats, "db_status", &db_status_result(r#"{"pipeline_paused":"yes"}"#));
        assert!(stats.pipeline_paused, "a malformed field must not clear a known pause");
        // An explicit false is real data, and must win over the held value.
        update_stats_from_query(&mut stats, "db_status", &db_status_result(r#"{"pipeline_paused":false}"#));
        assert!(!stats.pipeline_paused, "an explicit false is authoritative and must clear it");
    }

    #[test]
    fn a_malformed_db_status_blob_does_not_panic() {
        let mut stats = GlobalStats { pipeline_paused: true, ..Default::default() };
        // Unparseable blob: the whole read is skipped, so the last known state
        // stands (the next 2s poll carries the real one).
        for blob in ["", "not json", r#"{"pipeline_paused":"#] {
            update_stats_from_query(&mut stats, "db_status", &db_status_result(blob));
            assert!(stats.pipeline_paused, "unparseable db_status ({:?}) must not clear the state", blob);
        }
        // Parseable but carrying no usable flag: same reasoning, hold the state.
        // (Note this differs from the two backup flags beside it, which default
        // to false — a false reading there only withholds a warning, whereas a
        // false here hides the badge that the operator needs.)
        for blob in ["[]", "null", r#"{"pipeline_paused":"yes"}"#] {
            update_stats_from_query(&mut stats, "db_status", &db_status_result(blob));
            assert!(stats.pipeline_paused, "db_status ({:?}) with no usable flag must hold the state", blob);
        }
        // A failed db_status is skipped entirely, same as before.
        let mut failed = db_status_result(r#"{"pipeline_paused":true}"#);
        failed.success = false;
        update_stats_from_query(&mut stats, "db_status", &failed);
        assert!(stats.pipeline_paused);
    }

    fn pause_result(blob: &str, success: bool, error: &str) -> DatabaseQueryResult {
        DatabaseQueryResult {
            query_id: "pipeline_set_paused".to_string(),
            success,
            error: error.to_string(),
            result_blob: if success { blob.as_bytes().to_vec() } else { Vec::new() },
        }
    }

    #[test]
    fn the_pause_toggle_response_sets_the_state_the_engine_reports() {
        let mut stats = GlobalStats::default();
        update_stats_from_query(
            &mut stats,
            "pipeline_set_paused",
            &pause_result(r#"{"paused":true,"changed":true,"held_messages":3}"#, true, ""),
        );
        assert!(stats.pipeline_paused);

        update_stats_from_query(
            &mut stats,
            "pipeline_set_paused",
            &pause_result(r#"{"paused":false,"changed":true,"resumed_messages":3,"errors":[]}"#, true, ""),
        );
        assert!(!stats.pipeline_paused, "the resume response must clear the pause immediately");

        // Idempotent re-toggle: the engine answers with the state it is in, so
        // adopting the response is what keeps a double press from lying.
        update_stats_from_query(
            &mut stats,
            "pipeline_set_paused",
            &pause_result(r#"{"paused":false,"changed":false,"resumed_messages":0,"errors":[]}"#, true, ""),
        );
        assert!(!stats.pipeline_paused);
    }

    #[test]
    fn a_denied_pause_toggle_leaves_the_state_to_the_poll() {
        let mut stats = GlobalStats { pipeline_paused: true, ..Default::default() };
        update_stats_from_query(
            &mut stats,
            "pipeline_set_paused",
            &pause_result("", false, "pipeline_set_paused denied: not the TUI"),
        );
        assert!(stats.pipeline_paused, "a denial is not evidence the gate moved");
    }

    #[test]
    fn pause_notes_report_backlog_sizes_not_completed_work() {
        let note = |r: &DatabaseQueryResult| pipeline_pause_note(r).unwrap();

        assert!(note(&pause_result(r#"{"paused":true,"changed":true,"held_messages":42}"#, true, ""))
            .contains("42"));
        assert_eq!(
            note(&pause_result(r#"{"paused":true,"changed":true,"held_messages":42}"#, true, "")),
            "pipeline PAUSED — holding 42 queued message(s)"
        );
        // The release is detached: the number is what is being released, so the
        // wording must never read as "processed".
        let resumed = note(&pause_result(
            r#"{"paused":false,"changed":true,"resumed_messages":7,"errors":["x"]}"#,
            true,
            "",
        ));
        assert!(resumed.contains('7') && resumed.contains("releasing"), "{:?}", resumed);
        for banned in ["processed", "completed", "done"] {
            assert!(!resumed.to_lowercase().contains(banned), "{:?} must not claim work was {}", resumed, banned);
        }
        assert_eq!(
            note(&pause_result(r#"{"paused":true,"changed":false,"held_messages":0}"#, true, "")),
            "pipeline already paused"
        );
        assert_eq!(
            note(&pause_result(r#"{"paused":false,"changed":false,"resumed_messages":0}"#, true, "")),
            "pipeline already running"
        );
        assert_eq!(
            note(&pause_result("", false, "pipeline_set_paused denied: not the TUI")),
            "pipeline pause toggle failed: pipeline_set_paused denied: not the TUI"
        );
        // A success we cannot read is reported as nothing rather than guessed.
        assert!(pipeline_pause_note(&pause_result("garbage", true, "")).is_none());
    }

    /// Removing the engine from the TUI has to leave NOTHING of it on screen:
    /// not its connection details, not a status string that reads like a live
    /// link, not the pause state of a gate that no longer exists, and none of
    /// the numbers it was the only source of.
    #[test]
    fn forgetting_the_engine_clears_exactly_what_it_had_told_us() {
        let mut stats = GlobalStats {
            total_messages: 42,
            total_users: 7,
            total_commands: 9,
            platform_counts: [("twitch".to_string(), 5u64)].into_iter().collect(),
            platform_errors: [("twitch".to_string(), 2u64)].into_iter().collect(),
            db_size_mb: 12.5,
            db_target_mb: 99,
            engine_status: "connected".to_string(),
            module_entries: vec![ModuleStatus {
                name: "m0".into(),
                description: String::new(),
                status: "connected".into(),
                position: "preprocess".into(),
                credentials: Vec::new(),
                directory: String::new(),
                credential_values: Default::default(),
                config_complete: true,
                alive: true,
                last_seen: 1,
                avg_ms: None,
                autostart: false,
            }],
            connection: ConnectionInfo { ip: "10.0.0.1".into(), port: 9734, pin: 123456 },
            pipeline_paused: false,
            pause_flash_tick: 77,
            ..Default::default()
        };
        assert!(!stats.engine_removed);

        stats.forget_engine();

        // The removal is itself the new state — the one fact that must survive.
        assert!(stats.engine_removed);
        // Connection info: an engine the TUI forgot has no address to show.
        assert_eq!(stats.connection.ip, "");
        assert_eq!(stats.connection.port, 0);
        assert_eq!(stats.connection.pin, 0);
        // Not a live-looking status, and the gate is back to the safe default
        // rather than frozen at whatever the last poll said.
        assert_ne!(stats.engine_status, "connected");
        assert!(stats.pipeline_paused, "a forgotten engine leaves no belief about the gate");
        // Every engine-reported statistic.
        assert!(stats.module_entries.is_empty());
        assert!(stats.platform_counts.is_empty());
        assert!(stats.platform_errors.is_empty());
        assert_eq!(stats.db_size_mb, 0.0);
        assert_eq!(stats.total_messages, 0);
        assert_eq!(stats.total_users, 0);
        assert_eq!(stats.total_commands, 0);
        assert!(stats.chart_data.is_empty());
        assert!(!stats.timeline_backup && !stats.userdb_backup);
        // The user-database cache only ever arrives through the engine, so it
        // goes with it rather than sitting in the users window as a stale list.
        assert!(stats.users.is_empty());
        assert!(stats.user_detail.is_none());
        assert!(stats.user_values.is_empty());
        // ...but the draw loop's own clock is NOT the engine's to reset: that
        // would rewind the PAUSED blink's phase.
        assert_eq!(stats.pause_flash_tick, 77);
        // A default-built struct must not claim to have been removed.
        assert!(!GlobalStats::default().engine_removed);
    }
}
