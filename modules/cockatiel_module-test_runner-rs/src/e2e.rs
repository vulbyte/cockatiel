//! End-to-end suite: drives the whole system through the real TLS WebSocket
//! surface as the CONTROL SURFACE (the TUI), then verifies the parts the other
//! suites leave unchecked.
//!
//! Where `chain` proves a timeline row exists and `probe` proves modules
//! connect, this suite asserts the SYSTEM actually works:
//!
//! * the full query surface answers with the right shapes (`db_status`,
//!   `engine_info`, `module_list` incl. the gate columns, `stats`,
//!   `channel_viewers`);
//! * the user-database lifecycle round-trips (add → get → score → value → list
//!   → delete) through the engine's `userdb_*` surface;
//! * an adapter message traverses the WHOLE pipeline with the real modules
//!   connected and lands `complete` with a `processed_message` and no error;
//! * pause holds a message (`queued`) and resume drives it to `complete`;
//! * a JWT reconnect re-authenticates and can query;
//! * the engine log produced during the run contains no panic / stage timeout /
//!   unresponsive-module / fatal line.
//!
//! It is deliberately strict: a module that connects but never processes still
//! fails here, because the pipeline row cannot reach `complete` while a
//! connected stage module withholds its ack (it times out and the row fails).

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use prost::Message;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::screening::WsStream;
use crate::{connect_engine, make_container, receive_container, send_container, Cli, Metrics};
use cockatiel_client::proto::{
    container_for_engine::Payload as EnginePayload,
    container_for_module::Payload as ModulePayload,
    *,
};

/// The control-surface identity. The engine always-trusts it and bootstraps the
/// first-ever registration, so the suite gets full query rights (stats,
/// userdb, pipeline pause, engine_info PIN).
const CONTROL: &str = "cockatiel-tui";

/// Log substrings that mean a REAL fault. Intentional negative-test noise
/// (screening/hardening) cannot appear in the run-scoped window this suite
/// checks, so nothing is allowlisted here.
const BAD_LOG_LINES: &[&str] = &[
    "panicked at",
    "FATAL",
    "[pipeline] stage",
    "Module unresponsive",
    "Recovery error",
    "failed to decode Protobuf",
];

pub async fn run_e2e_suite(cli: &Cli) -> Vec<Metrics> {
    let mut m = Metrics::new("e2e");
    if let Err(e) = run(cli, &mut m).await {
        m.notes.push(format!("e2e aborted: {e}"));
    }
    m.finalize();
    vec![m]
}

fn detail(m: &mut Metrics, name: &str, r: Result<(), String>) {
    match r {
        Ok(()) => m.push_detail(name, true, 0, 0.0, 0.0, 0.0, "ok"),
        Err(e) => m.push_detail(name, false, 0, 0.0, 0.0, 0.0, e),
    }
}

