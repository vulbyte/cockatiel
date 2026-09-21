use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use cockatiel_client::proto::{container::Payload, *};

mod fake_engine;
mod metrics;

use metrics::Metrics;


// ── CLI (hand-rolled) ─────────────────────────────────────────────

struct Cli {
    suite: String, // "chain" | "modules" | "all"
    module: Option<String>,
    iterations: u64,
    json: bool,
    ip: String,
    port: u16,
    pin: i32,
}

fn print_help() {
    println!(
        r#"cockatiel-test-runner — compliance & benchmark suite

USAGE:
  cockatiel-test-runner [OPTIONS]

OPTIONS:
  --suite <name>       "chain" | "modules" | "all"   (default: all)
  --module <name>      run only this module (runtime probe)
  --iterations <n>     messages per test burst        (default: 100)
  --json               output machine-readable JSON summary
  --help               show this help
"#
    );
}

fn parse_args(args: &[String]) -> Cli {
    let mut cli = Cli {
        suite: "all".to_string(),
        module: None,
        iterations: 100,
        json: false,
        ip: "127.0.0.1".to_string(),
        port: 9734,
        pin: 0,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--suite" => {
                if let Some(v) = args.get(i + 1) {
                    cli.suite = v.clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--module" => {
                if let Some(v) = args.get(i + 1) {
                    cli.module = Some(v.clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--iterations" => {
                if let Some(v) = args.get(i + 1) {
                    cli.iterations = v.parse().unwrap_or(100);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--json" => {
                cli.json = true;
                i += 1;
            }
            "--ip" | "-i" => {
                if let Some(v) = args.get(i + 1) {
                    cli.ip = v.clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--port" | "-p" => {
                if let Some(v) = args.get(i + 1) {
                    cli.port = v.parse().unwrap_or(9734);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--pin" => {
                if let Some(v) = args.get(i + 1) {
                    cli.pin = v.parse().unwrap_or(0);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    cli
}

// ── Shared WS client helpers (same pattern as stress-test) ─────────

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_engine(cli: &Cli) -> Result<WsStream, String> {
    let url = format!("ws://{}:{}", cli.ip, cli.port);
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| format!("connect: {}", e))?;
    Ok(ws)
}

async fn send_container(ws: &mut WsStream, c: &Container) -> Result<(), String> {
    let mut buf = Vec::new();
    c.encode(&mut buf).map_err(|e| e.to_string())?;
    ws.send(WsMessage::Binary(buf.into()))
        .await
        .map_err(|e| format!("send: {}", e))
}

async fn receive_container(ws: &mut WsStream, timeout_ms: u64) -> Result<Container, String> {
    let r = tokio::time::timeout(Duration::from_millis(timeout_ms), ws.next()).await;
    match r {
        Ok(Some(Ok(WsMessage::Binary(data)))) => Container::decode(data.as_ref()).map_err(|e| e.to_string()),
        Ok(Some(Ok(WsMessage::Close(_)))) => Err("closed".into()),
        Ok(Some(Ok(_))) => Err("non-binary".into()),
        Ok(Some(Err(e))) => Err(format!("ws err: {}", e)),
        Ok(None) => Err("stream ended".into()),
        Err(_) => Err("timeout".into()),
    }
}

fn make_container(module: &str, uuid: &str, auth: &str, payload: Payload) -> Container {
    Container {
        version: 1,
        auth_token: auth.to_string(),
        module_name: module.to_string(),
        module_instance_uuid7: uuid.to_string(),
        payload: Some(payload),
    }
}

// ── Mode A: chain verification (real engine) ────────────────────────

async fn run_chain_suite(cli: &Cli) -> Vec<Metrics> {
    println!("[chain] verifying pre→in→post through the real engine...");
    let mut m = Metrics::new("chain");
    let uuid = uuid::Uuid::now_v7().to_string();

    let mut ws = match connect_engine(cli).await {
        Ok(w) => w,
        Err(e) => {
            m.failed += 1;
            m.notes.push(format!("engine unreachable: {}", e));
            return vec![m];
        }
    };

    // Authenticate as the test runner (auto-approved via modules.json).
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
    if send_container(&mut ws, &req).await.is_err() {
        m.failed += 1;
        m.notes.push("failed to send ConnectionRequest".into());
        return vec![m];
    }
    let auth = match receive_container(&mut ws, 5000).await {
        Ok(c) => c.auth_token,
        Err(e) => {
            m.failed += 1;
            m.notes.push(format!("auth failed: {}", e));
            return vec![m];
        }
    };

    // Send N fake messages, then verify each was ingested by querying the
    // timeline for its row. The engine does not echo a response per message;
    // the DB row (status 'processing'/'complete') is the proof.
    let mut latencies = Vec::new();
    let mut successes = 0u32;
    for i in 0..cli.iterations {
        let chat = ChatMessage {
            platform: "test".into(),
            raw_data: vec![],
            raw_message: format!("chain message {}", i),
            user_uuid7: String::new(),
            command: None,
            user_data: None,
        };
        // Ingest like an adapter: EMPTY message_uuid7 so the engine assigns the
        // row uuid and starts the pipeline (a non-empty uuid would be treated as
        // a pre-process reply and never ingested).
        let pre = make_container(
            "cockatiel-test-runner",
            &uuid,
            &auth,
            Payload::MessagePreProcess(MessagePreProcess {
                message_uuid7: String::new(),
                raw_message: Some(chat),
                audio: vec![],
                audio_type: String::new(),
            }),
        );
        let start = std::time::Instant::now();
        if send_container(&mut ws, &pre).await.is_err() {
            m.failed += 1;
            break;
        }

        // Give the engine a moment to ingest, then check the row exists by its
        // unique content (the engine owns the uuid).
        tokio::time::sleep(Duration::from_millis(60)).await;
        let qid = format!("chain_check_{}", i);
        let check = make_container(
            "cockatiel-test-runner",
            &uuid,
            &auth,
            Payload::DatabaseQuery(DatabaseQuery {
                query_id: qid.clone(),
                sql: format!(
                    "SELECT pipeline_status FROM timeline_events WHERE platform = 'test' AND raw_message = 'chain message {}'",
                    i
                ),
                params: vec![],
            }),
        );
        if send_container(&mut ws, &check).await.is_err() {
            m.failed += 1;
            break;
        }
        // Read until we get the matching query response (skip any strays).
        let verified = {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            let mut ok = false;
            while tokio::time::Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                    Ok(Some(Ok(WsMessage::Binary(data)))) => {
                        if let Ok(c) = Container::decode(data.as_ref()) {
                            if let Some(Payload::DatabaseQueryResult(res)) = c.payload {
                                if res.query_id != qid {
                                    continue;
                                }
                                let has_row = match serde_json::from_slice::<serde_json::Value>(&res.result_blob) {
                                Ok(v) => v.as_array().map(|arr| !arr.is_empty()).unwrap_or(false),
                                Err(_) => false,
                            };
                                ok = res.success && has_row;
                                break;
                            }
                        }
                    }
                    Ok(Some(Ok(_))) => {}
                    _ => break,
                }
            }
            ok
        };
        if verified {
            successes += 1;
        } else {
            m.failed += 1;
            m.notes.push(format!("msg {} not ingested", i));
        }
        latencies.push(start.elapsed());
    }

    m.total_msgs = successes as u64;
    m.passed = successes;
    let sum_ms: u128 = latencies.iter().map(|d| d.as_millis()).sum();
    m.duration_ms = sum_ms / (cli.iterations.max(1) as u128);
    m.record(&latencies);
    m.finalize();
    m.notes.push("mode A: engine ingested + responded".into());

    let _ = ws.close(None).await;
    vec![m]
}

// ── Mode B: fake-engine benchmark (per module) ──────────────────────

/// True when a module declares non-optional credentials and NONE of them are
/// present (non-empty) in its saved config — such modules would stall waiting
/// for setup. If at least one required credential is configured, we run it.
fn module_missing_required_credentials(dir: &std::path::Path, manifest: &serde_json::Value) -> bool {
    let Some(creds) = manifest.get("credentials").and_then(|v| v.as_array()) else {
        return false;
    };
    let required: Vec<&serde_json::Value> = creds
        .iter()
        .filter(|f| !f.get("optional").and_then(|v| v.as_bool()).unwrap_or(false))
        .collect();
    if required.is_empty() {
        return false;
    }
    // Read the module's config.json — credential values live under
    // `module_specific`, but legacy modules put them at the top level.
    let mut values = serde_json::Map::new();
    if let Ok(data) = std::fs::read_to_string(dir.join("config.json")) {
        if let Ok(root) = serde_json::from_str::<serde_json::Value>(&data) {
            if let Some(spec) = root.get("module_specific").and_then(|v| v.as_object()) {
                values = spec.clone();
            } else if let Some(obj) = root.as_object() {
                values = obj.clone();
            }
        }
    }
    let mut any_present = false;
    for field in required {
        let key = field.get("key").and_then(|v| v.as_str()).unwrap_or("");
        if key.is_empty() {
            continue;
        }
        let present = match values.get(key) {
            Some(serde_json::Value::String(s)) => !s.is_empty(),
            Some(serde_json::Value::Array(arr)) => !arr.is_empty(),
            Some(_) => true,
            None => false,
        };
        if present {
            any_present = true;
        }
    }
    !any_present
}

async fn run_module_suite(cli: &Cli) -> Vec<Metrics> {
    // Discover module manifests from the repo root's modules/ directory.
    let runner_dir = std::env::current_dir().unwrap_or_default();
    let modules_dir = runner_dir
        .parent()
        .unwrap_or(&runner_dir)
        .join("modules");
    let mut results = Vec::new();

    let mut entries = match std::fs::read_dir(&modules_dir) {
        Ok(e) => e.flatten().collect::<Vec<_>>(),
        Err(e) => {
            eprintln!("[modules] cannot read {}: {}", modules_dir.display(), e);
            return results;
        }
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let dir = entry.path();
        let manifest_path = dir.join("cockatiel_module_info.json");
        if !manifest_path.exists() {
            continue;
        }
        let manifest: serde_json::Value = match std::fs::read_to_string(&manifest_path)
            .and_then(|s| serde_json::from_str(&s).map_err(std::io::Error::other))
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        let name = manifest.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        // Runtime probe: --module <name>
        if let Some(filter) = &cli.module {
            if filter != &name {
                continue;
            }
        }
        // Skip UI modules and modules whose non-optional credentials aren't
        // configured (they'd stall waiting for setup against the fake engine).
        if manifest.get("terminal").and_then(|v| v.as_bool()).unwrap_or(false) {
            println!("[modules] skipping '{}' (terminal/UI module)", name);
            continue;
        }
        if module_missing_required_credentials(&dir, &manifest) {
            println!("[modules] skipping '{}' (needs credentials not configured)", name);
            continue;
        }
        println!("[modules] benchmarking '{}'...", name);
        let m = benchmark_one_module(&dir, &manifest, &name, cli.iterations).await;
        results.push(m);
    }
    results
}

async fn benchmark_one_module(
    dir: &std::path::Path,
    manifest: &serde_json::Value,
    name: &str,
    iterations: u64,
) -> Metrics {
    let mut m = Metrics::new(format!("module:{}", name));
    m.total_msgs = iterations;

    // Build launch command from the manifest.
    let launch = manifest.get("launch_command").and_then(|v| v.as_str()).unwrap_or("");
    let mut launch_parts: Vec<String> = launch.split_whitespace().map(String::from).collect();
    let flags: Vec<String> = manifest
        .get("command_flags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|f| f.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if launch_parts.is_empty() {
        m.failed += 1;
        m.notes.push("no launch_command in manifest".into());
        return m;
    }
    let program = launch_parts.remove(0);

    // Start the fake engine first so we know the port.
    let (engine, listener) = match fake_engine::FakeEngine::bind().await {
        Ok((e, l)) => (e, l),
        Err(e) => {
            m.failed += 1;
            m.notes.push(format!("fake engine bind failed: {}", e));
            return m;
        }
    };
    let port = listener.local_addr().unwrap().port();
    let engine = std::sync::Arc::new(engine);
    let engine_clone = engine.clone();
    let name_owned = name.to_string();
    tokio::spawn(async move {
        engine_clone.run(listener, iterations, name_owned).await;
    });

    // Spawn the module pointing at the fake engine (accepts any auth).
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&launch_parts)
        .args(&flags)
        .arg("--")
        .arg("--ip")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--pin")
        .arg("0")
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            m.failed += 1;
            m.notes.push(format!("spawn failed: {}", e));
            return m;
        }
    };
    let child_pid = child.id();

    // Wait for the fake engine to finish the burst (session done or timeout).
    let saw_session = tokio::time::timeout(
        Duration::from_secs(4),
        engine.done.notified(),
    ).await.is_ok();
    if !saw_session {
        m.notes.push("module never connected to the fake engine (may be blocked on interactive input or missing config)".into());
        m.failed += 1;
    }

    // Kill the module (crash detection: if it already died, note it).
    let _ = std::process::Command::new("kill").arg(child_pid.to_string()).spawn();

    let em = engine.metrics.lock().await.clone();
    m.passed = em.passed;
    m.failed = m.failed.max(em.failed);
    m.duration_ms = em.duration_ms;
    m.req_per_sec = em.req_per_sec;
    m.msgs_per_min_projected = em.msgs_per_min_projected;
    m.latency_p50_ms = em.latency_p50_ms;
    m.latency_p95_ms = em.latency_p95_ms;
    m.latency_p99_ms = em.latency_p99_ms;
    for n in &em.notes {
        m.notes.push(n.clone());
    }
    m.finalize();
    m
}

// ── Timeline archival via engine test_archive (dedicated virtual query) ─

async fn archive_to_timeline(batch_uuid: &str, results: &[Metrics], ip: &str, port: u16, pin: i32) {
    let url = format!("ws://{}:{}", ip, port);
    let Ok((mut ws, _)) = tokio_tungstenite::connect_async(url).await else { return };
    let uuid = uuid::Uuid::now_v7().to_string();
    let req = make_container(
        "cockatiel-test-runner",
        &uuid,
        "",
        Payload::ConnectionRequest(ConnectionRequest {
            pin,
            process_position: ProcessPosition::Connection as i32,
            priority: 1,
            module_instance_uuid7: uuid.clone(),
        }),
    );
    let _ = send_container(&mut ws, &req).await;
    let Ok(auth_container) = receive_container(&mut ws, 5000).await else { return };
    let auth = auth_container.auth_token;

    // Send every metric in one test_archive query; the engine inserts the
    // archival rows via its own method (no raw INSERT allowed anymore).
    let entries: Vec<serde_json::Value> = results
        .iter()
        .map(|r| serde_json::json!({ "json": r.to_json().to_string() }))
        .collect();
    let payload = serde_json::json!({ "batch_uuid": batch_uuid, "entries": entries }).to_string();
    let _ = send_container(
        &mut ws,
        &make_container(
            "cockatiel-test-runner",
            &uuid,
            &auth,
            Payload::DatabaseQuery(DatabaseQuery {
                query_id: "test_archive".into(),
                sql: payload,
                params: vec![],
            }),
        ),
    )
    .await;
    let _ = receive_container(&mut ws, 5000).await;
    let _ = ws.close(None).await;
}

// ── Main ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = parse_args(&args);
    let batch_uuid = format!("test-{}", uuid::Uuid::now_v7().to_string());

    println!("test-runner batch: {}", batch_uuid);
    println!("suite: {} | iterations: {}", cli.suite, cli.iterations);

    let mut all: Vec<Metrics> = Vec::new();

    if cli.suite == "chain" || cli.suite == "all" {
        all.extend(run_chain_suite(&cli).await);
    }
    if cli.suite == "modules" || cli.suite == "all" {
        all.extend(run_module_suite(&cli).await);
    }

    // Summary
    let mut passed = 0u32;
    let mut failed = 0u32;
    for m in &all {
        passed += m.passed;
        failed += m.failed;
    }
    println!("\n{}", "=".repeat(60));
    println!("batch {}: {} passed, {} failed, {} suites", batch_uuid, passed, failed, all.len());
    for m in &all {
        println!(
            "  {:<28} req/s={:.1}  msgs/min~{:.0}  p50={:.1}ms  p95={:.1}ms  pass={} fail={}",
            m.name, m.req_per_sec, m.msgs_per_min_projected, m.latency_p50_ms, m.latency_p95_ms, m.passed, m.failed
        );
    }
    println!("{}", "=".repeat(60));

    if cli.json {
        let summary: Vec<serde_json::Value> = all.iter().map(|m| m.to_json()).collect();
        println!("__RESULT_JSON__{}", serde_json::to_string_pretty(&summary).unwrap_or_default());
    }

    // Archive every test to the timeline.
    archive_to_timeline(&batch_uuid, &all, &cli.ip, cli.port, cli.pin).await;
}