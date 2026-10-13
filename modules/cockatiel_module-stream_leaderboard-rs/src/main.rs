//! Stream leaderboard — ranks the chatters of the current stream by the points
//! they have earned *this stream*.
//!
//! It is a `output` (post-process) module, so it sees every chat message after
//! the engine has enriched it: the engine's `enrich_chat_user` resolves (or
//! creates) the user-db record and writes the user's live total `score` into
//! `user_data.styling.css_properties["score"]`.
//!
//! Per stream, for each user the module records the score it first saw
//! (the baseline) and reports `earned = current - baseline`. A new stream is
//! detected by polling the engine's `channel_viewers` surface and resetting the
//! baselines on an offline→live transition. The ranking is pushed to the engine
//! via the `leaderboard_report` query; the TUI reads it back via
//! `stream_leaderboard`. The engine only caches and echoes the report — it never
//! computes the ranking itself.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use cockatiel_client::proto::container_for_engine::Payload as EnginePayload;
use cockatiel_client::proto::container_for_module::Payload as ModulePayload;
use cockatiel_client::proto::*;
use cockatiel_client::CockatielClient;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tracing::info;

mod config;
use config::LeaderboardConfig;

/// The connection config the client reads/writes. It also carries our settings
/// under `module_specific`.
const CONFIG_PATH: &str = "config.json";
/// Reconnect delay after a dropped engine connection.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsWrite = SplitSink<WsStream, WsMessage>;

/// The per-stream ranking state, updated by the message loop and read by the
/// report timer. Guarded by a plain `Mutex` (held only for short map ops).
#[derive(Default)]
struct Leaderboard {
    /// uuid -> the score first seen for that user this stream.
    baseline: HashMap<String, i32>,
    /// uuid -> (username, latest score).
    latest: HashMap<String, (String, i32)>,
    /// Whether a stream was live at the last poll (for edge detection).
    live: bool,
}

/// Record one chat message: remember the user's first-seen score (baseline) and
/// their latest score/name.
fn observe(state: &Mutex<Leaderboard>, chat: &ChatMessage) {
    let Some(ud) = chat.user_data.as_ref() else { return };
    if ud.uuid.is_empty() {
        return;
    }
    let Some(score) = ud
        .styling
        .as_ref()
        .and_then(|s| s.css_properties.get("score"))
        .and_then(|v| v.parse::<i32>().ok())
    else {
        return;
    };
    let mut st = state.lock().unwrap();
    st.baseline.entry(ud.uuid.clone()).or_insert(score);
    st.latest.insert(ud.uuid.clone(), (ud.username.clone(), score));
}

/// Apply a liveness reading. A false→true edge means a new stream began, so the
/// per-user baselines (and the previous stream's names) are cleared and the next
/// message from each user starts a fresh count.
fn set_live(state: &Mutex<Leaderboard>, live: bool) {
    let mut st = state.lock().unwrap();
    if live && !st.live {
        st.baseline.clear();
        st.latest.clear();
    }
    st.live = live;
}