async fn run(cli: &Cli, m: &mut Metrics) -> Result<(), String> {
    // Scope the log check to THIS suite's window: read only the bytes the
    // engine appends from here on. The engine opens its log in append mode, so
    // the offset is stable.
    let log_path = engine_log_path(cli);
    let log_start = std::fs::metadata(&log_path).map(|meta| meta.len()).unwrap_or(0);

    // Launch the eligible real modules so the pipeline has real recipients.
    let launched = discover(cli);
    let mut children: Vec<std::process::Child> = Vec::new();
    for (dir, manifest, name) in &launched {
        match crate::probe::launch_module(dir, manifest, name, cli) {
            Some(child) => children.push(child),
            None => m.notes.push(format!("e2e: '{}' could not be launched", name)),
        }
    }
    let launched_names: Vec<String> = launched.iter().map(|(_, _, n)| n.clone()).collect();

    let (mut ws, auth, uuid) = match auth_control(cli).await {
        Ok(x) => x,
        Err(e) => {
            crate::probe::cleanup(&mut children);
            return Err(e);
        }
    };

    // Wait for the launched modules to connect (bounded).
    let connected = wait_for_connected(&mut ws, &auth, &uuid, launched_names.len(), Duration::from_secs(25)).await;
    m.notes.push(format!("e2e: launched={} connected={}", launched_names.len(), connected));

    // e1 — the query surface.
    detail(m, "e1_query_surface", query_surface(&mut ws, &auth, &uuid, cli).await);

    // Make sure the pipeline is RUNNING before the message tests (doctor also
    // runs the paused boot mode, where the engine starts held).
    let _ = query(
        &mut ws,
        &auth,
        &uuid,
        "pipeline_set_paused",
        &serde_json::json!({ "paused": false }).to_string(),
    )
    .await;

    // e2 — user-database lifecycle.
    detail(m, "e2_userdb_lifecycle", userdb_lifecycle(&mut ws, &auth, &uuid).await);

    // e3 — a plain adapter message through the whole pipeline.
    let nonce = uuid::Uuid::now_v7().to_string();
    detail(
        m,
        "e3_pipeline_plain",
        pipeline_roundtrip(&mut ws, &auth, &uuid, &format!("e2e plain {}", nonce)).await,
    );

    // e4 — a command message (engine-handled `!help`) through the pipeline.
    detail(
        m,
        "e4_pipeline_command",
        pipeline_roundtrip(&mut ws, &auth, &uuid, &format!("!help {}", nonce)).await,
    );

    // e5 — pause holds a message; resume drives it to completion.
    detail(m, "e5_pause_resume", pause_resume(&mut ws, &auth, &uuid, &nonce).await);

    // e6 — the engine-shutdown gate refuses by default and the engine survives.
    detail(m, "e6_engine_shutdown_gate", shutdown_gate(&mut ws, &auth, &uuid).await);

    // e7 — the stream leaderboard reflects the chatters the pipeline saw
    // (the `stream-leaderboard` module is launched + auto-approved like any
    // other, and reports the ranking the engine caches).
    detail(m, "e7_leaderboard", leaderboard(&mut ws, &auth, &uuid).await);

    // e8 — the stream-manager flow: a TUI `stream_control` write is stored and
    // the `stream-manager-stub` adapter polls it and reports back a status.
    detail(m, "e8_stream_control", stream_control(&mut ws, &auth, &uuid).await);

    // e9 — JWT reconnect (runs late: it claims the socket for the session).
    detail(m, "e9_reconnect", reconnect(cli, &auth, &uuid).await);

    // e10 — the engine log written during this suite must be clean.
    detail(m, "e10_engine_log_clean", engine_log_clean(&log_path, log_start));

    let _ = ws.close(None).await;
    crate::probe::cleanup(&mut children);
    Ok(())
}

// ── auth ────────────────────────────────────────────────────────────

/// Authenticate as the control surface (`cockatiel-tui`): pinned uuid first
/// (when the sandbox registered one), else a fresh uuid which bootstraps the
/// always-trusted name.
async fn auth_control(cli: &Cli) -> Result<(WsStream, String, String), String> {
    let mut uuids = Vec::new();
    if let Some(u) = crate::hardening::registered_instance_uuid(cli, CONTROL) {
        uuids.push(u);
    }
    uuids.push(uuid::Uuid::now_v7().to_string());

    let mut last = String::from("no uuid attempted");
    for uuid in uuids {
        let mut ws = connect_engine(cli).await?;
        let req = make_container(
            CONTROL,
            &uuid,
            "",
            EnginePayload::ConnectionRequest(ConnectionRequest {
                pin: cli.pin,
                process_position: ProcessPosition::Connection as i32,
                priority: 1,
                module_instance_uuid7: uuid.clone(),
            }),
        );
        send_container(&mut ws, &req).await?;
        match receive_container(&mut ws, 5000).await {
            Ok(c) if !c.auth_token.is_empty() => {
                // Clear the post-auth drain window before the first query.
                tokio::time::sleep(Duration::from_millis(300)).await;
                return Ok((ws, c.auth_token, uuid));
            }
            Ok(_) => {
                last = format!("uuid {} rejected (empty token)", uuid);
                continue;
            }
            Err(e) => {
                last = format!("uuid {}: {}", uuid, e);
                continue;
            }
        }
    }
    Err(format!("control-surface auth failed: {}", last))
}

