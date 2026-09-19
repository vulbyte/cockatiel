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
}

#[derive(Debug, Clone, Default)]
pub struct ConnectionInfo {
    pub ip: String,
    pub port: u16,
    pub pin: u32,
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

                        stats.module_entries.push(ModuleStatus {
                            name: name.to_string(),
                            description,
                            status,
                            position: position.to_string(),
                            credentials,
                            directory,
                            credential_values,
                            config_complete,
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
    ]
}
