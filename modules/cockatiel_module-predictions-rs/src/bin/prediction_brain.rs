//! prediction_brain — the chat-command half of the predictions module.
//!
//! Registers `!pred`, owns the one in-memory prediction, answers the actor's
//! score via `prediction_get_score` (correlating each DatabaseQueryResult back
//! to the pending bet), applies parimutuel payouts / refunds through
//! `userdb_adjust_score`, and broadcasts a `PredictionUpdate` bar after every
//! state change so every connected module (the display window) re-renders.
//! Modeled on cockatiel_module-commend-rs: split the stream, spawn a read loop
//! that answers AuthVerify, surfaces DatabaseQueryResult failures, acks every
//! pre/in-process stage, and reconnects with backoff — re-registering the
//! command on each fresh session. All pure logic lives in `logic`.

use futures_util::{SinkExt, StreamExt};
use prost::Message;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tracing::{info, warn};
use tracing_subscriber::FmtSubscriber;

use cockatiel_client::{
    proto::container::Payload,
    proto::{prediction_update, *},
    CockatielClient,
};
use cockatiel_prediction::logic::{
    self, ParsedCommand, Poll, PollCommand, PollStatus, Prediction, Side, Status, UserInfo,
};

type WsWriteHalf = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    WsMessage,
>;

const COMMAND_NAME: &str = "pred";
const POLL_COMMAND_NAME: &str = "poll";
const DEFAULT_MODULE_NAME: &str = "predictions";
const DEFAULT_FLAG: &str = "!";
const DEFAULT_CREATOR_ROLE: &str = "mod";
const DEFAULT_POLL_CREATOR_ROLE: &str = "mod";
const DEFAULT_BET_MIN: i64 = 1;
const DEFAULT_BET_MAX: i64 = 0; // 0 = unlimited
const DEFAULT_DISPLAY_BIN: &str = "target/release/prediction_display";
const DEFAULT_DISPLAY_CONFIG: &str = "prediction_display.json";
const DEFAULT_RECONNECT_BASE_SECS: u64 = 1;
const DEFAULT_RECONNECT_MAX_SECS: u64 = 30;
const DEFAULT_HOLD_AFTER_RESOLVE_SECS: u64 = 30;

/// Module config convention (like commend's `ensure_defaults`): settings live
/// in config.json's `module_specific` and are created (with defaults) when
/// missing. The connection keys are named `engine_ip`/`engine_port` here and
/// are mirrored into the top-level `ip`/`port` the client loader reads.
#[derive(Debug, Clone)]
struct ModuleSettings {
    engine_ip: String,
    engine_port: u16,
    command_flag: String,
    creator_role: String,
    poll_creator_role: String,
    bet_min: i64,
    bet_max: i64,
    display_bin: String,
    display_config: String,
    reconnect_base_secs: u64,
    reconnect_max_secs: u64,
    hold_after_resolve_secs: u64,
}

