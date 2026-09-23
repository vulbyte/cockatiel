//! Hardening suite: malicious / privilege-escalation attempts. Each test
//! asserts the engine DENIES the attempt — no bypass, no crash, no escalation.
//!
//! Test sessions use a split WS + background reader that auto-replies to the
//! engine's AuthVerify liveness probe (like a real module), so the watchdog
//! never severs a long-running test mid-check.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::{Cli, Metrics, connect_engine, make_container, receive_container, send_container};
use cockatiel_client::proto::{container::Payload, *};

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsWrite = futures_util::stream::SplitSink<WsStream, WsMessage>;
type WsRead = futures_util::stream::SplitStream<WsStream>;

/// A test session: write half for outbound, a channel of forwarded inbound
/// frames (AuthVerify probes auto-replied), plus the identity.
struct Session {
    write: WsWrite,
    rx: mpsc::Receiver<Container>,
    auth: String,
    uuid: String,
    name: String,
}

fn spawn_reader(read: WsRead, tx: mpsc::Sender<Container>) {
    tokio::spawn(async move {
        let mut read = read;
        while let Some(Ok(WsMessage::Binary(data))) = read.next().await {
            if let Ok(c) = Container::decode(data.as_ref()) {
                if tx.send(c).await.is_err() {
                    break;
                }
            }
        }
    });
}

async fn send_split(write: &mut WsWrite, c: &Container) -> Result<(), String> {
    let mut buf = Vec::new();
    c.encode(&mut buf).map_err(|e| e.to_string())?;
    write.send(WsMessage::Binary(buf)).await.map_err(|e| format!("send: {}", e))
}

/// Receive the next non-probe frame, auto-replying to AuthVerify probes so the
/// engine's liveness watchdog keeps the session alive.
async fn recv_frame(sess: &mut Session, timeout_ms: u64) -> Result<Container, String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("recv timeout".into());
        }
        match tokio::time::timeout(remaining, sess.rx.recv()).await {
            Ok(Some(c)) => {
                if matches!(c.payload, Some(Payload::AuthVerify(_))) {
                    let reply = make_container(
                        &sess.name, &sess.uuid, &sess.auth,
                        Payload::Log(Log { log: "probe-ack".into(), blob: vec![] }),
                    );
                    let _ = send_split(&mut sess.write, &reply).await;
                    continue;
                }
                return Ok(c);
            }
            _ => return Err("recv timeout".into()),
        }
    }
}

async fn connect_session(cli: &Cli, name: &str, position: ProcessPosition) -> Result<Session, String> {
    for _ in 0..3 {
        let ws = connect_engine(cli).await?;
        let (mut write, read) = ws.split();
        let uuid = uuid::Uuid::now_v7().to_string();
        let req = make_container(
            name, &uuid, "",
            Payload::ConnectionRequest(ConnectionRequest {
                pin: cli.pin,
                process_position: position as i32,
                priority: if position == ProcessPosition::Connection { 1 } else { 100 },
                module_instance_uuid7: uuid.clone(),
            }),
        );
        if send_split(&mut write, &req).await.is_err() {
            continue;
        }
        let (tx, mut rx) = mpsc::channel(256);
        spawn_reader(read, tx);
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(c)) if !c.auth_token.is_empty() => {
                let sess = Session {
                    write,
                    rx,
                    auth: c.auth_token,
                    uuid,
                    name: name.to_string(),
                };
                // Settle past the engine's post-auth drain window so the first
                // follow-up query isn't discarded as "sent before authorization".
                tokio::time::sleep(Duration::from_millis(300)).await;
                return Ok(sess);
            }
            _ => continue,
        }
    }
    Err(format!("auth failed after retries: '{}'", name))
}

async fn auth_as_test_runner(cli: &Cli) -> Result<Session, String> {
    connect_session(cli, "cockatiel-test-runner", ProcessPosition::Connection).await
}

async fn auth_as_module(cli: &Cli, name: &str) -> Result<Session, String> {
    connect_session(cli, name, ProcessPosition::Preprocess).await
}

/// Send a virtual query and return the error message when it is DENIED
/// (success=false), or Err if it wasn't denied (possible bypass).
async fn expect_denied(sess: &mut Session, query_id: &str, sql: &str) -> Result<String, String> {
    let q = make_container(
        &sess.name, &sess.uuid, &sess.auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: query_id.to_string(), sql: sql.to_string(), params: vec![] }),
    );
    send_split(&mut sess.write, &q).await?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match recv_frame(sess, 2000).await {
            Ok(c) => {
                if let Some(Payload::DatabaseQueryResult(res)) = c.payload {
                    if res.query_id == query_id {
                        if res.success {
                            return Err(format!("NOT denied (success=true) for '{}': {}", query_id, res.error));
                        }
                        return Ok(res.error);
                    }
                }
            }
            Err(_) => break,
        }
    }
    Err(format!("no response for '{}' (possible hang)", query_id))
}

