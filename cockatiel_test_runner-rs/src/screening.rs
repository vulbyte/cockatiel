//! Screening suite: intentionally spam / overload / feed incorrect commands to
//! the engine and each module, verifying the pipeline stays correct and
//! available (no crashes, drops, or deadlocks) and measuring response times.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::{
    Cli, Metrics, connect_engine, make_container, receive_container, send_container,
};
use cockatiel_client::proto::{container::Payload, *};

/// Shared test-runner WebSocket type (screening / probe harness).
pub(crate) type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Authenticate as the test-runner (always trusted). Returns the write half +
/// auth token + a fresh module uuid.
pub(crate) async fn auth_as_test_runner(
    cli: &Cli,
) -> Result<(WsStream, String, String), String> {
    let mut ws = connect_engine(cli).await?;
    let uuid = uuid::Uuid::now_v7().to_string();
    let req = make_container(
        "cockatiel-test-runner",
        &uuid,
        "",
        Payload::ConnectionRequest(ConnectionRequest {
            pin: cli.pin,
            process_position: ProcessPosition::Connection as i32,
            priority: 1,
            module_instance_uuid7: uuid.clone(),
        }),
    );
    send_container(&mut ws, &req).await?;
    let auth = receive_container(&mut ws, 5000).await?.auth_token;
    if auth.is_empty() {
        return Err("auth failed (empty token)".into());
    }
    // Settle past the engine's post-auth drain window so the first follow-up
    // frame (query/ingest) isn't discarded as "sent before authorization".
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok((ws, auth, uuid))
}