// ── query helper ────────────────────────────────────────────────────

/// Send a legacy `DatabaseQuery` and return the parsed JSON result. Skips log
/// broadcasts and answers the engine's liveness probes so the session survives.
async fn query(
    ws: &mut WsStream,
    auth: &str,
    uuid: &str,
    query_id: &str,
    sql: &str,
) -> Result<serde_json::Value, String> {
    let req = make_container(
        CONTROL,
        uuid,
        auth,
        EnginePayload::DatabaseQuery(DatabaseQuery {
            query_id: query_id.to_string(),
            sql: sql.to_string(),
            params: vec![],
        }),
    );
    send_container(ws, &req).await?;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            return Err(format!("{}: timeout waiting for result", query_id));
        }
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(WsMessage::Binary(data)))) => {
                let Ok(c) = ContainerForModule::decode(data.as_ref()) else { continue };
                match c.payload {
                    Some(ModulePayload::DatabaseQueryResult(res)) if res.query_id == query_id => {
                        if !res.success {
                            return Err(format!("{}: engine refused: {}", query_id, res.error));
                        }
                        return serde_json::from_slice(&res.result_blob)
                            .map_err(|e| format!("{}: non-JSON result: {}", query_id, e));
                    }
                    Some(ModulePayload::AuthVerify(_)) => {
                        let _ = send_container(
                            ws,
                            &make_container(
                                CONTROL,
                                uuid,
                                auth,
                                EnginePayload::AuthVerify(AuthVerify {
                                    cur_auth: auth.to_string(),
                                }),
                            ),
                        )
                        .await;
                    }
                    _ => {}
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(format!("{}: ws error: {}", query_id, e)),
            Ok(None) => return Err(format!("{}: stream ended", query_id)),
            Err(_) => {}
        }
    }
}

// ── e1: query surface ───────────────────────────────────────────────

async fn query_surface(ws: &mut WsStream, auth: &str, uuid: &str, cli: &Cli) -> Result<(), String> {
    // db_status — pause/block state + backup + size.
    let st = query(ws, auth, uuid, "db_status", "SELECT 1").await?;
    for key in ["pipeline_paused", "timeline_backup", "resume_when_unblocked"] {
        if st.get(key).is_none() {
            return Err(format!("db_status missing '{}': {}", key, st));
        }
    }
    if !st["pipeline_paused"].is_boolean() {
        return Err(format!("db_status.pipeline_paused is not a bool: {}", st));
    }
    if st.get("blocked_reason").is_none() {
        return Err(format!("db_status missing 'blocked_reason': {}", st));
    }
    if st.get("timeline_db_size_bytes").and_then(|v| v.as_u64()).is_none() {
        return Err(format!("db_status missing numeric 'timeline_db_size_bytes': {}", st));
    }

    // engine_info — port + PIN (control surface only).
    let ei = query(ws, auth, uuid, "engine_info", "SELECT 1").await?;
    if ei["port"].as_u64() != Some(cli.port as u64) {
        return Err(format!("engine_info.port != {} : {}", cli.port, ei));
    }
    if ei.get("pin").map(|p| p.is_null()).unwrap_or(true) {
        return Err(format!("engine_info must expose the PIN to the control surface: {}", ei));
    }

    // module_list — array with the gate columns (authority/price/min_rank).
    let ml = query(ws, auth, uuid, "module_list", "SELECT 1").await?;
    let arr = ml.as_array().ok_or_else(|| format!("module_list is not an array: {}", ml))?;
    let banned = arr
        .iter()
        .find(|e| e["name"] == "banned-words")
        .ok_or_else(|| format!("module_list is missing the seeded 'banned-words': {}", ml))?;
    for key in ["authority", "price", "min_rank"] {
        if banned.get(key).is_none() {
            return Err(format!("module_list entry missing gate column '{}': {}", key, banned));
        }
    }
    if banned["min_rank"].as_f64().is_none() {
        return Err(format!("module_list.min_rank is not numeric: {}", banned));
    }

    // stats — aggregates object (control surface only).
    let stats = query(ws, auth, uuid, "stats", "{}").await?;
    if !stats.is_object() || stats.get("total_messages").is_none() {
        return Err(format!("stats is not the expected aggregate object: {}", stats));
    }

    // channel_viewers — public object.
    let cv = query(ws, auth, uuid, "channel_viewers", "{}").await?;
    if !cv.is_object() {
        return Err(format!("channel_viewers is not an object: {}", cv));
    }

    Ok(())
}

