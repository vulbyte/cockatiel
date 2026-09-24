//! Soak suite: every launched module must STAY connected to the real engine,
//! uninterrupted, for a continuous window (default 30s, `--duration-secs`).
//!
//! Each module's real binary is launched against the engine (terminal/UI
//! modules via a pseudo-TTY so crossterm works headless). The harness then
//! repeatedly — across the whole window — checks that the module is still in
//! the engine's live-session list AND still answers an `auth_verify` probe. A
//! module that drops off or goes silent at any point FAILS with how long it
//! stayed up. This is the regression guard for modules that connect then go
//! silent or get severed / flagged unresponsive (e.g. youtube-adapter held the
//! engine write lock during a prompt, blocking its AuthVerify reply until the
//! liveness probe killed it) — the kind of bug the one-shot suites can't see.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use prost::Message;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::{
    Cli, Metrics, make_container, module_missing_required_credentials, send_container,
    probe::{cleanup, launch_module, launch_module_pty},
    screening::{auth_as_test_runner, connected_modules, WsStream},
};
use cockatiel_client::proto::{container::Payload, *};

/// Default continuous window a module must survive without dropping.
pub const DEFAULT_SOAK_SECS: u64 = 30;

/// Modules that are DEV/benchmark harnesses rather than runtime consumers —
/// they don't hold a normal engine session, so a "stay connected" soak does
/// not apply (e.g. stress-test connects to send, not to consume).
const EXCLUDED_MODULES: &[&str] = &["stress-test"];

/// Send ONE `auth_verify` test_probe and return whether THAT module responded.
/// The response must carry the same module name — a late/stale response from a
/// previous probe would otherwise be misattributed (false pass/fail).
async fn auth_verify_ok(ws: &mut WsStream, auth: &str, uuid: &str, module: &str) -> bool {
    let q = make_container(
        "cockatiel-test-runner",
        uuid,
        auth,
        Payload::DatabaseQuery(DatabaseQuery {
            query_id: "test_probe".into(),
            sql: format!(r#"{{"module":"{}","type":"auth_verify"}}"#, module),
            params: vec![],
        }),
    );
    if send_container(ws, &q).await.is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if let Ok(Some(Ok(WsMessage::Binary(data)))) =
            tokio::time::timeout(Duration::from_millis(1500), ws.next()).await
        {
            if let Ok(c) = Container::decode(data.as_ref()) {
                if let Some(Payload::DatabaseQueryResult(res)) = c.payload {
                    if res.query_id == "test_probe" {
                        return serde_json::from_slice::<serde_json::Value>(&res.result_blob)
                            .ok()
                            .and_then(|v| {
                                if v.get("module").and_then(|m| m.as_str()) != Some(module) {
                                    // Not our module's response — keep draining.
                                    return None;
                                }
                                v.get("responded").and_then(|r| r.as_bool())
                            })
                            .unwrap_or(false);
                    }
                }
            }
        }
    }
    false
}

