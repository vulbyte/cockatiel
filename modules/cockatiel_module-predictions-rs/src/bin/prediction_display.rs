//! prediction_display — a minimal terminal window for predictions.
//!
//! Connects as "prediction-display", renders the engine's PredictionUpdate bars
//! and PollUpdate polls with plain ANSI escape codes (no ratatui). On
//! RESOLVED/CANCELLED (prediction) or CLOSED (poll) it holds the screen for
//! `hold_after_resolve_secs`, then clears to "no active prediction"/"no active
//! poll". Both update kinds share this one window — the last update received
//! wins the screen. Answers AuthVerify probes and reconnects with backoff.

use futures_util::{SinkExt, StreamExt};
use prost::Message;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tracing::{info, warn};
use tracing_subscriber::FmtSubscriber;

use cockatiel_client::{
    proto::container::Payload,
    proto::*,
    CockatielClient,
};
use cockatiel_prediction::logic::{self, PollStatus, Side, Status};

type WsWriteHalf = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    WsMessage,
>;
type WsReadHalf = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
>;

const DEFAULT_CONFIG: &str = "prediction_display.json";
const DEFAULT_HOLD_AFTER_RESOLVE_SECS: u64 = 30;
const DEFAULT_RECONNECT_BASE_SECS: u64 = 1;
const DEFAULT_RECONNECT_MAX_SECS: u64 = 30;

#[derive(Debug, Clone)]
struct DisplaySettings {
    hold_after_resolve_secs: u64,
    reconnect_base_secs: u64,
    reconnect_max_secs: u64,
}