// ── e2: user-db lifecycle ───────────────────────────────────────────

async fn userdb_lifecycle(ws: &mut WsStream, auth: &str, uuid: &str) -> Result<(), String> {
    let username = format!("e2e-user-{}", uuid::Uuid::now_v7());

    let add = query(
        ws,
        auth,
        uuid,
        "userdb_add_user",
        &serde_json::json!({
            "username": username,
            "channel": { "platform": "twitch", "channel_id": "e2e-chan", "handle": "e2e" }
        })
        .to_string(),
    )
    .await?;
    if add["success"] != true {
        return Err(format!("userdb_add_user failed: {}", add));
    }
    let user_uuid = add["user"]["uuid7"]
        .as_str()
        .ok_or_else(|| format!("userdb_add_user returned no uuid7: {}", add))?
        .to_string();

    let get = query(ws, auth, uuid, "userdb_get_user", &serde_json::json!({ "uuid7": user_uuid }).to_string()).await?;
    if get["success"] != true || get["user"]["uuid7"].as_str() != Some(user_uuid.as_str()) {
        return Err(format!("userdb_get_user mismatch: {}", get));
    }

    let add_score = query(
        ws,
        auth,
        uuid,
        "userdb_add_score",
        &serde_json::json!({ "uuid7": user_uuid, "delta": 5, "reason": "e2e" }).to_string(),
    )
    .await?;
    if add_score["success"] != true {
        return Err(format!("userdb_add_score failed: {}", add_score));
    }
    let get2 = query(ws, auth, uuid, "userdb_get_user", &serde_json::json!({ "uuid7": user_uuid }).to_string()).await?;
    if get2["user"]["score"].as_i64() != Some(5) {
        return Err(format!("score did not persist (expected 5): {}", get2));
    }

    let write = query(
        ws,
        auth,
        uuid,
        "userdb_write_user_value",
        &serde_json::json!({ "uuid7": user_uuid, "key": "e2e", "value": "hello" }).to_string(),
    )
    .await?;
    if write["success"] != true {
        return Err(format!("userdb_write_user_value failed: {}", write));
    }
    let list = query(ws, auth, uuid, "userdb_list_user_values", &serde_json::json!({ "uuid7": user_uuid }).to_string()).await?;
    let has_value = list["values"]
        .as_array()
        .map(|a| a.iter().any(|v| v["key"] == "e2e" && v["value"] == "hello"))
        .unwrap_or(false);
    if !has_value {
        return Err(format!("user value not listed after write: {}", list));
    }

    let list_users = query(ws, auth, uuid, "userdb_list_users", &serde_json::json!({ "limit": 500 }).to_string()).await?;
    let in_list = list_users["users"]
        .as_array()
        .map(|a| a.iter().any(|u| u["uuid7"].as_str() == Some(user_uuid.as_str())))
        .unwrap_or(false);
    if !in_list {
        return Err(format!("added user missing from userdb_list_users: {}", list_users));
    }

    // Loopback actor → owner perm, so the delete is allowed.
    let del = query(
        ws,
        auth,
        uuid,
        "userdb_delete_user",
        &serde_json::json!({ "uuid7": user_uuid, "actor_uuid7": user_uuid, "actor_role": "owner" }).to_string(),
    )
    .await?;
    if del["success"] != true {
        return Err(format!("userdb_delete_user failed: {}", del));
    }

    Ok(())
}

