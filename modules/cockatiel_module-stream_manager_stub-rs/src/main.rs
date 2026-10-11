//! stream-manager-stub — a credential-free stand-in for a platform adapter's
//! stream-control handling.
//!
//! It polls the engine's `stream_schedule` query; when a new `revision` appears
//! it "applies" the control (here: logs it) and reports the result back via the
//! `stream_control_status` write, exactly as a real adapter would after calling
//! its platform API. This lets the whole stream-manager flow (TUI write →
//! engine store → adapter apply → status back to the TUI) be exercised
//! end-to-end with no real credentials.
//!
//! It is a DEV/verification tool: `autostart: false`, `capabilities: "input"`.

use std::time::Duration;

use cockatiel_client::proto::container_for_engine::Payload as EnginePayload;
use cockatiel_client::proto::container_for_module::Payload as ModulePayload;
use cockatiel_client::proto::*;
use cockatiel_client::CockatielClient;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

const CONFIG_PATH: &str = "config.json";
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
/// How often the schedule is polled.
const POLL: Duration = Duration::from_secs(3);
/// The platform label this stub reports under.
const PLATFORM: &str = "stub";

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsWrite = SplitSink<WsStream, WsMessage>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    loop {
        if let Err(e) = run_once().await {
            eprintln!("[stream-manager-stub] {}", e);
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
    eprintln!("[stream-manager-stub] connected as {} [{}]", module_name, instance_uuid7);

    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_applied: Option<u64> = None;

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
                            Some(ModulePayload::DatabaseQueryResult(res))
                                if res.query_id == "stream_schedule" =>
                            {
                                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&res.result_blob) {
                                    let revision = json.get("revision").and_then(|v| v.as_u64()).unwrap_or(0);
                                    // Nothing to do before the operator sets a control,
                                    // or for a revision already applied.
                                    if revision == 0 || last_applied == Some(revision) {
                                        continue;
                                    }
                                    let title = json.get("title").and_then(|v| v.as_str()).unwrap_or("");
                                    let start = json.get("scheduled_start_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                                    let thumb = json.get("thumbnail_path").and_then(|v| v.as_str()).unwrap_or("");
                                    // A real adapter would call its platform API here.
                                    eprintln!(
                                        "[stream-manager-stub] applying rev {}: title={:?} start={} thumbnail={:?}",
                                        revision, title, start, thumb
                                    );
                                    last_applied = Some(revision);
                                    let status = serde_json::json!({
                                        "platform": PLATFORM,
                                        "revision": revision,
                                        "ok": true,
                                        "message": format!("applied title {:?}", title),
                                    });
                                    let report = make_container(
                                        &module_name,
                                        &instance_uuid7,
                                        &auth_token,
                                        EnginePayload::DatabaseQuery(DatabaseQuery {
                                            query_id: "stream_control_status".to_string(),
                                            sql: status.to_string(),
                                            params: vec![],
                                        }),
                                    );
                                    let _ = send_raw(&mut write, &report).await;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(format!("engine read error: {}", e)),
                    None => return Err("engine closed the connection".to_string()),
                }
            }
            _ = tick.tick() => {
                let query = make_container(
                    &module_name,
                    &instance_uuid7,
                    &auth_token,
                    EnginePayload::DatabaseQuery(DatabaseQuery {
                        query_id: "stream_schedule".to_string(),
                        sql: "{}".to_string(),
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