/// Read the display's own config (written by the brain): `module_specific`
/// holds the hold/reconnect knobs; the brain sets `hold_after_resolve_secs` to
/// match its own. Defaults keep the window usable standalone.
fn ensure_defaults() -> DisplaySettings {
    let ms = std::fs::read_to_string(DEFAULT_CONFIG)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .and_then(|r: serde_json::Value| r.get("module_specific").cloned())
        .and_then(|m| m.as_object().cloned())
        .unwrap_or_default();
    DisplaySettings {
        hold_after_resolve_secs: ms
            .get("hold_after_resolve_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_HOLD_AFTER_RESOLVE_SECS),
        reconnect_base_secs: ms
            .get("reconnect_base_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_RECONNECT_BASE_SECS),
        reconnect_max_secs: ms
            .get("reconnect_max_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_RECONNECT_MAX_SECS),
    }
}

/// Session identity for AuthVerify replies.
#[derive(Clone)]
struct EngineIdentity {
    auth: String,
    instance: String,
    module: String,
}

async fn send_container(write_shared: &Arc<AsyncMutex<WsWriteHalf>>, container: Container) {
    let mut buf = Vec::new();
    if container.encode(&mut buf).is_ok() {
        let mut w = write_shared.lock().await;
        let _ = w.send(WsMessage::Binary(buf)).await;
    }
}

/// Map the wire status code (prost i32) onto the logic Status.
fn status_of(code: i32) -> Status {
    match code {
        2 => Status::Resolved,
        3 => Status::Cancelled,
        _ => Status::Open,
    }
}

/// Map the wire winner_side string ("left"/"right") onto a logic Side.
fn side_of(winner: &str) -> Option<Side> {
    match winner {
        "left" => Some(Side::Left),
        "right" => Some(Side::Right),
        _ => None,
    }
}

/// Render a PredictionUpdate through the shared pure renderer and print it.
fn print_update(upd: &PredictionUpdate) {
    let screen = logic::render_screen(
        &upd.prompt,
        &upd.side_left_label,
        upd.side_left_total,
        &upd.side_right_label,
        upd.side_right_total,
        status_of(upd.status),
        side_of(&upd.winner_side),
    );
    print!("{}", screen);
    let _ = std::io::stdout().flush();
}

/// Map the wire PollUpdate status code (prost i32) onto the logic PollStatus.
fn poll_status_of(code: i32) -> PollStatus {
    match code {
        2 => PollStatus::Closed,
        _ => PollStatus::Open,
    }
}

/// Render a PollUpdate through the shared pure renderer and print it.
fn print_poll_update(upd: &PollUpdate) {
    let screen = logic::render_poll_screen(
        &upd.prompt,
        &upd.options,
        &upd.vote_counts,
        upd.total_votes,
        poll_status_of(upd.status),
        upd.winner_index,
        upd.hide_counts,
    );
    print!("{}", screen);
    let _ = std::io::stdout().flush();
}

/// After a RESOLVED/CANCELLED bar (or CLOSED poll), hold it on screen for
/// `hold_secs` while still answering probes and re-rendering any fresh
/// PredictionUpdate/PollUpdate (a new OPEN one exits the hold early). The idle
/// screen shown on timeout tracks whichever CLOSED/resolved update started (or
/// most recently refreshed) the hold. Returns true if the stream closed.
async fn hold_update(
    read: &mut WsReadHalf,
    write_shared: &Arc<AsyncMutex<WsWriteHalf>>,
    identity: &Arc<AsyncMutex<EngineIdentity>>,
    hold_secs: u64,
    mut idle: fn() -> String,
) -> bool {
    let sleep = tokio::time::sleep(Duration::from_secs(hold_secs));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => {
                print!("{}", idle());
                let _ = std::io::stdout().flush();
                return false;
            }
            msg = read.next() => {
                let Some(msg) = msg else { return true };
                let data = match msg {
                    Ok(WsMessage::Binary(d)) => d,
                    Ok(WsMessage::Close(_)) => return true,
                    Ok(_) => continue,
                    Err(e) => {
                        warn!("Engine WebSocket error during hold: {}", e);
                        return true;
                    }
                };
                let Ok(container) = Container::decode(data.as_ref()) else { continue };
                let id = identity.lock().await.clone();
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
                        send_container(write_shared, reply).await;
                    }
                    Some(Payload::PredictionUpdate(upd)) => {
                        print_update(&upd);
                        if matches!(status_of(upd.status), Status::Open) {
                            return false; // fresh prediction — back to the main loop
                        }
                        idle = logic::render_idle;
                        sleep.as_mut().reset(Instant::now() + Duration::from_secs(hold_secs));
                    }
                    Some(Payload::PollUpdate(upd)) => {
                        print_poll_update(&upd);
                        if matches!(poll_status_of(upd.status), PollStatus::Open) {
                            return false; // fresh poll — back to the main loop
                        }
                        idle = logic::render_poll_idle;
                        sleep.as_mut().reset(Instant::now() + Duration::from_secs(hold_secs));
                    }
                    _ => {}
                }
            }
        }
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
    let hold_secs = settings.hold_after_resolve_secs;
    let mut backoff = settings.reconnect_base_secs;

    loop {
        match CockatielClient::connect(DEFAULT_CONFIG).await {
            Ok(client) => {
                info!("prediction display connected as '{}'", client.config.module_name);
                let (write, read) = client.stream.split();
                let write_shared: Arc<AsyncMutex<WsWriteHalf>> = Arc::new(AsyncMutex::new(write));
                let identity: Arc<AsyncMutex<EngineIdentity>> =
                    Arc::new(AsyncMutex::new(EngineIdentity {
                        auth: client.auth_token.clone(),
                        instance: client.instance_uuid7.clone(),
                        module: client.config.module_name.clone(),
                    }));
                backoff = settings.reconnect_base_secs;
                let mut read = read;
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
                    let id = identity.lock().await.clone();
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
                            send_container(&write_shared, reply).await;
                        }
                        Some(Payload::PredictionUpdate(upd)) => {
                            let hold_this = matches!(
                                status_of(upd.status),
                                Status::Resolved | Status::Cancelled
                            );
                            print_update(&upd);
                            if hold_this
                                && hold_update(
                                    &mut read,
                                    &write_shared,
                                    &identity,
                                    hold_secs,
                                    logic::render_idle,
                                )
                                .await
                            {
                                break; // stream closed during the hold
                            }
                        }
                        // Polls share the same display window: the last update
                        // received (prediction or poll) wins the screen. A
                        // CLOSED poll holds, then clears to "no active poll".
                        Some(Payload::PollUpdate(upd)) => {
                            let hold_this = matches!(poll_status_of(upd.status), PollStatus::Closed);
                            print_poll_update(&upd);
                            if hold_this
                                && hold_update(
                                    &mut read,
                                    &write_shared,
                                    &identity,
                                    hold_secs,
                                    logic::render_poll_idle,
                                )
                                .await
                            {
                                break; // stream closed during the hold
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                warn!("display connect failed: {} — retrying in {}s", e, backoff);
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(settings.reconnect_max_secs);
    }
}