// ── e3/e4: the pipeline ─────────────────────────────────────────────

/// Send an adapter-style `MessagePreProcess` (empty uuid7) whose raw text
/// STARTS WITH `text`, then poll the timeline until that row reaches `complete`.
/// A connected stage module that withholds its ack makes the stage time out and
/// the row fail, so `complete` is proof every recipient processed it.
async fn pipeline_roundtrip(
    ws: &mut WsStream,
    auth: &str,
    uuid: &str,
    text: &str,
) -> Result<(), String> {
    let chat = ChatMessage {
        platform: "e2e".into(),
        raw_data: vec![],
        raw_message: text.into(),
        user_uuid7: "e2e-user".into(),
        command: None,
        channel_id: "e2e-chan".into(),
        user_data: None,
    };
    let pre = make_container(
        CONTROL,
        uuid,
        auth,
        EnginePayload::MessagePreProcess(MessagePreProcess {
            message_uuid7: String::new(),
            raw_message: Some(chat),
            audio: vec![],
            audio_type: String::new(),
        }),
    );
    send_container(ws, &pre).await?;

    let ev = wait_for_status(ws, auth, uuid, text, "complete", Duration::from_secs(10))
        .await
        .ok_or_else(|| format!("'{}' never reached 'complete'", text))?;
    if !ev.error_message.is_empty() {
        return Err(format!("'{}' completed with error_message='{}'", text, ev.error_message));
    }
    if ev.processed_message.is_empty() {
        return Err(format!("'{}' completed but processed_message is empty", text));
    }
    Ok(())
}

// ── e7: stream leaderboard ──────────────────────────────────────────

/// The `stream-leaderboard` module reports on a ~2s cadence. The messages
/// injected by e3/e4 carry `user_uuid7 = "e2e-user"`, which the engine
/// auto-creates in the user DB and enriches with a score; the module should
/// therefore list that user. We assert the shape (`entries` array, numeric
/// `earned`/`score`), not a specific point total — no scoring module runs here,
/// so the earned delta is legitimately 0.
async fn leaderboard(ws: &mut WsStream, auth: &str, uuid: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = serde_json::json!(null);
    while Instant::now() < deadline {
        let lb = query(ws, auth, uuid, "stream_leaderboard", "{}").await?;
        let entries = lb
            .get("entries")
            .and_then(|e| e.as_array())
            .ok_or_else(|| format!("stream_leaderboard missing 'entries': {}", lb))?;
        if let Some(e) = entries.iter().find(|e| e["username"] == "e2e-user") {
            for key in ["uuid", "username", "earned", "score"] {
                if e.get(key).is_none() {
                    return Err(format!("leaderboard entry missing '{}': {}", key, e));
                }
            }
            if e["earned"].as_i64().is_none() {
                return Err(format!("leaderboard entry 'earned' is not numeric: {}", e));
            }
            return Ok(());
        }
        last = lb;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!(
        "stream_leaderboard never listed the injected chatter 'e2e-user': {}",
        last
    ))
}

// ── e8: stream manager ──────────────────────────────────────────────