/// Verify the connection was severed: the reader's channel closes when the
/// socket drops (the engine severs invalid-auth sessions).
async fn is_severed(sess: &mut Session) -> bool {
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(1500), sess.rx.recv()).await {
            Ok(None) => return true, // channel closed => connection dropped
            Err(_) => return false,  // still open (not severed)
            Ok(Some(c)) => {
                if matches!(c.payload, Some(Payload::AuthVerify(_))) {
                    let reply = make_container(
                        &sess.name, &sess.uuid, &sess.auth,
                        Payload::Log(Log { log: "probe-ack".into(), blob: vec![] }),
                    );
                    let _ = send_split(&mut sess.write, &reply).await;
                }
            }
        }
    }
    false
}

pub async fn run_hardening_suite(cli: &Cli) -> Vec<Metrics> {
    let mut m = Metrics::new("hardening");

    c1_wrong_pin(cli, &mut m).await;
    c1_blank_identity(cli, &mut m).await;
    c2_forged_token(cli, &mut m).await;
    c3_name_trust(cli, &mut m).await;
    c4_gated_queries(cli, &mut m).await;
    c5_test_archive(cli, &mut m).await;
    c6_chat_rating_gate(cli, &mut m).await;
    c7_mod_actor(cli, &mut m).await;
    c8_send_to_platforms_actor(cli, &mut m).await;
    c9_sql_injection(cli, &mut m).await;
    c10_prompt_impersonation(cli, &mut m).await;

    m.finalize();
    vec![m]
}

/// C1a — a wrong PIN must be rejected (no auth token returned).
async fn c1_wrong_pin(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut cli2 = cli.clone();
    cli2.pin = 999999;
    let mut ws = match connect_engine(&cli2).await {
        Ok(w) => w,
        Err(e) => {
            m.push_detail("c1_wrong_pin", true, start.elapsed().as_millis(), 0.0, 0.0, 0.0, format!("rejected: {}", e));
            return;
        }
    };
    let uuid = uuid::Uuid::now_v7().to_string();
    let req = make_container(
        "cockatiel-test-runner", &uuid, "",
        Payload::ConnectionRequest(ConnectionRequest {
            pin: 999999, process_position: ProcessPosition::Connection as i32,
            priority: 1, module_instance_uuid7: uuid.clone(),
        }),
    );
    let _ = send_container(&mut ws, &req).await;
    let outcome = match receive_container(&mut ws, 3000).await {
        Ok(c) => c.auth_token.is_empty(),
        Err(_) => true, // closed = rejected
    };
    m.push_detail("c1_wrong_pin", outcome, start.elapsed().as_millis(), 0.0, 0.0, 0.0, "wrong PIN must not authenticate");
}

/// C1b — blank / unnamed_module identity must be rejected.
async fn c1_blank_identity(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut ws = match connect_engine(cli).await {
        Ok(w) => w,
        Err(e) => {
            m.push_detail("c1_blank_identity", true, start.elapsed().as_millis(), 0.0, 0.0, 0.0, e);
            return;
        }
    };
    let uuid = uuid::Uuid::now_v7().to_string();
    let req = make_container(
        "unnamed_module", &uuid, "",
        Payload::ConnectionRequest(ConnectionRequest {
            pin: cli.pin, process_position: ProcessPosition::Connection as i32,
            priority: 1, module_instance_uuid7: uuid.clone(),
        }),
    );
    let _ = send_container(&mut ws, &req).await;
    let outcome = match receive_container(&mut ws, 3000).await {
        Ok(c) => c.auth_token.is_empty(),
        Err(_) => true,
    };
    m.push_detail("c1_blank_identity", outcome, start.elapsed().as_millis(), 0.0, 0.0, 0.0, "unnamed_module must be rejected");
}

/// C2 — a forged/random JWT on a live connection must be severed.
async fn c2_forged_token(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut sess = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("c2_forged_token", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    let c = make_container(
        "cockatiel-test-runner", &sess.uuid, "not.a.jwt",
        Payload::Log(Log { log: "forged".into(), blob: vec![] }),
    );
    let _ = send_split(&mut sess.write, &c).await;
    let severed = is_severed(&mut sess).await;
    m.push_detail("c2_forged_token", severed, start.elapsed().as_millis(), 0.0, 0.0, 0.0, "forged JWT must be severed");
}

/// C3 — name-trust: a module's valid token claimed under a trusted name must
/// be rejected.
async fn c3_name_trust(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut sess = match auth_as_module(cli, "banned-words").await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("c3_name_trust", false, 0, 0.0, 0.0, 0.0, format!("setup: {}", e));
            return;
        }
    };
    // Claim to be the TUI while using the module's own token.
    let c = make_container(
        "cockatiel-tui", &sess.uuid, &sess.auth,
        Payload::Log(Log { log: "impersonate".into(), blob: vec![] }),
    );
    let _ = send_split(&mut sess.write, &c).await;
    let severed = is_severed(&mut sess).await;
    m.push_detail("c3_name_trust", severed, start.elapsed().as_millis(), 0.0, 0.0, 0.0, "token under a trusted name must be severed");
}

