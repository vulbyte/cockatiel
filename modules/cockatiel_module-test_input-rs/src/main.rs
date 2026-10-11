//! Test input adapter — injects a synthetic chat message whose every field
//! (username, message, rank, score, roles, colour, platform, channel, uuid) is
//! operator-configurable, so you can watch how the rest of the pipeline reacts
//! (term-chat rendering, rank/authority/price gates, image embedding, command
//! routing, scoring, ...).
//!
//! It is a real `input` adapter: it authenticates like any other module and
//! sends `MessagePreProcess` containers carrying a `ChatMessage` with its own
//! `user_data`. The engine only overwrites that `user_data` when the user-db
//! has a record for `user_uuid7`, so with the default test UUID the adapter's
//! rank/score/roles are exactly what downstream modules see.
//!
//! Injection triggers (all configurable in `config.json` → `module_specific`):
//!   * once on connect (`send_on_start`),
//!   * every `interval_secs` seconds (`0` = off),
//!   * whenever `config.json` changes (`watch_config`) — edit a value in the
//!     TUI's config editor or by hand and the module re-injects immediately.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cockatiel_client::proto::container_for_engine::Payload as EnginePayload;
use cockatiel_client::proto::container_for_module::Payload as ModulePayload;
use cockatiel_client::proto::*;
use cockatiel_client::CockatielClient;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

mod config;
use config::TestInputConfig;

/// The connection config the client reads/writes. It also carries our settings
/// under `module_specific`, so the TUI's config editor can tune both.
const CONFIG_PATH: &str = "config.json";

/// How often the config file is checked for changes (when `watch_config`).
const WATCH_INTERVAL: Duration = Duration::from_millis(500);
/// Reconnect delay after a dropped engine connection.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsWrite = SplitSink<WsStream, WsMessage>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    loop {
        if let Err(e) = run_once().await {
            eprintln!("[test-input] {}", e);
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

    let mut cfg = TestInputConfig::load_or_default_from(CONFIG_PATH);
    eprintln!(
        "[test-input] connected as {} [{}] (rank={}, score={}, mod={}, admin={}, owner={})",
        module_name, instance_uuid7, cfg.rank, cfg.score, cfg.is_moderator, cfg.is_admin, cfg.is_owner
    );

    // Clear the engine's post-auth drain window before the first send, or it is
    // discarded as "sent before authorization".
    if cfg.post_connect_settle_ms > 0 {
        tokio::time::sleep(Duration::from_millis(cfg.post_connect_settle_ms as u64)).await;
    }
    if cfg.send_on_start {
        send_message(&mut write, &module_name, &instance_uuid7, &auth_token, &cfg).await?;
    }

    let mut tick = tokio::time::interval(WATCH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_cfg_hash = config_hash(CONFIG_PATH);
    let mut last_send = Instant::now();

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Binary(data))) => {
                        let Ok(container) = ContainerForModule::decode(data.as_ref()) else { continue };
                        // Answer the engine's liveness probe so a quiet period
                        // never severs us.
                        if let Some(ModulePayload::AuthVerify(_)) = container.payload {
                            let reply = make_container(
                                &module_name,
                                &instance_uuid7,
                                &auth_token,
                                EnginePayload::AuthVerify(AuthVerify { cur_auth: auth_token.clone() }),
                            );
                            let _ = send_raw(&mut write, &reply).await;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(format!("engine read error: {}", e)),
                    None => return Err("engine closed the connection".to_string()),
                }
            }
            _ = tick.tick() => {
                // Config-change trigger.
                if cfg.watch_config {
                    let h = config_hash(CONFIG_PATH);
                    if h != last_cfg_hash {
                        cfg = TestInputConfig::load_or_default_from(CONFIG_PATH);
                        // Re-hash AFTER the load: it backfills missing keys, which
                        // rewrites the file, and hashing before that would see the
                        // rewrite as a second change and inject twice.
                        last_cfg_hash = config_hash(CONFIG_PATH);
                        eprintln!("[test-input] config changed — re-injecting");
                        send_message(&mut write, &module_name, &instance_uuid7, &auth_token, &cfg).await?;
                        last_send = Instant::now();
                        continue;
                    }
                }
                // Interval trigger.
                if cfg.interval_secs > 0
                    && last_send.elapsed() >= Duration::from_secs(cfg.interval_secs as u64)
                {
                    send_message(&mut write, &module_name, &instance_uuid7, &auth_token, &cfg).await?;
                    last_send = Instant::now();
                }
            }
        }
    }
}

/// Build and send the configured synthetic chat message.
async fn send_message(
    write: &mut WsWrite,
    module_name: &str,
    instance_uuid7: &str,
    auth_token: &str,
    cfg: &TestInputConfig,
) -> Result<(), String> {
    let container = make_container(
        module_name,
        instance_uuid7,
        auth_token,
        EnginePayload::MessagePreProcess(MessagePreProcess {
            message_uuid7: String::new(),
            raw_message: Some(build_chat(cfg)),
            audio: Vec::new(),
            audio_type: String::new(),
        }),
    );
    send_raw(write, &container).await?;
    eprintln!(
        "[test-input] injected '{}' as {} | {} (rank={}, score={})",
        cfg.message, cfg.username, cfg.platform, cfg.rank, cfg.score
    );
    Ok(())
}

/// The `ChatMessage` the adapter injects, carrying the configured user identity,
/// rank/score styling and roles.
fn build_chat(cfg: &TestInputConfig) -> ChatMessage {
    let mut css: HashMap<String, String> = HashMap::new();
    // term-chat (and any numeric gate) reads `rank_value`; the tier NAME is
    // separate so a display never has to load the chart itself.
    css.insert("rank_value".to_string(), format!("{:.6}", cfg.rank));
    if !cfg.rank_name.is_empty() {
        css.insert("rank".to_string(), cfg.rank_name.clone());
    }
    css.insert("score".to_string(), cfg.score.to_string());
    css.insert("commendations".to_string(), cfg.commendations.to_string());
    css.insert("reprimands".to_string(), cfg.reprimands.to_string());
    if !cfg.name_color.is_empty() {
        css.insert("color".to_string(), cfg.name_color.clone());
    }

    ChatMessage {
        platform: cfg.platform.clone(),
        raw_data: Vec::new(),
        raw_message: cfg.message.clone(),
        user_uuid7: cfg.user_uuid7.clone(),
        command: None,
        user_data: Some(UserData {
            uuid: cfg.user_uuid7.clone(),
            username: cfg.username.clone(),
            is_sponsor: cfg.is_sponsor,
            is_moderator: cfg.is_moderator,
            is_admin: cfg.is_admin,
            is_owner: cfg.is_owner,
            bans: Vec::new(),
            commendations: Vec::new(),
            styling: Some(UserStylingTemplate { css_properties: css }),
            platform_ids: HashMap::new(),
        }),
        channel_id: cfg.channel_id.clone(),
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

/// A content hash of the config file, so a same-length edit is still detected.
fn config_hash(path: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let Ok(data) = std::fs::read(path) else { return 0 };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}