/// The stream-manager loop: the control surface writes a `stream_control`, the
/// engine stores it (bumping the revision), and the `stream-manager-stub`
/// adapter polls `stream_schedule`, "applies" the revision and reports a status
/// back — which the same `stream_schedule` read then exposes.
///
/// The control written here is deliberately EMPTY. The doctor may launch real
/// adapters with real platform credentials (a module dir with a filled-in
/// `config.json`), and a non-empty title would make one of them write to a live
/// stream. An empty control still bumps the revision (so the poll/apply/report
/// loop is exercised) but every adapter applies nothing.
async fn stream_control(ws: &mut WsStream, auth: &str, uuid: &str) -> Result<(), String> {
    let written = query(
        ws,
        auth,
        uuid,
        "stream_control",
        &serde_json::json!({
            "title": "",
            "scheduled_start_ms": 0,
            "thumbnail_path": "",
        })
        .to_string(),
    )
    .await?;
    let revision = written["revision"].as_u64().unwrap_or(0);
    if revision == 0 {
        return Err(format!("stream_control did not bump the revision: {}", written));
    }

    // The stub polls every ~3s; wait (bounded) for its status to appear.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = serde_json::json!(null);
    while Instant::now() < deadline {
        let s = query(ws, auth, uuid, "stream_schedule", "{}").await?;
        if let Some(st) = s["statuses"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["platform"] == "stub"))
        {
            if st["revision"].as_u64() == Some(revision) && st["ok"] == true {
                return Ok(());
            }
        }
        last = s;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!(
        "the stub adapter never reported applying revision {}: {}",
        revision, last
    ))
}

// ── e5: pause / resume ──────────────────────────────────────────────

async fn pause_resume(ws: &mut WsStream, auth: &str, uuid: &str, nonce: &str) -> Result<(), String> {
    // Pause.
    let paused = query(
        ws,
        auth,
        uuid,
        "pipeline_set_paused",
        &serde_json::json!({ "paused": true }).to_string(),
    )
    .await?;
    if paused.get("blocked_reason").and_then(|v| v.as_str()).is_some() {
        return Err(format!("engine is BLOCKED, cannot test pause/resume: {}", paused));
    }

    // Ingest a message while held.
    let text = format!("e2e paused {}", nonce);
    let chat = ChatMessage {
        platform: "e2e".into(),
        raw_data: vec![],
        raw_message: text.clone(),
        user_uuid7: "e2e-user".into(),
        command: None,
        channel_id: "e2e-chan".into(),
        user_data: None,
    };
    let pre = make_container(
        CONTROL,
        uuid,
        auth,
        EnginePayload::MessagePreProcess(MessagePreProcess {
            message_uuid7: String::new(),
            raw_message: Some(chat),
            audio: vec![],
            audio_type: String::new(),
        }),
    );
    send_container(ws, &pre).await?;

    // It must be persisted but NOT dispatched.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    if let Some(ev) = find_event(ws, auth, uuid, &text).await {
        if ev.pipeline_status == "complete" {
            return Err(format!("'{}' completed while the pipeline was paused", text));
        }
    } else {
        return Err(format!("'{}' was not persisted while paused", text));
    }

    // Resume and drive it to completion.
    let resumed = query(
        ws,
        auth,
        uuid,
        "pipeline_set_paused",
        &serde_json::json!({ "paused": false }).to_string(),
    )
    .await?;
    if resumed["paused"] != false {
        return Err(format!("resume did not clear the pause: {}", resumed));
    }
    let ev = wait_for_status(ws, auth, uuid, &text, "complete", Duration::from_secs(10))
        .await
        .ok_or_else(|| format!("'{}' did not complete after resume", text))?;
    if !ev.error_message.is_empty() {
        return Err(format!("'{}' failed after resume: {}", text, ev.error_message));
    }
    Ok(())
}

// ── e6: engine-shutdown gate ────────────────────────────────────────

/// `engine_shutdown` must be REFUSED while `shutdown_on_request` is false (the
/// default, and what `doctor`'s sandbox config uses), and the engine must still
/// be serving afterwards. A control-surface caller is authorised, so this pins
/// the CONFIG gate, not the auth gate.
async fn shutdown_gate(ws: &mut WsStream, auth: &str, uuid: &str) -> Result<(), String> {
    match query(ws, auth, uuid, "engine_shutdown", "{}").await {
        Ok(v) => {
            return Err(format!(
                "engine_shutdown was ACCEPTED but shutdown_on_request defaults to false: {}",
                v
            ))
        }
        Err(e) => {
            if !e.contains("shutdown_on_request is false") {
                return Err(format!("unexpected engine_shutdown refusal: {}", e));
            }
        }
    }
    // Still alive after the refusal.
    let st = query(ws, auth, uuid, "db_status", "SELECT 1").await?;
    if st.get("pipeline_paused").is_none() {
        return Err("engine did not survive the refused shutdown".into());
    }
    Ok(())
}