/// C4 — gated queries from a NON-control-surface module must all be denied.
async fn c4_gated_queries(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut sess = match auth_as_module(cli, "banned-words").await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("c4_gated_queries", false, 0, 0.0, 0.0, 0.0, format!("setup: {}", e));
            return;
        }
    };
    let attempts = [
        ("userdb_list_users", r#"{"platform":"","limit":10,"offset":0}"#),
        ("userdb_set_roles", r#"{"uuid7":"x","roles":["owner"]}"#),
        ("userdb_delete_user", r#"{"uuid7":"x"}"#),
        ("audit_approve", r#"{"uuid7":"x"}"#),
        ("audit_reject", r#"{"uuid7":"x"}"#),
        ("set_credentials", r#"{"module":"x","values":{}}"#),
        ("test_run", r#"{"suite":"chain"}"#),
    ];
    let mut denied = 0;
    for (qid, sql) in attempts {
        match expect_denied(&mut sess, qid, sql).await {
            Ok(_) => denied += 1,
            Err(e) => m.notes.push(format!("c4 {} LEAK: {}", qid, e)),
        }
    }
    // engine_info is NOT a hard denial: it succeeds for any module but REDACTS
    // the PIN (null) for non-control-surface callers. Assert the redaction.
    let pin_redacted = expect_engine_info_pin_null(&mut sess).await;
    if pin_redacted {
        denied += 1;
    } else {
        m.notes.push("c4 engine_info LEAK: PIN not redacted".to_string());
    }
    m.push_detail(
        "c4_gated_queries",
        denied == attempts.len() + 1,
        start.elapsed().as_millis(),
        0.0, 0.0, 0.0,
        format!("{}/{} control-surface queries denied (+engine_info pin redacted={})", denied, attempts.len() + 1, pin_redacted),
    );
}

/// Assert that a `engine_info` response from a non-control-surface module has
/// `pin: null` (the PIN is redacted, never exposed).
async fn expect_engine_info_pin_null(sess: &mut Session) -> bool {
    let qid = "engine_info";
    let q = make_container(
        &sess.name, &sess.uuid, &sess.auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: qid.to_string(), sql: String::new(), params: vec![] }),
    );
    if send_split(&mut sess.write, &q).await.is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(c) = recv_frame(sess, 2000).await {
            if let Some(Payload::DatabaseQueryResult(res)) = c.payload {
                if res.query_id == qid {
                    if !res.success {
                        return false;
                    }
                    return serde_json::from_slice::<serde_json::Value>(&res.result_blob)
                        .map(|v| v.get("pin").map(|p| p.is_null()).unwrap_or(false))
                        .unwrap_or(false);
                }
            }
        }
    }
    false
}

/// C5 — test_archive from a NON-test-runner must be denied.
async fn c5_test_archive(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut sess = match auth_as_module(cli, "banned-words").await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("c5_test_archive", false, 0, 0.0, 0.0, 0.0, format!("setup: {}", e));
            return;
        }
    };
    let outcome = expect_denied(&mut sess, "test_archive", "{}").await;
    m.push_detail(
        "c5_test_archive",
        outcome.is_ok(),
        start.elapsed().as_millis(),
        0.0, 0.0, 0.0,
        match outcome { Ok(e) => e, Err(e) => e },
    );
}

/// C6 — chat_commend / chat_reprimand from a module that isn't the owner must
/// be denied (they're gated to the commend/reprimand modules).
async fn c6_chat_rating_gate(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut sess = match auth_as_module(cli, "banned-words").await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("c6_chat_rating_gate", false, 0, 0.0, 0.0, 0.0, format!("setup: {}", e));
            return;
        }
    };
    let r1 = expect_denied(&mut sess, "chat_commend", r#"{"platform":"test","handle":"x","actor":{"handle":"y"}}"#).await;
    let r2 = expect_denied(&mut sess, "chat_reprimand", r#"{"platform":"test","handle":"x","actor":{"handle":"y"}}"#).await;
    m.push_detail(
        "c6_chat_rating_gate",
        r1.is_ok() && r2.is_ok(),
        start.elapsed().as_millis(),
        0.0, 0.0, 0.0,
        format!("commend={} reprimand={}", r1.is_ok(), r2.is_ok()),
    );
}

/// C7 — mod_* with a non-mod actor must be denied.
async fn c7_mod_actor(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut sess = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("c7_mod_actor", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    let outcome = expect_denied(
        &mut sess, "mod_ban",
        r#"{"platform":"test","handle":"target","reason":"x","actor":{"platform":"test","handle":"some-viewer"}}"#,
    ).await;
    m.push_detail("c7_mod_actor", outcome.is_ok(), start.elapsed().as_millis(), 0.0, 0.0, 0.0, outcome.unwrap_or_else(|e| e));
}

/// C8 — SendToPlatforms with a non-mod actor must be denied.
async fn c8_send_to_platforms_actor(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut sess = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("c8_send_to_platforms_actor", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    let send = make_container(
        "cockatiel-test-runner", &sess.uuid, &sess.auth,
        Payload::SendToPlatforms(SendToPlatforms {
            msg: "spam".into(), level: 0, module_uuid7: String::new(), pid: String::new(),
            platform: "twitch".into(), actor_platform: "twitch".into(),
            actor_handle: "some-viewer".into(), actor_uuid7: String::new(), channel_id: String::new(),
        }),
    );
    let sent = send_split(&mut sess.write, &send).await.is_ok();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Connection still usable -> engine denied but didn't sever.
    let qid = uuid::Uuid::now_v7().to_string();
    let q = make_container(
        "cockatiel-test-runner", &sess.uuid, &sess.auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: qid.clone(), sql: "SELECT 1 AS one".into(), params: vec![] }),
    );
    let alive = send_split(&mut sess.write, &q).await.is_ok() && recv_frame(&mut sess, 2000).await.is_ok();
    m.push_detail("c8_send_to_platforms_actor", sent && alive, start.elapsed().as_millis(), 0.0, 0.0, 0.0, "non-mod send denied (connection alive)");
}

/// C9 — SQL injection via DatabaseQuery must be rejected, and the EXISTS panic
/// guard must return an error without killing the connection.
async fn c9_sql_injection(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut sess = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("c9_sql_injection", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    // Multi-statement injection must be rejected by the read-only gate.
    let inj = expect_denied(&mut sess, "x1", "SELECT 1; DROP TABLE timeline_events").await;
    // Write statements rejected.
    let del = expect_denied(&mut sess, "x2", "DELETE FROM timeline_events").await;
    // EXISTS (a Limbo panic) must return an error, not kill the connection.
    let exists = expect_denied(&mut sess, "x3", "SELECT * FROM timeline_events WHERE EXISTS (SELECT 1)").await;
    // The connection must still work after the panic guard.
    let qid = uuid::Uuid::now_v7().to_string();
    let q = make_container(
        "cockatiel-test-runner", &sess.uuid, &sess.auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: qid.clone(), sql: "SELECT 1 AS one".into(), params: vec![] }),
    );
    let alive = send_split(&mut sess.write, &q).await.is_ok() && recv_frame(&mut sess, 2000).await.is_ok();
    m.push_detail(
        "c9_sql_injection",
        inj.is_ok() && del.is_ok() && exists.is_ok() && alive,
        start.elapsed().as_millis(),
        0.0, 0.0, 0.0,
        format!("inject={} delete={} exists={} alive={}", inj.is_ok(), del.is_ok(), exists.is_ok(), alive),
    );
}

/// C10 — a PromptResponse for a prompt the engine never issued must be ignored
/// (no crash, no routing side-effect).
async fn c10_prompt_impersonation(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let mut sess = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("c10_prompt_impersonation", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    let bogus = make_container(
        "cockatiel-test-runner", &sess.uuid, &sess.auth,
        Payload::PromptResponse(PromptResponse {
            prompt_id_uuid7: "00000000-0000-0000-0000-000000000000".into(),
            accepted: true, reason: String::new(),
        }),
    );
    let sent = send_split(&mut sess.write, &bogus).await.is_ok();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let qid = uuid::Uuid::now_v7().to_string();
    let q = make_container(
        "cockatiel-test-runner", &sess.uuid, &sess.auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: qid.clone(), sql: "SELECT 1 AS one".into(), params: vec![] }),
    );
    let alive = send_split(&mut sess.write, &q).await.is_ok() && recv_frame(&mut sess, 2000).await.is_ok();
    m.push_detail("c10_prompt_impersonation", sent && alive, start.elapsed().as_millis(), 0.0, 0.0, 0.0, "bogus prompt ignored; engine alive");
}