pub async fn run_soak_suite(cli: &Cli) -> Vec<Metrics> {
    let window = Duration::from_secs(cli.duration_secs.max(1));
    let mut m = Metrics::new("soak");

    // 1. Discover modules (same eligibility as the probe harness, EXCEPT that
    //    terminal/UI modules are included via a PTY — they are exactly the
    //    modules this suite exists to keep honest).
    let runner_dir = std::env::current_dir().unwrap_or_default();
    let modules_dir = runner_dir.parent().unwrap_or(&runner_dir).join("modules");
    let mut launched: Vec<String> = Vec::new();
    let mut children: Vec<std::process::Child> = Vec::new();

    let mut entries = match std::fs::read_dir(&modules_dir) {
        Ok(e) => e.flatten().collect::<Vec<_>>(),
        Err(e) => {
            m.push_detail("soak_discovery", false, 0, 0.0, 0.0, 0.0, format!("cannot read {}: {}", modules_dir.display(), e));
            m.finalize();
            return vec![m];
        }
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let dir = entry.path();
        let manifest_path = dir.join("cockatiel_module_info.json");
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
        if EXCLUDED_MODULES.contains(&name.as_str()) {
            println!("[soak] skipping '{}' (dev/benchmark harness)", name);
            continue;
        }
        if module_missing_required_credentials(&dir, &manifest) {
            println!("[soak] skipping '{}' (needs credentials not configured)", name);
            continue;
        }
        let is_terminal = manifest.get("terminal").and_then(|v| v.as_bool()).unwrap_or(false);
        println!("[soak] launching '{}' (terminal={}) against the real engine...", name, is_terminal);
        let child = if is_terminal {
            launch_module_pty(&dir, &manifest, &name, cli)
        } else {
            launch_module(&dir, &manifest, &name, cli)
        };
        match child {
            Some(c) => {
                launched.push(name);
                children.push(c);
            }
            None => m.notes.push(format!("soak: '{}' could not be launched", name)),
        }
    }
    m.total_msgs = launched.len() as u64;

    // 2. Wait (bounded) for the launched modules to connect.
    let (mut ws, auth, uuid) = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("soak_harness", false, 0, 0.0, 0.0, 0.0, format!("auth failed: {}", e));
            cleanup(&mut children);
            m.finalize();
            return vec![m];
        }
    };
    let connect_deadline = Instant::now() + Duration::from_secs(20);
    let mut known: Vec<String> = Vec::new();
    while Instant::now() < connect_deadline {
        let live = connected_modules(&mut ws, &auth, &uuid).await;
        known = launched
            .iter()
            .filter(|n| live.contains(n))
            .cloned()
            .collect();
        if known.len() == launched.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if launched.is_empty() {
        m.notes.push("soak: no eligible modules to launch".to_string());
        cleanup(&mut children);
        m.finalize();
        return vec![m];
    }

    // 3. Soak: every module that made it to the live list must stay connected
    //    AND responsive for the FULL window. Any drop at any poll = FAIL.
    let window_start = Instant::now();
    let mut failures: HashMap<String, String> = HashMap::new();
    for (i, name) in launched.iter().enumerate() {
        if known.contains(name) {
            continue;
        }
        // A module that never connected AND whose process already exited is an
        // environment limitation (missing creds/deps, e.g. adapters without
        // credentials or tts-service without torch), not a connection-stability
        // failure — note it and skip. A module still running but never
        // connected is hung/blocked: a hard FAIL.
        let exited = children
            .get_mut(i)
            .map(|c| c.try_wait().ok().flatten().is_some())
            .unwrap_or(false);
        if exited {
            m.notes
                .push(format!("soak: '{}' did not connect (process exited — missing creds/deps in this environment?)", name));
            continue;
        }
        failures.insert(name.clone(), "never connected (within 20s; process still alive)".to_string());
    }
    while Instant::now().duration_since(window_start) < window {
        let live = connected_modules(&mut ws, &auth, &uuid).await;
        // A module that connected late (after the wait deadline) is still
        // monitored once it shows up — don't let a slow start hide it.
        for name in &launched {
            if live.contains(name) && !known.contains(name) {
                known.push(name.clone());
            }
        }
        for name in &launched {
            if failures.contains_key(name) {
                continue;
            }
            let connected_now = live.contains(name);
            let responsive = if connected_now {
                auth_verify_ok(&mut ws, &auth, &uuid, name).await
            } else {
                false
            };
            if !connected_now || !responsive {
                let stayed = Instant::now().duration_since(window_start);
                failures.insert(
                    name.clone(),
                    format!(
                        "dropped after {:.1}s of {}s (connected={} responsive={})",
                        stayed.as_secs_f64(),
                        window.as_secs(),
                        connected_now,
                        responsive
                    ),
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(2500)).await;
    }

    // 3b. Final verification pass at the very end of the window: a poll can
    //     overshoot the window, so a module that dropped in the last stretch
    //     could otherwise slip past the loop. One last check catches it.
    {
        let live = connected_modules(&mut ws, &auth, &uuid).await;
        for name in &launched {
            if failures.contains_key(name) {
                continue;
            }
            let connected_now = live.contains(name);
            let responsive = if connected_now {
                auth_verify_ok(&mut ws, &auth, &uuid, name).await
            } else {
                false
            };
            if !connected_now || !responsive {
                failures.insert(
                    name.clone(),
                    format!(
                        "dropped before the final check (connected={} responsive={})",
                        connected_now, responsive
                    ),
                );
            }
        }
    }
    let window_secs = window.as_secs();

    // 4. Report per module.
    for name in &launched {
        match failures.get(name) {
            Some(reason) => m.push_detail(
                format!("soak:{}", name),
                false,
                0, 0.0, 0.0, 0.0,
                reason.clone(),
            ),
            None => m.push_detail(
                format!("soak:{}", name),
                true,
                window_secs as u128 * 1000,
                0.0, 0.0, 0.0,
                format!("connected + responsive for the full {}s", window_secs),
            ),
        }
    }
    m.notes.push(format!("launched={} window={}s", launched.len(), window_secs));

    // 5. Cleanup: kill every module we spawned, always.
    cleanup(&mut children);

    m.finalize();
    vec![m]
}