// ── e7: reconnect ───────────────────────────────────────────────────

async fn reconnect(cli: &Cli, auth: &str, uuid: &str) -> Result<(), String> {
    let mut ws = connect_engine(cli).await?;
    let req = make_container(
        CONTROL,
        uuid,
        auth,
        EnginePayload::ConnectionRequest(ConnectionRequest {
            pin: 0,
            process_position: ProcessPosition::Connection as i32,
            priority: 1,
            module_instance_uuid7: uuid.to_string(),
        }),
    );
    send_container(&mut ws, &req).await?;
    let resp = receive_container(&mut ws, 5000).await?;
    if resp.auth_token.is_empty() {
        return Err("JWT reconnect was rejected (empty token)".into());
    }
    // Clear the reconnect's post-auth drain window before querying.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let st = query(&mut ws, &resp.auth_token, uuid, "db_status", "SELECT 1").await?;
    if st.get("pipeline_paused").is_none() {
        return Err("a query on the reconnected socket failed".into());
    }
    let _ = ws.close(None).await;
    Ok(())
}

// ── e7: engine log cleanliness ──────────────────────────────────────

/// The engine's log path. `doctor` sets `COCKATIEL_ENGINE_LOG` (its sandbox
/// redirects the engine's stdout/stderr to `<sandbox>/engine.log`, a sibling of
/// the engine dir). Fall back to the two conventional locations.
fn engine_log_path(cli: &Cli) -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("COCKATIEL_ENGINE_LOG").filter(|v| !v.is_empty()) {
        return std::path::PathBuf::from(p);
    }
    let dir = crate::paths::engine_dir(cli);
    let direct = dir.join("engine.log");
    if direct.exists() {
        return direct;
    }
    dir.parent()
        .map(|p| p.join("engine.log"))
        .filter(|p| p.exists())
        .unwrap_or(direct)
}

fn engine_log_clean(path: &std::path::Path, start: u64) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let start = (start as usize).min(bytes.len());
    let window = String::from_utf8_lossy(&bytes[start..]);
    let bad: Vec<&str> = window
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && BAD_LOG_LINES.iter().any(|b| l.contains(b)))
        .collect();
    if !bad.is_empty() {
        return Err(format!("engine log has {} fault line(s): {}", bad.len(), bad.join(" | ")));
    }
    Ok(())
}

// ── shared helpers ──────────────────────────────────────────────────

/// Discover the launchable modules in the modules dir (same eligibility as the
/// probe suite: skip core components, terminal/UI modules, and modules whose
/// required credentials are unconfigured).
fn discover(cli: &Cli) -> Vec<(std::path::PathBuf, serde_json::Value, String)> {
    let dir = crate::paths::modules_dir(cli);
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.path();
        let manifest_path = path.join("cockatiel_module_info.json");
        let Ok(data) = std::fs::read_to_string(&manifest_path) else { continue };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&data) else { continue };
        let name = manifest.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        if let Some(kind) = manifest.get("kind").and_then(|v| v.as_str()) {
            if !kind.is_empty() && kind != "module" {
                continue;
            }
        }
        if manifest.get("terminal").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        if crate::module_missing_required_credentials(&path, &manifest) {
            continue;
        }
        out.push((path, manifest, name));
    }
    out
}