/// Build the report JSON: every user seen this stream, ranked by points earned,
/// highest first (ties broken by name). `limit == 0` means "no limit".
fn build_report(st: &Leaderboard, limit: u32) -> serde_json::Value {
    let mut rows: Vec<(String, String, i32, i32)> = st
        .baseline
        .iter()
        .map(|(uuid, base)| {
            let (name, score) = st
                .latest
                .get(uuid)
                .cloned()
                .unwrap_or_else(|| (String::new(), *base));
            (uuid.clone(), name, score - base, score)
        })
        .collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    let take = if limit == 0 { rows.len() } else { limit as usize };
    let entries: Vec<serde_json::Value> = rows
        .into_iter()
        .take(take)
        .map(|(uuid, username, earned, score)| {
            serde_json::json!({
                "uuid": uuid,
                "username": username,
                "earned": earned,
                "score": score,
            })
        })
        .collect();
    serde_json::json!({ "entries": entries })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    loop {
        if let Err(e) = run_once().await {
            eprintln!("[stream-leaderboard] {}", e);
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn run_once() -> Result<(), String> {
    let client = CockatielClient::connect(CONFIG_PATH).await?;
    let module_name = client.config.module_name.clone();
    let auth_token = client.auth_token.clone();
    let instance_uuid7 = client.instance_uuid7.clone();
    let (mut write, mut read) = client.stream.split();

    let cfg = LeaderboardConfig::load_or_default_from(CONFIG_PATH);
    let state = Mutex::new(Leaderboard::default());
    let filter = if cfg.platform.is_empty() {
        "{}".to_string()
    } else {
        serde_json::json!({ "platform": cfg.platform }).to_string()
    };
    eprintln!(
        "[stream-leaderboard] connected as {} [{}] (report={}s poll={}s limit={})",
        module_name, instance_uuid7, cfg.report_secs, cfg.poll_secs, cfg.limit
    );

    // ONE connection only. A second socket under the same module name re-binds
    // the engine's session to it, and the post-process stage then waits forever
    // for an ack that this read loop can never send. The liveness poll and the
    // report therefore share the main socket via `select!`.
    let mut report_tick = tokio::time::interval(Duration::from_secs(cfg.report_secs.max(1) as u64));
    report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut poll_tick = tokio::time::interval(Duration::from_secs(cfg.poll_secs.max(1) as u64));
    poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Binary(data))) => {
                        let Ok(container) = ContainerForModule::decode(data.as_ref()) else { continue };
                        match container.payload {
                            Some(ModulePayload::AuthVerify(_)) => {
                                let reply = make_container(
                                    &module_name,
                                    &instance_uuid7,
                                    &auth_token,
                                    EnginePayload::AuthVerify(AuthVerify { cur_auth: auth_token.clone() }),
                                );
                                let _ = send_raw(&mut write, &reply).await;
                            }
                            Some(ModulePayload::MessagePostProcess(pp)) => {
                                if let Some(chat) = pp.raw_message.as_ref() {
                                    observe(&state, chat);
                                }
                                // Echo the stage back so the engine completes
                                // the post-process stage for this message. The
                                // receipt ping alone only stops resends; the
                                // stage-echo (same message_uuid7) is what clears
                                // the pending entry.
                                if !pp.message_uuid7.is_empty() {
                                    let echo = make_container(
                                        &module_name,
                                        &instance_uuid7,
                                        &auth_token,
                                        EnginePayload::MessagePostProcess(MessagePostProcess {
                                            message_uuid7: pp.message_uuid7.clone(),
                                            raw_message: pp.raw_message.clone(),
                                            processed_message: pp.processed_message.clone(),
                                            audio: pp.audio.clone(),
                                            audio_type: pp.audio_type.clone(),
                                        }),
                                    );
                                    let _ = send_raw(&mut write, &echo).await;
                                    // Receipt ping: stop the engine resending.
                                    let ack = make_container(
                                        &module_name,
                                        &instance_uuid7,
                                        &auth_token,
                                        EnginePayload::MessageAck(MessageAck {
                                            message_uuid7: pp.message_uuid7.clone(),
                                        }),
                                    );
                                    let _ = send_raw(&mut write, &ack).await;
                                }
                            }
                            Some(ModulePayload::DatabaseQueryResult(res))
                                if res.query_id == "channel_viewers" =>
                            {
                                if let Ok(json) =
                                    serde_json::from_slice::<serde_json::Value>(&res.result_blob)
                                {
                                    let any_live = json
                                        .get("channels")
                                        .and_then(|c| c.as_array())
                                        .map(|chs| {
                                            chs.iter().any(|ch| {
                                                ch.get("is_live")
                                                    .and_then(|v| v.as_bool())
                                                    .unwrap_or(false)
                                            })
                                        })
                                        .unwrap_or(false);
                                    set_live(&state, any_live);
                                }
                            }
                            Some(ModulePayload::Shutdown(_)) => {
                                info!("Engine requested shutdown — exiting");
                                std::process::exit(0);
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(format!("engine read error: {}", e)),
                    None => return Err("engine closed the connection".to_string()),
                }
            }
            _ = report_tick.tick() => {
                let report = {
                    let st = state.lock().unwrap();
                    build_report(&st, cfg.limit)
                };
                let query = make_container(
                    &module_name,
                    &instance_uuid7,
                    &auth_token,
                    EnginePayload::DatabaseQuery(DatabaseQuery {
                        query_id: "leaderboard_report".to_string(),
                        sql: report.to_string(),
                        params: vec![],
                    }),
                );
                let _ = send_raw(&mut write, &query).await;
            }
            _ = poll_tick.tick() => {
                let query = make_container(
                    &module_name,
                    &instance_uuid7,
                    &auth_token,
                    EnginePayload::DatabaseQuery(DatabaseQuery {
                        query_id: "channel_viewers".to_string(),
                        sql: filter.clone(),
                        params: vec![],
                    }),
                );
                let _ = send_raw(&mut write, &query).await;
            }
        }
    }
}