fn ensure_defaults() -> ModuleSettings {
    let root: Option<serde_json::Value> = std::fs::read_to_string("config.json")
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok());
    let mut root = root.unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }

    let top_ip = root
        .get("ip")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let top_port = root
        .get("port")
        .and_then(|v| v.as_u64())
        .unwrap_or(9734) as u16;

    // Read module_specific (immutable snapshot), then write everything back.
    let ms_read = root
        .get("module_specific")
        .and_then(|m| m.as_object())
        .cloned()
        .unwrap_or_default();

    let engine_ip = ms_read
        .get("engine_ip")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or(top_ip);
    let engine_port = ms_read
        .get("engine_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(top_port as u64) as u16;
    let command_flag = ms_read
        .get("command_flag")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| DEFAULT_FLAG.to_string());
    let creator_role = ms_read
        .get("creator_role")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| DEFAULT_CREATOR_ROLE.to_string());
    let poll_creator_role = ms_read
        .get("poll_creator_role")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| DEFAULT_POLL_CREATOR_ROLE.to_string());
    let bet_min = ms_read
        .get("bet_min")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_BET_MIN);
    let bet_max = ms_read
        .get("bet_max")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_BET_MAX);
    let display_bin = ms_read
        .get("display_bin")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| DEFAULT_DISPLAY_BIN.to_string());
    let display_config = ms_read
        .get("display_config")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| DEFAULT_DISPLAY_CONFIG.to_string());
    let reconnect_base_secs = ms_read
        .get("reconnect_base_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_RECONNECT_BASE_SECS);
    let reconnect_max_secs = ms_read
        .get("reconnect_max_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_RECONNECT_MAX_SECS);
    let hold_after_resolve_secs = ms_read
        .get("hold_after_resolve_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_HOLD_AFTER_RESOLVE_SECS);

    if let Some(obj) = root.as_object_mut() {
        // Connection settings the client loader reads (top-level), kept in sync
        // with the `engine_ip`/`engine_port` keys above.
        obj.entry("ip".to_string()).or_insert_with(|| serde_json::json!(engine_ip));
        obj.entry("port".to_string()).or_insert_with(|| serde_json::json!(engine_port));
        obj.entry("module_name".to_string())
            .or_insert_with(|| serde_json::json!(DEFAULT_MODULE_NAME));
        obj.entry("position".to_string()).or_insert_with(|| serde_json::json!(1));
        obj.entry("priority".to_string()).or_insert_with(|| serde_json::json!(100));
        let ms = obj
            .entry("module_specific".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(ms) = ms.as_object_mut() {
            ms.entry("engine_ip".to_string()).or_insert_with(|| serde_json::json!(engine_ip));
            ms.entry("engine_port".to_string()).or_insert_with(|| serde_json::json!(engine_port));
            ms.entry("command_flag".to_string())
                .or_insert_with(|| serde_json::json!(command_flag));
            ms.entry("creator_role".to_string())
                .or_insert_with(|| serde_json::json!(creator_role));
            ms.entry("poll_creator_role".to_string())
                .or_insert_with(|| serde_json::json!(poll_creator_role));
            ms.entry("bet_min".to_string()).or_insert_with(|| serde_json::json!(bet_min));
            ms.entry("bet_max".to_string()).or_insert_with(|| serde_json::json!(bet_max));
            ms.entry("display_bin".to_string()).or_insert_with(|| serde_json::json!(display_bin));
            ms.entry("display_config".to_string())
                .or_insert_with(|| serde_json::json!(display_config));
            ms.entry("reconnect_base_secs".to_string())
                .or_insert_with(|| serde_json::json!(reconnect_base_secs));
            ms.entry("reconnect_max_secs".to_string())
                .or_insert_with(|| serde_json::json!(reconnect_max_secs));
            ms.entry("hold_after_resolve_secs".to_string())
                .or_insert_with(|| serde_json::json!(hold_after_resolve_secs));
        }
        let _ = std::fs::write("config.json", serde_json::to_string_pretty(&root).unwrap());
    }

    ModuleSettings {
        engine_ip,
        engine_port,
        command_flag,
        creator_role,
        poll_creator_role,
        bet_min,
        bet_max,
        display_bin,
        display_config,
        reconnect_base_secs,
        reconnect_max_secs,
        hold_after_resolve_secs,
    }
}

/// Session identity (auth token + module ids) shared between the read loop and
/// the query sender so a reconnect's fresh credentials are picked up by both.
#[derive(Clone)]
struct EngineIdentity {
    auth: String,
    instance: String,
    module: String,
}

/// Encode and send a Container on the shared write half.
async fn send_container(write_shared: &Arc<AsyncMutex<WsWriteHalf>>, container: Container) {
    let mut buf = Vec::new();
    if container.encode(&mut buf).is_ok() {
        let mut w = write_shared.lock().await;
        let _ = w.send(WsMessage::Binary(buf)).await;
    }
}

/// Register the chat command (with its parsed flags) with the engine. Called on
/// every fresh session — the engine forgets a session's commands when the
/// socket drops.
async fn register_commands(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    command_flag: &str,
) {
    let id = identity.lock().await.clone();
    let commands = Container {
        version: 1,
        auth_token: id.auth,
        module_name: id.module,
        module_instance_uuid7: id.instance,
        payload: Some(Payload::CommandsPayload(Commands {
            commands: vec![Command {
                command_name: COMMAND_NAME.to_string(),
                command_flag: command_flag.to_string(),
                command_description: "start/stop a zero-sum prediction and bet score (parimutuel)"
                    .to_string(),
                command_flags: vec![
                    Flag {
                        flag_name: "l".to_string(),
                        flag_description: "bet <amount> on the left (or -l <label> on start)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                    Flag {
                        flag_name: "r".to_string(),
                        flag_description: "bet <amount> on the right (or -r <label> on start)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                    Flag {
                        flag_name: "w".to_string(),
                        flag_description: "stop -w l|r resolves to that side (absent = refund)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                ],
            },
            Command {
                command_name: POLL_COMMAND_NAME.to_string(),
                command_flag: command_flag.to_string(),
                command_description: "create/close a free-vote poll and vote (!poll -1..-6)"
                    .to_string(),
                command_flags: vec![
                    Flag {
                        flag_name: "p".to_string(),
                        flag_description: "create: the poll prompt (-p <prompt>)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                    Flag {
                        flag_name: "1".to_string(),
                        flag_description: "option 1 (label on create, vote on bare -1)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                    Flag {
                        flag_name: "2".to_string(),
                        flag_description: "option 2 (label on create, vote on bare -2)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                    Flag {
                        flag_name: "3".to_string(),
                        flag_description: "option 3 (label on create, vote on bare -3)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                    Flag {
                        flag_name: "4".to_string(),
                        flag_description: "option 4 (label on create, vote on bare -4)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                    Flag {
                        flag_name: "5".to_string(),
                        flag_description: "option 5 (label on create, vote on bare -5)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                    Flag {
                        flag_name: "6".to_string(),
                        flag_description: "option 6 (label on create, vote on bare -6)"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                    Flag {
                        flag_name: "h".to_string(),
                        flag_description: "create: hide live counts while the poll is open"
                            .to_string(),
                        limiting_type: 0,
                        min_val: 0.0,
                        max_val: 0.0,
                        options: vec![],
                        value: String::new(),
                    },
                ],
            },
        ],
            alert_on_unknown_command: false,
        })),
    };
    send_container(write_shared, commands).await;
    info!("registered !{} and !{} commands", COMMAND_NAME, POLL_COMMAND_NAME);
}

/// Send a DatabaseQuery (json sql) to the engine.
async fn send_query(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    query_id: &str,
    sql: String,
) {
    let id = identity.lock().await.clone();
    let query = Container {
        version: 1,
        auth_token: id.auth,
        module_name: id.module,
        module_instance_uuid7: id.instance,
        payload: Some(Payload::DatabaseQuery(DatabaseQuery {
            query_id: query_id.to_string(),
            sql,
            params: vec![],
        })),
    };
    send_container(write_shared, query).await;
}

/// Project the brain's prediction state onto the wire PredictionUpdate shape.
fn update_for(pred: &Prediction) -> PredictionUpdate {
    let (status, winner_side) = match pred.status {
        Status::Open => (prediction_update::Status::Open as i32, String::new()),
        Status::Resolved => (
            prediction_update::Status::Resolved as i32,
            match pred.winner {
                Some(Side::Left) => "left".to_string(),
                Some(Side::Right) => "right".to_string(),
                None => String::new(),
            },
        ),
        Status::Cancelled => (prediction_update::Status::Cancelled as i32, String::new()),
    };
    PredictionUpdate {
        prediction_id: pred.id.clone(),
        prompt: pred.prompt.clone(),
        side_left_label: pred.left_label.clone(),
        side_right_label: pred.right_label.clone(),
        side_left_total: pred.side_total(Side::Left),
        side_right_total: pred.side_total(Side::Right),
        pot: pred.pot(),
        status,
        winner_side,
    }
}

/// Broadcast a PredictionUpdate after every state change. The engine relays it
/// to every other connected module (the display window renders it).
async fn broadcast_prediction(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    pred: &Prediction,
) {
    let id = identity.lock().await.clone();
    let container = Container {
        version: 1,
        auth_token: id.auth,
        module_name: id.module,
        module_instance_uuid7: id.instance,
        payload: Some(Payload::PredictionUpdate(update_for(pred))),
    };
    send_container(write_shared, container).await;
}

/// Project the brain's poll state onto the wire PollUpdate shape.
fn poll_update_for(poll: &Poll) -> PollUpdate {
    let status = match poll.status {
        PollStatus::Open => poll_update::Status::Open as i32,
        PollStatus::Closed => poll_update::Status::Closed as i32,
    };
    PollUpdate {
        poll_id: poll.id.clone(),
        prompt: poll.prompt.clone(),
        options: poll.options.clone(),
        vote_counts: poll.votes.clone(),
        total_votes: poll.total(),
        status,
        winner_index: poll.winner_index,
        hide_counts: poll.hide_counts,
    }
}

/// Broadcast a PollUpdate after every poll state change. The engine relays it
/// to every other connected module (the display window renders it).
async fn broadcast_poll(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    poll: &Poll,
) {
    let id = identity.lock().await.clone();
    let container = Container {
        version: 1,
        auth_token: id.auth,
        module_name: id.module,
        module_instance_uuid7: id.instance,
        payload: Some(Payload::PollUpdate(poll_update_for(poll))),
    };
    send_container(write_shared, container).await;
}

/// Send a confirmation to the chat via SendToPlatforms. The engine verifies
/// the actor (uuid7 lookup) is a verified moderator/admin/owner — so this is
/// only used for start/stop confirmations (the actor is role-gated), never for
/// bets (a non-mod actor would be denied).
async fn send_to_chat(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    chat: &ChatMessage,
    msg: &str,
) {
    let Some(ud) = &chat.user_data else { return };
    let id = identity.lock().await.clone();
    let send = SendToPlatforms {
        msg: msg.to_string(),
        level: 0, // PlatformSendLevel::Unspecified — routing is by platform string
        module_uuid7: id.instance.clone(),
        pid: String::new(),
        platform: chat.platform.clone(),
        actor_platform: chat.platform.clone(),
        actor_handle: ud.username.clone(),
        actor_uuid7: chat.user_uuid7.clone(),
        channel_id: chat.channel_id.clone(),
    };
    let container = Container {
        version: 1,
        auth_token: id.auth,
        module_name: id.module,
        module_instance_uuid7: id.instance,
        payload: Some(Payload::SendToPlatforms(send)),
    };
    send_container(write_shared, container).await;
}

/// The chat actor's platform role (projected from UserData).
fn user_info(chat: &ChatMessage) -> Option<UserInfo> {
    chat.user_data.as_ref().map(|u| UserInfo {
        is_sponsor: u.is_sponsor,
        is_moderator: u.is_moderator,
        is_admin: u.is_admin,
        is_owner: u.is_owner,
    })
}

/// Role gate for start/stop: the actor must meet `creator_role`.
fn role_ok(chat: &ChatMessage, creator_role: &str) -> bool {
    logic::role_gate(user_info(chat).as_ref(), creator_role)
}

/// Best-effort username for logs.
fn handle(chat: &ChatMessage) -> &str {
    chat.user_data
        .as_ref()
        .map(|u| u.username.as_str())
        .unwrap_or("<unknown>")
}

/// A bet awaiting its score lookup. Exactly one can be in flight at a time:
/// the read loop stashes it before sending `prediction_get_score`, and the
/// correlated DatabaseQueryResult completes it (a second bet while one is
/// pending is rejected).
struct PendingBet {
    user_uuid7: String,
    handle: String,
    side: Side,
    amount: i64,
}

// 8 args: the async orchestration touches the socket halves, identity, config,
// the chat message, the parsed command and the shared prediction state. A
// context struct would hide the read-loop's dataflow, not simplify it.
#[allow(clippy::too_many_arguments)]
async fn handle_start(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    settings: &ModuleSettings,
    chat: &ChatMessage,
    prompt: String,
    left_label: Option<String>,
    right_label: Option<String>,
    active: &mut Option<Prediction>,
) {
    if !role_ok(chat, &settings.creator_role) {
        info!("[predictions] '{}' denied start (creator_role '{}')", handle(chat), settings.creator_role);
        return;
    }
    if active.is_some() {
        info!("[predictions] '{}' tried to start a prediction but one is already open", handle(chat));
        return;
    }
    if prompt.trim().is_empty() {
        info!("[predictions] '{}' start rejected: empty prompt", handle(chat));
        return;
    }
    let (ll, rl) = logic::start_labels(left_label.as_deref(), right_label.as_deref());
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    let pred = Prediction::new(logic::new_uuid7(), prompt, ll, rl, created_at);
    *active = Some(pred);
    broadcast_prediction(write_shared, identity, active.as_ref().unwrap()).await;
    let a = active.as_ref().unwrap();
    send_to_chat(
        write_shared,
        identity,
        chat,
        &format!(
            "Prediction started: {} ({} vs {}). Bet with !pred -l <amt> or -r <amt>.",
            a.prompt, a.left_label, a.right_label
        ),
    )
    .await;
    info!(
        "[predictions] '{}' started '{}' ({} vs {})",
        handle(chat),
        a.prompt,
        a.left_label,
        a.right_label
    );
}

// 8 args: same orchestration shape as handle_start — socket halves, identity,
// config, the chat message, the side/amount, and the shared active + pending
// state. A struct would obscure the flow, not clarify it.
#[allow(clippy::too_many_arguments)]
async fn handle_bet(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    settings: &ModuleSettings,
    chat: &ChatMessage,
    side: Side,
    amount: i64,
    active: &Option<Prediction>,
    pending_bet: &mut Option<PendingBet>,
) {
    let Some(pred) = active.as_ref() else {
        info!("[predictions] '{}' bet but no prediction is open", handle(chat));
        return;
    };
    if pred.status != Status::Open {
        info!("[predictions] '{}' bet but the prediction is not open", handle(chat));
        return;
    }
    let uuid = chat.user_uuid7.clone();
    if uuid.is_empty() {
        info!("[predictions] '{}' bet rejected: no user_uuid7", handle(chat));
        return;
    }
    if amount <= 0 {
        info!("[predictions] '{}' bet rejected: amount must be positive", handle(chat));
        return;
    }
    if settings.bet_max > 0 && amount > settings.bet_max {
        info!("[predictions] '{}' bet rejected: amount {} > bet_max {}", handle(chat), amount, settings.bet_max);
        return;
    }
    if pending_bet.is_some() {
        info!("[predictions] '{}' bet rejected: a previous bet is still being scored", handle(chat));
        return;
    }
    // Stash the pending bet, then ask the engine for the actor's current score.
    // The correlated DatabaseQueryResult("prediction_get_score") completes it —
    // no blocking here, the read loop just keeps processing.
    *pending_bet = Some(PendingBet {
        user_uuid7: uuid.clone(),
        handle: handle(chat).to_string(),
        side,
        amount,
    });
    let sql = serde_json::json!({ "uuid7": uuid }).to_string();
    send_query(write_shared, identity, "prediction_get_score", sql).await;
}

async fn handle_stop(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    settings: &ModuleSettings,
    chat: &ChatMessage,
    winner: Option<Side>,
    active: &mut Option<Prediction>,
) {
    if !role_ok(chat, &settings.creator_role) {
        info!("[predictions] '{}' denied stop (creator_role '{}')", handle(chat), settings.creator_role);
        return;
    }
    let Some(mut pred) = active.take() else {
        info!("[predictions] '{}' tried to stop but no prediction is open", handle(chat));
        return;
    };
    match winner {
        None => {
            // Cancel: refund every bet in full.
            let refunds = pred.cancel();
            for r in &refunds {
                let sql = serde_json::json!({
                    "uuid7": r.user,
                    "delta": r.amount,
                    "reason": "prediction refund",
                })
                .to_string();
                send_query(write_shared, identity, "userdb_adjust_score", sql).await;
            }
            broadcast_prediction(write_shared, identity, &pred).await;
            send_to_chat(write_shared, identity, chat, "prediction cancelled — all bets refunded.").await;
            info!(
                "[predictions] '{}' cancelled '{}' — {} bet(s) refunded",
                handle(chat),
                pred.prompt,
                refunds.len()
            );
        }
        Some(side) => {
            // Resolve: parimutuel payout to the winners (zero-sum, integer floor).
            let pot = pred.pot();
            let payouts = pred.resolve(side);
            for p in &payouts {
                let sql = serde_json::json!({
                    "uuid7": p.user,
                    "delta": p.amount,
                    "reason": "prediction payout",
                })
                .to_string();
                send_query(write_shared, identity, "userdb_adjust_score", sql).await;
            }
            broadcast_prediction(write_shared, identity, &pred).await;
            let (side_name, label) = match side {
                Side::Left => ("Left", pred.left_label.as_str()),
                Side::Right => ("Right", pred.right_label.as_str()),
            };
            send_to_chat(
                write_shared,
                identity,
                chat,
                &format!(
                    "{} [{}] wins — {} winner(s) split {} points.",
                    side_name,
                    label,
                    payouts.len(),
                    pot
                ),
            )
            .await;
            info!(
                "[predictions] '{}' resolved {} for '{}' — {} payout(s), pot {}",
                handle(chat),
                side_name,
                pred.prompt,
                payouts.len(),
                pot
            );
        }
    }
}

// 8 args: same orchestration shape as handle_start — socket halves, identity,
// config, the chat message, the prompt/options/hide-counts, and the shared
// active-poll state. A struct would obscure the flow, not clarify it.
#[allow(clippy::too_many_arguments)]
async fn handle_poll_create(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    settings: &ModuleSettings,
    chat: &ChatMessage,
    prompt: String,
    options: Vec<String>,
    hide_counts: bool,
    active_poll: &mut Option<Poll>,
) {
    if !role_ok(chat, &settings.poll_creator_role) {
        info!(
            "[predictions] '{}' denied poll create (poll_creator_role '{}')",
            handle(chat),
            settings.poll_creator_role
        );
        return;
    }
    if active_poll.is_some() {
        info!("[predictions] '{}' tried to create a poll but one is already open", handle(chat));
        return;
    }
    if prompt.trim().is_empty() {
        info!("[predictions] '{}' poll create rejected: empty prompt", handle(chat));
        return;
    }
    let poll = Poll::new(logic::new_uuid7(), prompt, options, hide_counts);
    *active_poll = Some(poll);
    broadcast_poll(write_shared, identity, active_poll.as_ref().unwrap()).await;
    let a = active_poll.as_ref().unwrap();
    send_to_chat(
        write_shared,
        identity,
        chat,
        &format!(
            "Poll started: {} — vote with !poll -1 .. -{}.",
            a.prompt,
            a.options.len()
        ),
    )
    .await;
    info!(
        "[predictions] '{}' created poll '{}' with {} option(s){}",
        handle(chat),
        a.prompt,
        a.options.len(),
        if a.hide_counts { " (counts hidden)" } else { "" }
    );
}

async fn handle_poll_vote(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    chat: &ChatMessage,
    option_index: usize,
    active_poll: &mut Option<Poll>,
) {
    let Some(poll) = active_poll.as_mut() else {
        info!("[predictions] '{}' voted but no poll is open", handle(chat));
        return;
    };
    if poll.status != PollStatus::Open {
        info!("[predictions] '{}' voted but the poll is not open", handle(chat));
        return;
    }
    let uuid = chat.user_uuid7.clone();
    if uuid.is_empty() {
        info!("[predictions] '{}' vote rejected: no user_uuid7", handle(chat));
        return;
    }
    match poll.cast_vote(&uuid, option_index) {
        Ok(idx) => {
            broadcast_poll(write_shared, identity, poll).await;
            info!(
                "[predictions] '{}' voted option {} on '{}'",
                handle(chat),
                idx + 1,
                poll.prompt
            );
        }
        Err(logic::PollError::AlreadyVoted) => {
            info!(
                "[predictions] '{}' repeat vote on '{}' ignored (first vote kept)",
                handle(chat),
                poll.prompt
            );
        }
        Err(logic::PollError::InvalidOption) => {
            info!(
                "[predictions] '{}' vote rejected: invalid option {}",
                handle(chat),
                option_index + 1
            );
        }
    }
}

async fn handle_poll_close(
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    settings: &ModuleSettings,
    chat: &ChatMessage,
    active_poll: &mut Option<Poll>,
) {
    if !role_ok(chat, &settings.poll_creator_role) {
        info!(
            "[predictions] '{}' denied poll close (poll_creator_role '{}')",
            handle(chat),
            settings.poll_creator_role
        );
        return;
    }
    let Some(mut poll) = active_poll.take() else {
        info!("[predictions] '{}' tried to close a poll but none is open", handle(chat));
        return;
    };
    let winner = poll.close();
    broadcast_poll(write_shared, identity, &poll).await;
    let msg = match winner {
        Some(i) => format!(
            "Poll '{}' closed — winner: {} ({} votes).",
            poll.prompt,
            poll.options[i],
            poll.votes[i]
        ),
        None => format!("Poll '{}' closed — no winner (tie).", poll.prompt),
    };
    send_to_chat(write_shared, identity, chat, &msg).await;
    info!(
        "[predictions] '{}' closed poll '{}' — winner {:?}",
        handle(chat),
        poll.prompt,
        winner
    );
}

/// Write the display's connection config and keep the display window alive:
/// respawn it (with backoff) if the binary is missing or dies. Inherited env
/// carries COCKATIEL_PIN / COCKATIEL_TLS_CERT to the child.
async fn display_supervisor(settings: &ModuleSettings) {
    let mut backoff = settings.reconnect_base_secs;
    loop {
        let cfg = serde_json::json!({
            "ip": settings.engine_ip,
            "port": settings.engine_port,
            "module_name": "prediction-display",
            "position": 1,
            "priority": 100,
            "module_specific": {
                "hold_after_resolve_secs": settings.hold_after_resolve_secs,
            },
        });
        let _ = std::fs::write(
            &settings.display_config,
            serde_json::to_string_pretty(&cfg).unwrap(),
        );
        match std::process::Command::new(&settings.display_bin).spawn() {
            Ok(mut child) => {
                info!("[predictions] display spawned (pid {})", child.id());
                backoff = settings.reconnect_base_secs;
                let _ = child.wait();
                warn!("[predictions] display exited — respawning...");
            }
            Err(e) => {
                warn!(
                    "[predictions] display spawn failed ({}): retrying in {}s",
                    e, backoff
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(settings.reconnect_max_secs);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let settings = ensure_defaults();
    let client = CockatielClient::connect("config.json").await?;
    let (write, read) = client.stream.split();
    let write_shared: Arc<AsyncMutex<WsWriteHalf>> = Arc::new(AsyncMutex::new(write));
    info!("predictions module connected as '{}'", client.config.module_name);
    let identity: Arc<AsyncMutex<EngineIdentity>> = Arc::new(AsyncMutex::new(EngineIdentity {
        auth: client.auth_token.clone(),
        instance: client.instance_uuid7.clone(),
        module: client.config.module_name.clone(),
    }));

    // Display supervisor: write prediction_display.json and keep the window
    // alive (respawn with backoff on exit). Runs for the lifetime of the brain.
    let display_settings = settings.clone();
    tokio::spawn(async move {
        display_supervisor(&display_settings).await;
    });

    let settings_for_task = settings.clone();
    let command_flag = settings.command_flag.clone();
    let write_for_task = Arc::clone(&write_shared);
    let identity_for_task = Arc::clone(&identity);

    // Read loop + engine-session supervisor. Prediction and poll state (active +
    // the pending bet) live here and survive socket drops.
    tokio::spawn(async move {
        let mut read = read;
        let mut active: Option<Prediction> = None;
        let mut active_poll: Option<Poll> = None;
        let mut pending_bet: Option<PendingBet> = None;
        loop {
            // Fresh session (initial connect + every reconnect): register the
            // command — the engine forgets commands when a socket drops.
            register_commands(&write_for_task, &identity_for_task, &command_flag).await;
            loop {
                let Some(msg) = read.next().await else { break };
                let data = match msg {
                    Ok(WsMessage::Binary(d)) => d,
                    Ok(WsMessage::Close(_)) => {
                        info!("Engine closed connection");
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        warn!("Engine WebSocket error: {}", e);
                        break;
                    }
                };
                let Ok(container) = Container::decode(data.as_ref()) else { continue };
                let id = identity_for_task.lock().await.clone();
                match container.payload {
                    Some(Payload::AuthVerify(_)) => {
                        let reply = Container {
                            version: 1,
                            auth_token: id.auth.clone(),
                            module_name: id.module.clone(),
                            module_instance_uuid7: id.instance.clone(),
                            payload: Some(Payload::AuthVerify(AuthVerify {
                                cur_auth: id.auth.clone(),
                            })),
                        };
                        send_container(&write_for_task, reply).await;
                    }
                    Some(Payload::DatabaseQueryResult(res)) => {
                        match res.query_id.as_str() {
                            "prediction_get_score" => {
                                // Correlated bet completion: the pending bet's
                                // score lookup answered. Deduct, record, broadcast.
                                let Some(pb) = pending_bet.take() else {
                                    warn!("[predictions] stray prediction_get_score result (no pending bet)");
                                    continue;
                                };
                                if !res.success {
                                    warn!("[predictions] score query failed for '{}': {}", pb.handle, res.error);
                                    continue;
                                }
                                let score = String::from_utf8(res.result_blob)
                                    .ok()
                                    .and_then(|blob| {
                                        serde_json::from_str::<serde_json::Value>(&blob).ok()
                                    })
                                    .and_then(|v| v.get("score").and_then(|s| s.as_i64()))
                                    .unwrap_or(0);
                                let Some(pred) = active.as_mut() else {
                                    warn!("[predictions] bet scored for '{}' but the prediction vanished", pb.handle);
                                    continue;
                                };
                                match pred.place_bet(
                                    &pb.user_uuid7,
                                    pb.side,
                                    pb.amount,
                                    score,
                                    settings_for_task.bet_min,
                                    settings_for_task.bet_max,
                                ) {
                                    Ok(_) => {
                                        let sql = serde_json::json!({
                                            "uuid7": pb.user_uuid7,
                                            "delta": -pb.amount,
                                            "reason": "prediction bet",
                                        })
                                        .to_string();
                                        send_query(
                                            &write_for_task,
                                            &identity_for_task,
                                            "userdb_adjust_score",
                                            sql,
                                        )
                                        .await;
                                        broadcast_prediction(&write_for_task, &identity_for_task, pred).await;
                                        let side_name = match pb.side {
                                            Side::Left => "left",
                                            Side::Right => "right",
                                        };
                                        info!(
                                            "[predictions] '{}' bet {} on {} (score {} -> {})",
                                            pb.handle,
                                            pb.amount,
                                            side_name,
                                            score,
                                            score - pb.amount
                                        );
                                    }
                                    Err(e) => {
                                        info!("[predictions] '{}' bet rejected: {:?}", pb.handle, e);
                                    }
                                }
                            }
                            "userdb_adjust_score" if !res.success => {
                                warn!("[predictions] userdb_adjust_score failed: {}", res.error);
                            }
                            _ => {}
                        }
                    }
                    Some(Payload::MessagePreProcess(pre)) => {
                        let MessagePreProcess {
                            message_uuid7: uuid,
                            raw_message,
                            audio,
                            audio_type,
                        } = pre;
                        if let Some(chat) = &raw_message {
                            if let Some(cmd) = &chat.command {
                                if cmd.command_name == COMMAND_NAME {
                                    // The engine parses flags into command_flags
                                    // (each Flag = flag_name + value); the logic
                                    // parser also inspects the raw text for the
                                    // subcommand token after `!pred`.
                                    let flags: Vec<(String, String)> = cmd
                                        .command_flags
                                        .iter()
                                        .map(|f| (f.flag_name.clone(), f.value.clone()))
                                        .collect();
                                    let parsed = logic::parse_command(
                                        &chat.raw_message,
                                        &cmd.command_flag,
                                        &cmd.command_name,
                                        &flags,
                                    );
                                    match parsed {
                                        ParsedCommand::Start {
                                            prompt,
                                            left_label,
                                            right_label,
                                        } => {
                                            handle_start(
                                                &write_for_task,
                                                &identity_for_task,
                                                &settings_for_task,
                                                chat,
                                                prompt,
                                                left_label,
                                                right_label,
                                                &mut active,
                                            )
                                            .await;
                                        }
                                        ParsedCommand::Bet { side, amount } => {
                                            handle_bet(
                                                &write_for_task,
                                                &identity_for_task,
                                                &settings_for_task,
                                                chat,
                                                side,
                                                amount,
                                                &active,
                                                &mut pending_bet,
                                            )
                                            .await;
                                        }
                                        ParsedCommand::Stop { winner } => {
                                            handle_stop(
                                                &write_for_task,
                                                &identity_for_task,
                                                &settings_for_task,
                                                chat,
                                                winner,
                                                &mut active,
                                            )
                                            .await;
                                        }
                                        ParsedCommand::None => {}
                                    }
                                } else if cmd.command_name == POLL_COMMAND_NAME {
                                    // `!poll` — free-vote polls. The engine
                                    // flags carry -p/-1..-6/-h on a create, a
                                    // bare -N is a vote, and the raw text's
                                    // `stop` subcommand closes the poll.
                                    let flags: Vec<(String, String)> = cmd
                                        .command_flags
                                        .iter()
                                        .map(|f| (f.flag_name.clone(), f.value.clone()))
                                        .collect();
                                    let parsed = logic::parse_poll_command(
                                        &chat.raw_message,
                                        &cmd.command_flag,
                                        &cmd.command_name,
                                        &flags,
                                    );
                                    match parsed {
                                        PollCommand::Create {
                                            prompt,
                                            options,
                                            hide_counts,
                                        } => {
                                            handle_poll_create(
                                                &write_for_task,
                                                &identity_for_task,
                                                &settings_for_task,
                                                chat,
                                                prompt,
                                                options,
                                                hide_counts,
                                                &mut active_poll,
                                            )
                                            .await;
                                        }
                                        PollCommand::Vote { option_index } => {
                                            handle_poll_vote(
                                                &write_for_task,
                                                &identity_for_task,
                                                chat,
                                                option_index,
                                                &mut active_poll,
                                            )
                                            .await;
                                        }
                                        PollCommand::Stop => {
                                            handle_poll_close(
                                                &write_for_task,
                                                &identity_for_task,
                                                &settings_for_task,
                                                chat,
                                                &mut active_poll,
                                            )
                                            .await;
                                        }
                                        PollCommand::None => {}
                                    }
                                }
                            }
                        }
                        // ALWAYS ack the pre-process stage: echo the raw
                        // ChatMessage back with the same message_uuid7 so the
                        // engine advances instead of stalling until the timeout
                        // sweep — on every path, command or not.
                        let ack = Container {
                            version: 1,
                            auth_token: id.auth.clone(),
                            module_name: id.module.clone(),
                            module_instance_uuid7: id.instance.clone(),
                            payload: Some(Payload::MessagePreProcess(MessagePreProcess {
                                message_uuid7: uuid,
                                raw_message,
                                audio,
                                audio_type,
                            })),
                        };
                        send_container(&write_for_task, ack).await;
                    }
                    Some(Payload::MessageInProcess(process)) => {
                        // Pass-through ack of the in-process stage so it never
                        // stalls.
                        let ack = Container {
                            version: 1,
                            auth_token: id.auth.clone(),
                            module_name: id.module.clone(),
                            module_instance_uuid7: id.instance.clone(),
                            payload: Some(Payload::MessageInProcess(MessageInProcess {
                                message_uuid7: process.message_uuid7,
                                raw_message: process.raw_message,
                                processed_message: process.processed_message,
                                abandon_message: process.abandon_message,
                                audio: process.audio,
                                audio_type: process.audio_type,
                            })),
                        };
                        send_container(&write_for_task, ack).await;
                    }
                    _ => {}
                }
            }

            // The engine connection dropped — a pending score lookup will never
            // be answered, so clear it; reconnect with backoff instead of
            // leaving the module unresponsive.
            pending_bet = None;
            info!("Engine disconnected — reconnecting...");
            let mut backoff = settings_for_task.reconnect_base_secs;
            loop {
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                match CockatielClient::connect("config.json").await {
                    Ok(conn) => {
                        info!("Reconnected to engine");
                        let (w, r) = conn.stream.split();
                        *write_for_task.lock().await = w;
                        *identity_for_task.lock().await = EngineIdentity {
                            auth: conn.auth_token,
                            instance: conn.instance_uuid7,
                            module: conn.config.module_name,
                        };
                        read = r;
                        break; // back to outer loop → re-register the command
                    }
                    Err(e) => {
                        warn!("Engine reconnect failed: {} — retrying in {}s", e, backoff);
                        backoff = (backoff * 2).min(settings_for_task.reconnect_max_secs);
                    }
                }
            }
        }
    });

    // The read-loop task owns the socket + state; main just keeps the process
    // alive.
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll(options: &[&str]) -> Poll {
        Poll::new(
            "poll-1".to_string(),
            "Best game?".to_string(),
            options.iter().map(|s| s.to_string()).collect(),
            false,
        )
    }

    #[test]
    fn poll_update_projection_maps_open_poll() {
        let mut p = poll(&["A", "B"]);
        p.hide_counts = true;
        p.cast_vote("u1", 0).unwrap();
        let upd = poll_update_for(&p);
        assert_eq!(upd.poll_id, "poll-1");
        assert_eq!(upd.prompt, "Best game?");
        assert_eq!(upd.options, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(upd.vote_counts, vec![1, 0]);
        assert_eq!(upd.total_votes, 1);
        assert_eq!(upd.status, poll_update::Status::Open as i32);
        assert_eq!(upd.winner_index, -1);
        assert!(upd.hide_counts);
    }

    #[test]
    fn poll_update_projection_maps_closed_poll_with_winner() {
        let mut p = poll(&["A", "B"]);
        p.cast_vote("u1", 1).unwrap();
        p.close();
        let upd = poll_update_for(&p);
        assert_eq!(upd.status, poll_update::Status::Closed as i32);
        assert_eq!(upd.winner_index, 1);
        assert_eq!(upd.vote_counts, vec![0, 1]);
        assert_eq!(upd.total_votes, 1);
    }
}