/// Wait (bounded) for `want` launched modules to report connected.
async fn wait_for_connected(
    ws: &mut WsStream,
    auth: &str,
    uuid: &str,
    want: usize,
    timeout: Duration,
) -> usize {
    if want == 0 {
        return 0;
    }
    let deadline = Instant::now() + timeout;
    let mut last = 0;
    while Instant::now() < deadline {
        last = connected_module_names(ws, auth, uuid).await.len();
        if last >= want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    last
}

/// The names of live, connected modules, read through the CONTROL connection.
///
/// Deliberately NOT `screening::connected_modules`: that helper hardcodes the
/// `cockatiel-test-runner` module name, and reusing it on the `cockatiel-tui`
/// session makes the engine sever the connection (the token is bound to the
/// name). This builds the same query with the control name.
async fn connected_module_names(ws: &mut WsStream, auth: &str, uuid: &str) -> Vec<String> {
    let Ok(ml) = query(ws, auth, uuid, "module_list", "SELECT 1").await else {
        return Vec::new();
    };
    ml.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let name = e["name"].as_str()?;
                    let connected = e["connected_at"].as_i64().unwrap_or(0) > 0;
                    let alive = e["alive"].as_bool().unwrap_or(false);
                    let shutdown = e["shutdown_at"].as_i64().unwrap_or(0) > 0;
                    let is_control = matches!(
                        name,
                        "cockatiel-test-runner" | "cockatiel-tui" | "cockatiel-tui-child"
                    );
                    (connected && alive && !shutdown && !is_control).then(|| name.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Run a typed timeline read through the CONTROL connection (same name-binding
/// reasoning as [`connected_module_names`]).
async fn timeline_events(
    ws: &mut WsStream,
    auth: &str,
    uuid: &str,
    prefix: &str,
) -> Vec<TimelineEvent> {
    let request_id = uuid::Uuid::now_v7().to_string();
    let q = make_container(
        CONTROL,
        uuid,
        auth,
        EnginePayload::TimelineQuery(TimelineQuery {
            timeline_id_uuid7: String::new(),
            request_id: request_id.clone(),
            event_type: 1,
            platform: "e2e".into(),
            user_uuid7: String::new(),
            kind: String::new(),
            since_ms: 0,
            raw_prefix: prefix.to_string(),
            pipeline_status: String::new(),
            limit: 20,
            offset: 0,
        }),
    );
    if send_container(ws, &q).await.is_err() {
        return Vec::new();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(Some(Ok(WsMessage::Binary(data)))) =
            tokio::time::timeout(Duration::from_millis(500), ws.next()).await
        {
            let Ok(c) = ContainerForModule::decode(data.as_ref()) else { continue };
            match c.payload {
                Some(ModulePayload::TimelineQueryResult(res)) if res.request_id == request_id => {
                    return res.events;
                }
                Some(ModulePayload::AuthVerify(_)) => {
                    let _ = send_container(
                        ws,
                        &make_container(
                            CONTROL,
                            uuid,
                            auth,
                            EnginePayload::AuthVerify(AuthVerify {
                                cur_auth: auth.to_string(),
                            }),
                        ),
                    )
                    .await;
                }
                _ => {}
            }
        }
    }
    Vec::new()
}

/// Find the (single) timeline event whose raw_message starts with `prefix`.
async fn find_event(ws: &mut WsStream, auth: &str, uuid: &str, prefix: &str) -> Option<TimelineEvent> {
    timeline_events(ws, auth, uuid, prefix)
        .await
        .into_iter()
        .find(|e| e.raw_message.starts_with(prefix))
}

/// Poll until the row with `prefix` reaches `status`, returning it.
async fn wait_for_status(
    ws: &mut WsStream,
    auth: &str,
    uuid: &str,
    prefix: &str,
    status: &str,
    timeout: Duration,
) -> Option<TimelineEvent> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(ev) = find_event(ws, auth, uuid, prefix).await {
            if ev.pipeline_status == status {
                return Some(ev);
            }
            if ev.pipeline_status == "failed" || ev.pipeline_status == "dropped" {
                return Some(ev);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}