fn make_container(
    module_name: &str,
    instance_uuid7: &str,
    auth_token: &str,
    payload: EnginePayload,
) -> ContainerForEngine {
    ContainerForEngine {
        version: 2,
        auth_token: auth_token.to_string(),
        module_name: module_name.to_string(),
        module_instance_uuid7: instance_uuid7.to_string(),
        payload: Some(payload),
    }
}

async fn send_raw(write: &mut WsWrite, container: &ContainerForEngine) -> Result<(), String> {
    let mut buf = Vec::new();
    container.encode(&mut buf).map_err(|e| format!("encode: {}", e))?;
    write
        .send(WsMessage::Binary(buf))
        .await
        .map_err(|e| format!("send: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(uuid: &str, name: &str, score: i32) -> ChatMessage {
        let mut css = HashMap::new();
        css.insert("score".to_string(), score.to_string());
        ChatMessage {
            platform: "twitch".to_string(),
            raw_data: Vec::new(),
            raw_message: "hi".to_string(),
            user_uuid7: uuid.to_string(),
            command: None,
            user_data: Some(UserData {
                uuid: uuid.to_string(),
                username: name.to_string(),
                is_sponsor: false,
                is_moderator: false,
                is_admin: false,
                is_owner: false,
                bans: Vec::new(),
                commendations: Vec::new(),
                styling: Some(UserStylingTemplate { css_properties: css }),
                platform_ids: HashMap::new(),
            }),
            channel_id: "ch".to_string(),
        }
    }

    #[test]
    fn earned_is_delta_from_first_seen_score() {
        let state = Mutex::new(Leaderboard::default());
        observe(&state, &chat("u1", "Alice", 100));
        observe(&state, &chat("u1", "Alice", 130));
        let report = build_report(&state.lock().unwrap(), 10);
        let e = &report["entries"][0];
        assert_eq!(e["username"], "Alice");
        assert_eq!(e["earned"], 30);
        assert_eq!(e["score"], 130);
    }

    #[test]
    fn ranks_by_earned_descending() {
        let state = Mutex::new(Leaderboard::default());
        observe(&state, &chat("u1", "Alice", 0));
        observe(&state, &chat("u1", "Alice", 5));
        observe(&state, &chat("u2", "Bob", 50));
        observe(&state, &chat("u2", "Bob", 90));
        let report = build_report(&state.lock().unwrap(), 10);
        assert_eq!(report["entries"][0]["username"], "Bob");
        assert_eq!(report["entries"][0]["earned"], 40);
        assert_eq!(report["entries"][1]["username"], "Alice");
        assert_eq!(report["entries"][1]["earned"], 5);
    }

    #[test]
    fn limit_caps_the_entries() {
        let state = Mutex::new(Leaderboard::default());
        for i in 0..5 {
            observe(&state, &chat(&format!("u{i}"), &format!("U{i}"), 0));
        }
        let report = build_report(&state.lock().unwrap(), 2);
        assert_eq!(report["entries"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_new_stream_resets_the_baselines() {
        let state = Mutex::new(Leaderboard::default());
        observe(&state, &chat("u1", "Alice", 100));
        observe(&state, &chat("u1", "Alice", 150));
        // Live edge: new stream — the previous stream's counts are gone.
        set_live(&state, true);
        let report = build_report(&state.lock().unwrap(), 10);
        assert_eq!(report["entries"].as_array().unwrap().len(), 0);
        // The next message starts a fresh baseline.
        observe(&state, &chat("u1", "Alice", 150));
        observe(&state, &chat("u1", "Alice", 160));
        let report = build_report(&state.lock().unwrap(), 10);
        assert_eq!(report["entries"][0]["earned"], 10);
    }

    #[test]
    fn staying_live_does_not_reset() {
        let state = Mutex::new(Leaderboard::default());
        set_live(&state, true);
        observe(&state, &chat("u1", "Alice", 100));
        observe(&state, &chat("u1", "Alice", 120));
        set_live(&state, true); // still live — no edge
        let report = build_report(&state.lock().unwrap(), 10);
        assert_eq!(report["entries"][0]["earned"], 20);
    }

    #[test]
    fn messages_without_score_are_ignored() {
        let state = Mutex::new(Leaderboard::default());
        let mut c = chat("u1", "Alice", 0);
        c.user_data.as_mut().unwrap().styling = None;
        observe(&state, &c);
        assert_eq!(build_report(&state.lock().unwrap(), 10)["entries"].as_array().unwrap().len(), 0);
    }
}