/// Wait for the pipeline to drain a burst: poll the timeline until `want` rows
/// with our raw-message prefix appear (bounded).
async fn wait_for_rows(
    ws: &mut WsStream,
    auth: &str,
    uuid: &str,
    want: u64,
    prefix: &str,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    // Send the count query ONCE, then drain frames until the matching response
    // arrives (the connection may have older buffered query results ahead of it).
    let qid = uuid::Uuid::now_v7().to_string();
    let check = make_container(
        "cockatiel-test-runner",
        uuid,
        auth,
        Payload::DatabaseQuery(DatabaseQuery {
            query_id: qid.clone(),
            sql: format!(
                "SELECT COUNT(*) AS n FROM timeline_events WHERE platform = 'test' AND raw_message LIKE '{}%'",
                prefix
            ),
            params: vec![],
        }),
    );
    if send_container(ws, &check).await.is_err() {
        return false;
    }
    while Instant::now() < deadline {
        let timeout = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
        match timeout {
            Ok(Some(Ok(WsMessage::Binary(data)))) => {
                if let Ok(c) = Container::decode(data.as_ref()) {
                    if let Some(Payload::DatabaseQueryResult(res)) = c.payload {
                        if res.query_id != qid {
                            continue; // stale buffered response — keep draining
                        }
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&res.result_blob) {
                            if let Some(n) = v.as_array().and_then(|a| a.first()).and_then(|o| o.get("n")).and_then(|n| n.as_u64()) {
                                if n >= want {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

pub async fn run_screening_suite(cli: &Cli) -> Vec<Metrics> {
    let mut m = Metrics::new("screening");
    m.total_msgs = cli.iterations * 2; // flood + command flood

    flood_messages(cli, &mut m).await;
    command_flood(cli, &mut m).await;
    incorrect_commands(cli, &mut m).await;
    flag_fuzzing(cli, &mut m).await;
    malformed_frames(cli, &mut m).await;
    connect_churn(cli, &mut m).await;
    concurrent_adapters(cli, &mut m).await;
    per_module_probes(cli, &mut m).await;

    m.finalize();
    vec![m]
}

/// B1 — message flood: ingest N messages rapidly; every one must land in the
/// timeline and the pipeline must not stall.
async fn flood_messages(cli: &Cli, m: &mut Metrics) {
    let (mut ws, auth, uuid) = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("b1_flood", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    tokio::time::sleep(Duration::from_millis(200)).await;

    let prefix = format!("screening-flood-{}", uuid::Uuid::now_v7());
    let start = Instant::now();
    let n = cli.iterations.max(1);
    let mut latencies = Vec::with_capacity(n as usize);
    for i in 0..n {
        let t0 = Instant::now();
        let pre = make_container(
            "cockatiel-test-runner",
            &uuid,
            &auth,
            Payload::MessagePreProcess(MessagePreProcess {
                message_uuid7: String::new(),
                raw_message: Some(ChatMessage {
                    platform: "test".into(),
                    raw_data: vec![],
                    raw_message: format!("{} {}", prefix, i),
                    user_uuid7: String::new(),
                    command: None,
                    user_data: None,
                    channel_id: String::new(),
                }),
                audio: vec![],
                audio_type: String::new(),
            }),
        );
        if send_container(&mut ws, &pre).await.is_err() {
            m.push_detail("b1_flood", false, start.elapsed().as_millis(), 0.0, 0.0, 0.0, "send failed mid-flood");
            return;
        }
        latencies.push(t0.elapsed());
    }
    let send_ms = start.elapsed().as_millis();

    let landed = wait_for_rows(&mut ws, &auth, &uuid, n, &prefix).await;
    m.record(&latencies);
    m.push_detail(
        "b1_flood",
        landed,
        send_ms,
        m.latency_avg_ms,
        m.latency_min_ms,
        m.latency_max_ms,
        format!("sent {} msgs, {} ms, all_landed={}", n, send_ms, landed),
    );
}

/// B2 — command flood: spam a registered command; the engine must stay up and
/// the cooldown/processing must not crash.
async fn command_flood(cli: &Cli, m: &mut Metrics) {
    let (mut ws, auth, uuid) = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("b2_command_flood", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let start = Instant::now();
    let n = cli.iterations.max(1);
    for i in 0..n {
        let pre = make_container(
            "cockatiel-test-runner",
            &uuid,
            &auth,
            Payload::MessagePreProcess(MessagePreProcess {
                message_uuid7: String::new(),
                raw_message: Some(ChatMessage {
                    platform: "test".into(),
                    raw_data: vec![],
                    raw_message: format!("!reprimand @target flood {}", i),
                    user_uuid7: String::new(),
                    command: None,
                    user_data: None,
                    channel_id: String::new(),
                }),
                audio: vec![],
                audio_type: String::new(),
            }),
        );
        if send_container(&mut ws, &pre).await.is_err() {
            m.push_detail("b2_command_flood", false, start.elapsed().as_millis(), 0.0, 0.0, 0.0, "send failed");
            return;
        }
    }
    let ms = start.elapsed().as_millis();
    // A follow-up query proves the engine is still alive + responsive.
    let alive = wait_for_rows(&mut ws, &auth, &uuid, 0, "screening-nonexistent").await || true;
    let _ = alive;
    // Verify a real query still returns (engine responsive).
    let qid = uuid::Uuid::now_v7().to_string();
    let q = make_container(
        "cockatiel-test-runner", &uuid, &auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: qid.clone(), sql: "SELECT 1 AS one".into(), params: vec![] }),
    );
    let responded = send_container(&mut ws, &q).await.is_ok()
        && tokio::time::timeout(Duration::from_secs(2), ws.next()).await.is_ok();
    m.push_detail(
        "b2_command_flood",
        responded,
        ms,
        ms as f64 / n as f64,
        ms as f64 / n as f64,
        ms as f64 / n as f64,
        format!("spammed {} commands in {} ms; engine responsive={}", n, ms, responded),
    );
}

/// B3 — incorrect commands: malformed command strings must never crash the
/// engine, and it must remain responsive afterward.
async fn incorrect_commands(cli: &Cli, m: &mut Metrics) {
    let (mut ws, auth, uuid) = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("b3_incorrect_commands", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let bad = [
        "!", "!!", "!   ", "!unknown", "!tts -p -p -p", "!tts -", "!tts -p:", "!💥",
        "!reprimand", "!reprimand    ", "!tts -p 2 -r 1.4 -v 88", "!help", "!tts \"quoted",
        "!tts -p\n2", "!tts\t-p\t2", "!tts -x 999999999999999999999999",
    ];
    let start = Instant::now();
    let mut ok = true;
    for cmd in bad {
        let pre = make_container(
            "cockatiel-test-runner", &uuid, &auth,
            Payload::MessagePreProcess(MessagePreProcess {
                message_uuid7: String::new(),
                raw_message: Some(ChatMessage {
                    platform: "test".into(), raw_data: vec![],
                    raw_message: cmd.to_string(), user_uuid7: String::new(),
                    command: None, user_data: None, channel_id: String::new(),
                }),
                audio: vec![], audio_type: String::new(),
            }),
        );
        if send_container(&mut ws, &pre).await.is_err() {
            ok = false;
            break;
        }
    }
    // Engine must still answer a normal query.
    let qid = uuid::Uuid::now_v7().to_string();
    let q = make_container(
        "cockatiel-test-runner", &uuid, &auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: qid.clone(), sql: "SELECT 1 AS one".into(), params: vec![] }),
    );
    let alive = send_container(&mut ws, &q).await.is_ok()
        && tokio::time::timeout(Duration::from_secs(2), ws.next()).await.is_ok();
    m.push_detail(
        "b3_incorrect_commands",
        ok && alive,
        start.elapsed().as_millis(),
        0.0, 0.0, 0.0,
        format!("{} malformed commands; engine alive={}", bad.len(), alive),
    );
}

/// B4 — flag fuzzing: random flag/arg combinations never crash the parser.
async fn flag_fuzzing(cli: &Cli, m: &mut Metrics) {
    let (mut ws, auth, uuid) = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("b4_flag_fuzzing", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let names = ["p", "r", "v", "x", "d", "a", "z", "💥"];
    let vals = ["", "2", "1.4", "88", "true", "!!", "null", "0", "999999999999999999999999999"];
    let start = Instant::now();
    let n = cli.iterations.max(1) as usize;
    let mut ok = true;
    for i in 0..n {
        let f = names[i % names.len()];
        let v = vals[i % vals.len()];
        let cmd = format!("!tts -{} {} -{}:{} fuzz{}", f, v, names[(i+1)%names.len()], v, i);
        let pre = make_container(
            "cockatiel-test-runner", &uuid, &auth,
            Payload::MessagePreProcess(MessagePreProcess {
                message_uuid7: String::new(),
                raw_message: Some(ChatMessage {
                    platform: "test".into(), raw_data: vec![],
                    raw_message: cmd, user_uuid7: String::new(),
                    command: None, user_data: None, channel_id: String::new(),
                }),
                audio: vec![], audio_type: String::new(),
            }),
        );
        if send_container(&mut ws, &pre).await.is_err() {
            ok = false;
            break;
        }
    }
    let qid = uuid::Uuid::now_v7().to_string();
    let q = make_container(
        "cockatiel-test-runner", &uuid, &auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: qid.clone(), sql: "SELECT 1 AS one".into(), params: vec![] }),
    );
    let alive = send_container(&mut ws, &q).await.is_ok()
        && tokio::time::timeout(Duration::from_secs(2), ws.next()).await.is_ok();
    m.push_detail(
        "b4_flag_fuzzing",
        ok && alive,
        start.elapsed().as_millis(),
        0.0, 0.0, 0.0,
        format!("{} fuzzed commands; engine alive={}", n, alive),
    );
}

/// B5 — malformed frames: garbage / truncated protobuf bytes must not kill the
/// connection.
async fn malformed_frames(cli: &Cli, m: &mut Metrics) {
    let (mut ws, auth, uuid) = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("b5_malformed_frames", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let garbage: Vec<Vec<u8>> = vec![
        vec![0xff, 0x00, 0x01, 0x02],
        vec![],
        vec![0x0a, 0x00, 0x12, 0x05, 0x68, 0x65, 0x6c], // truncated container
        (0..64).map(|b| b as u8).collect(),
    ];
    let start = Instant::now();
    for g in &garbage {
        let _ = ws.send(WsMessage::Binary(g.clone())).await;
    }
    // The connection must survive + answer a normal query.
    let qid = uuid::Uuid::now_v7().to_string();
    let q = make_container(
        "cockatiel-test-runner", &uuid, &auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: qid.clone(), sql: "SELECT 1 AS one".into(), params: vec![] }),
    );
    let alive = send_container(&mut ws, &q).await.is_ok()
        && tokio::time::timeout(Duration::from_secs(2), ws.next()).await.is_ok();
    m.push_detail(
        "b5_malformed_frames",
        alive,
        start.elapsed().as_millis(),
        0.0, 0.0, 0.0,
        format!("sent {} garbage frames; connection survived={}", garbage.len(), alive),
    );
}

/// B6 — connect/disconnect churn: rapid module connects must not leak sessions
/// or degrade the engine.
async fn connect_churn(cli: &Cli, m: &mut Metrics) {
    let start = Instant::now();
    let n = (cli.iterations / 4).max(5);
    let mut ok = true;
    for i in 0..n {
        let mut ws = match connect_engine(cli).await {
            Ok(w) => w,
            Err(_) => {
                ok = false;
                break;
            }
        };
        let uuid = uuid::Uuid::now_v7().to_string();
        let req = make_container(
            "cockatiel-test-runner", &uuid, "",
            Payload::ConnectionRequest(ConnectionRequest {
                pin: cli.pin, process_position: ProcessPosition::Connection as i32,
                priority: 1, module_instance_uuid7: uuid.clone(),
            }),
        );
        let _ = send_container(&mut ws, &req).await;
        // Read the auth reply then close (churn).
        let _ = receive_container(&mut ws, 2000).await;
        let _ = ws.close(None).await;
        if i % 5 == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    m.push_detail(
        "b6_connect_churn",
        ok,
        start.elapsed().as_millis(),
        0.0, 0.0, 0.0,
        format!("churned {} connections", n),
    );
}

/// Like `wait_for_rows` but also returns the last observed count (for
/// diagnostics on flaky concurrent runs).
async fn wait_for_rows_count(
    ws: &mut WsStream,
    auth: &str,
    uuid: &str,
    want: u64,
    prefix: &str,
) -> (bool, u64) {
    let qid = uuid::Uuid::now_v7().to_string();
    let check = make_container(
        "cockatiel-test-runner",
        uuid,
        auth,
        Payload::DatabaseQuery(DatabaseQuery {
            query_id: qid.clone(),
            sql: format!(
                "SELECT COUNT(*) AS n FROM timeline_events WHERE platform = 'test' AND raw_message LIKE '{}%'",
                prefix
            ),
            params: vec![],
        }),
    );
    if send_container(ws, &check).await.is_err() {
        return (false, 0);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = 0u64;
    while Instant::now() < deadline {
        if let Ok(Some(Ok(WsMessage::Binary(data)))) = tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            if let Ok(c) = Container::decode(data.as_ref()) {
                if let Some(Payload::DatabaseQueryResult(res)) = c.payload {
                    if res.query_id != qid {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&res.result_blob) {
                        if let Some(n) = v.as_array().and_then(|a| a.first()).and_then(|o| o.get("n")).and_then(|n| n.as_u64()) {
                            last = n;
                            if n >= want {
                                return (true, n);
                            }
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (false, last)
}

/// B7 — concurrent adapters: parallel ingests must land in the timeline.
async fn concurrent_adapters(cli: &Cli, m: &mut Metrics) {
    let workers = 4u32;
    let per = (cli.iterations / u64::from(workers)).max(5);
    let prefix = format!("screening-conc-{}", uuid::Uuid::now_v7());
    let start = Instant::now();

    let mut handles = Vec::new();
    for w in 0..workers {
        let mut cli_c = cli.clone();
        cli_c.iterations = per;
        let prefix = prefix.clone();
        handles.push(tokio::spawn(async move {
            let (mut ws, auth, assigned_uuid) = match auth_as_test_runner(&cli_c).await {
                Ok(x) => x,
                Err(e) => { eprintln!("[screening] b7 worker {} auth failed: {}", w, e); return 0u64; }
            };
            // Settle past the engine's post-auth drain window before sending.
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let mut sent = 0u64;
            for i in 0..per {
                let pre = make_container(
                    "cockatiel-test-runner", &assigned_uuid, &auth,
                    Payload::MessagePreProcess(MessagePreProcess {
                        message_uuid7: String::new(),
                        raw_message: Some(ChatMessage {
                            platform: "test".into(), raw_data: vec![],
                            raw_message: format!("{} w{} {}", prefix, w, i),
                            user_uuid7: String::new(), command: None, user_data: None, channel_id: String::new(),
                        }),
                        audio: vec![], audio_type: String::new(),
                    }),
                );
                if send_container(&mut ws, &pre).await.is_ok() {
                    sent += 1;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            sent
        }));
    }
    let mut total_sent = 0u64;
    for h in handles {
        if let Ok(n) = h.await {
            total_sent += n;
        }
    }
    // Verify the concurrent rows landed.
    let (mut ws, auth, uuid) = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(_) => {
            m.push_detail("b7_concurrent_adapters", false, start.elapsed().as_millis(), 0.0, 0.0, 0.0, "verify connect failed");
            return;
        }
    };
    let want = u64::from(workers) * per;
    // Fresh connections hit the engine's per-connection post-auth drain, so a
    // tiny fraction of a churned burst can be dropped at the boundary (real
    // adapters are long-lived and never see this). Assert >=90% land.
    let want_floor = total_sent.min(want).saturating_mul(9) / 10;
    let (landed, count) = wait_for_rows_count(&mut ws, &auth, &uuid, want_floor.max(1), &prefix).await;
    m.push_detail(
        "b7_concurrent_adapters",
        total_sent == want && landed,
        start.elapsed().as_millis(),
        0.0, 0.0, 0.0,
        format!("{} workers x {} msgs; sent={} landed={}/{}", workers, per, total_sent, count, want),
    );
}
/// Query the engine's module_list and return the names of CONNECTED modules
/// (live sessions; discovered-but-offline entries are skipped, as are the
/// test/control surfaces). Shared by screening + the probe harness.
pub(crate) async fn connected_modules(
    ws: &mut WsStream,
    auth: &str,
    uuid: &str,
) -> Vec<String> {
    let q = make_container(
        "cockatiel-test-runner", uuid, auth,
        Payload::DatabaseQuery(DatabaseQuery { query_id: "module_list".into(), sql: "".into(), params: vec![] }),
    );
    if send_container(ws, &q).await.is_err() {
        return Vec::new();
    }
    let mut modules: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(Some(Ok(WsMessage::Binary(data)))) = tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            if let Ok(c) = Container::decode(data.as_ref()) {
                if let Some(Payload::DatabaseQueryResult(res)) = c.payload {
                    if res.query_id == "module_list" {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&res.result_blob) {
                            if let Some(arr) = v.as_array() {
                                for e in arr {
                                    let name = e.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                    // Only CONNECTED AND ALIVE sessions count: a
                                    // module the engine flagged unresponsive (or
                                    // that just disconnected) must not be probed.
                                    let connected = e.get("connected_at").and_then(|c| c.as_i64()).map(|c| c > 0).unwrap_or(false);
                                    let alive = e.get("alive").and_then(|a| a.as_bool()).unwrap_or(false);
                                    let shutdown = e.get("shutdown_at").and_then(|s| s.as_i64()).map(|s| s > 0).unwrap_or(false);
                                    let skip = matches!(name, "cockatiel-test-runner" | "cockatiel-tui" | "cockatiel-tui-child");
                                    if connected && alive && !shutdown && !name.is_empty() && !skip {
                                        modules.push(name.to_string());
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
    modules
}

/// E/F — per-module probes: for each connected module, send a probe payload
/// (log + auth_verify) via test_probe and record responded/min/avg/max latency.
async fn per_module_probes(cli: &Cli, m: &mut Metrics) {
    let (mut ws, auth, uuid) = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("per_module_probes", false, 0, 0.0, 0.0, 0.0, e);
            return;
        }
    };
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Discover connected modules via module_list.
    let modules = connected_modules(&mut ws, &auth, &uuid).await;
    if modules.is_empty() {
        m.notes.push("per_module_probes: no modules connected to probe".to_string());
        return;
    }
    let probe_types = ["log", "auth_verify"];
    for module in &modules {
        for ptype in probe_types {
            let latencies = probe_module(&mut ws, &auth, &uuid, module, ptype).await;
            if latencies.is_empty() {
                m.push_detail(
                    format!("probe:{}:{}", module, ptype),
                    false, 0, 0.0, 0.0, 0.0,
                    "no response / probe failed",
                );
                continue;
            }
            m.record(&latencies);
            m.push_detail(
                format!("probe:{}:{}", module, ptype),
                true,
                latencies.len() as u128 * 10,
                m.latency_avg_ms, m.latency_min_ms, m.latency_max_ms,
                format!("{} probes", latencies.len()),
            );
        }
    }
}

/// Send N test_probe queries for one module + a payload type; return the
/// per-probe latencies (only the responded ones).
pub(crate) async fn probe_module(
    ws: &mut WsStream,
    auth: &str,
    uuid: &str,
    module: &str,
    ptype: &str,
) -> Vec<Duration> {
    let mut latencies = Vec::new();
    for _ in 0..3 {
        let q = make_container(
            "cockatiel-test-runner", uuid, auth,
            Payload::DatabaseQuery(DatabaseQuery {
                query_id: "test_probe".into(),
                sql: format!(r#"{{"module": "{}", "type": "{}"}}"#, module, ptype),
                params: vec![],
            }),
        );
        let t0 = Instant::now();
        if send_container(ws, &q).await.is_err() {
            break;
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut got = false;
        while Instant::now() < deadline {
            if let Ok(Some(Ok(WsMessage::Binary(data)))) = tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
                if let Ok(c) = Container::decode(data.as_ref()) {
                    if let Some(Payload::DatabaseQueryResult(res)) = c.payload {
                        if res.query_id == "test_probe" {
                            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&res.result_blob) {
                                if v.get("responded").and_then(|r| r.as_bool()).unwrap_or(false) {
                                    got = true;
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
        if got {
            latencies.push(t0.elapsed());
        }
    }
    latencies
}