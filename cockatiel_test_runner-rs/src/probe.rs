//! Probe suite: launch the installed modules against the REAL engine, then
//! probe each one individually via the engine's `test_probe` query, reporting
//! per-module pass/fail + min/avg/max response times. This is the per-module
//! dimension of the screening/hardening passes — the mechanism exists; this
//! harness gives it real modules to measure.

use std::time::{Duration, Instant};

use crate::{
    Cli, Metrics, module_missing_required_credentials,
    screening::{auth_as_test_runner, connected_modules, probe_module},
};

/// Resolve the module's binary for the current platform from the manifest
/// (`binary.<os>.<arch>`), falling back to `launch_command` (cargo run) when no
/// built binary is declared — avoids cargo-overhead stalls when launching many
/// modules at once.
pub(crate) fn resolve_binary(dir: &std::path::Path, manifest: &serde_json::Value) -> Option<(String, Vec<String>)> {
    let os = if cfg!(target_os = "macos") { "macos" } else if cfg!(target_os = "windows") { "windows" } else { "linux" };
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else if cfg!(target_arch = "x86_64") { "x86_64" } else { "arm" };
    if let Some(bin) = manifest
        .get("binary")
        .and_then(|b| b.get(os))
        .and_then(|o| o.get(arch))
        .and_then(|v| v.as_str())
    {
        let path = dir.join(bin);
        if path.exists() {
            return Some((path.to_string_lossy().into_owned(), vec![]));
        }
    }
    let launch = manifest.get("launch_command").and_then(|v| v.as_str()).unwrap_or("");
    let mut parts: Vec<String> = launch.split_whitespace().map(String::from).collect();
    let flags: Vec<String> = manifest
        .get("command_flags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|f| f.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if parts.is_empty() {
        return None;
    }
    let program = parts.remove(0);
    parts.extend(flags);
    Some((program, parts))
}

/// Launch a module binary against the real engine (CLI overrides win over any
/// on-disk connection config). Returns the spawned child or None.
pub(crate) fn launch_module(dir: &std::path::Path, manifest: &serde_json::Value, name: &str, cli: &Cli) -> Option<std::process::Child> {
    launch_module_impl(dir, manifest, name, cli, false)
}

/// Launch a TERMINAL/UI module inside a pseudo-TTY so it can run headless
/// against the engine (TUI modules need a real TTY for crossterm). macOS:
/// `script -q /dev/null <cmd...>`; Linux: `script -q -c "<cmd>" /dev/null`;
/// Windows has no `script` — returns None. Shared by the probe + soak harnesses.
pub(crate) fn launch_module_pty(dir: &std::path::Path, manifest: &serde_json::Value, name: &str, cli: &Cli) -> Option<std::process::Child> {
    launch_module_impl(dir, manifest, name, cli, true)
}

fn launch_module_impl(
    dir: &std::path::Path,
    manifest: &serde_json::Value,
    name: &str,
    cli: &Cli,
    pty: bool,
) -> Option<std::process::Child> {
    let (program, mut args) = resolve_binary(dir, manifest)?;
    args.extend([
        "--ip".into(),
        cli.ip.clone(),
        "--port".into(),
        cli.port.to_string(),
        "--pin".into(),
        cli.pin.to_string(),
        // Pin the module's identity so it connects as itself even without a
        // local connection file (which may be absent or stale in CI).
        "--name".into(),
        name.into(),
    ]);
    let mut cmd = if pty {
        #[cfg(target_os = "macos")]
        {
            let mut c = std::process::Command::new("script");
            c.arg("-q").arg("/dev/null").arg(&program);
            c
        }
        #[cfg(target_os = "linux")]
        {
            // util-linux `script`: `-c` takes the whole command as one string.
            let mut full = String::new();
            for (i, part) in std::iter::once(&program).chain(args.iter()).enumerate() {
                if i > 0 {
                    full.push(' ');
                }
                full.push_str(&shell_quote_single(part));
            }
            let mut c = std::process::Command::new("script");
            c.arg("-q").arg("-c").arg(full).arg("/dev/null");
            c
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            return None;
        }
    } else {
        let mut c = std::process::Command::new(&program);
        c.args(&args);
        c
    };
    if pty {
        #[cfg(not(target_os = "linux"))]
        cmd.args(&args);
        #[cfg(target_os = "linux")]
        let _ = &args; // folded into the `-c` string above
    }
    // The engine only accepts WSS — point launched modules at its self-signed
    // cert so they connect over TLS (same as the TUI supervisor does).
    let cert = std::env::current_dir()
        .unwrap_or_default()
        .join("..")
        .join("cockatiel_engine-rs")
        .join("tls")
        .join("cockatiel-cert.pem");
    if cert.exists() {
        cmd.env("COCKATIEL_TLS_CERT", &cert);
    }
    cmd.current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().ok()
}

/// Minimal single-quote shell escaping for embedding a path/arg in a shell
/// command string (util-linux `script -c`). The args we build are safe, but a
/// module directory or binary path containing spaces must not break the line.
#[cfg(target_os = "linux")]
fn shell_quote_single(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub async fn run_probe_suite(cli: &Cli) -> Vec<Metrics> {
    let mut m = Metrics::new("probe");

    // 1. Discover modules (same eligibility as the benchmark: skip terminal/UI
    // modules and modules whose required credentials aren't configured).
    let runner_dir = std::env::current_dir().unwrap_or_default();
    let modules_dir = runner_dir.parent().unwrap_or(&runner_dir).join("modules");
    let mut launched: Vec<String> = Vec::new();
    let mut children: Vec<std::process::Child> = Vec::new();

    let mut entries = match std::fs::read_dir(&modules_dir) {
        Ok(e) => e.flatten().collect::<Vec<_>>(),
        Err(e) => {
            m.push_detail("probe_discovery", false, 0, 0.0, 0.0, 0.0, format!("cannot read {}: {}", modules_dir.display(), e));
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
        if manifest.get("terminal").and_then(|v| v.as_bool()).unwrap_or(false) {
            println!("[probe] skipping '{}' (terminal/UI module)", name);
            continue;
        }
        if module_missing_required_credentials(&dir, &manifest) {
            println!("[probe] skipping '{}' (needs credentials not configured)", name);
            continue;
        }
        println!("[probe] launching '{}' against the real engine...", name);
        match launch_module(&dir, &manifest, &name, cli) {
            Some(child) => {
                launched.push(name);
                children.push(child);
            }
            None => m.notes.push(format!("probe: '{}' could not be launched", name)),
        }
    }

    // 2. Wait (bounded) for the launched modules to connect.
    let (mut ws, auth, uuid) = match auth_as_test_runner(cli).await {
        Ok(x) => x,
        Err(e) => {
            m.push_detail("probe_harness", false, 0, 0.0, 0.0, 0.0, format!("auth failed: {}", e));
            cleanup(&mut children);
            m.finalize();
            return vec![m];
        }
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut connected: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        let live = connected_modules(&mut ws, &auth, &uuid).await;
        connected = launched
            .iter()
            .filter(|name| live.contains(name))
            .cloned()
            .collect();
        if connected.len() == launched.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 3. Probe each connected module individually + report min/avg/max.
    if launched.is_empty() {
        m.notes.push("probe: no eligible modules to launch".to_string());
    } else if connected.is_empty() {
        m.push_detail("probe_harness", false, 0, 0.0, 0.0, 0.0, "none of the launched modules connected (blocked on setup?)");
    }
    for name in &connected {
        // auth_verify is the meaningful per-module metric: every real module
        // answers it, so responded + min/avg/max are genuine liveness/round-trip
        // numbers. (Modules legitimately ignore display-only `log` payloads, so
        // that probe stays in the screening suite rather than failing here.)
        let latencies = probe_module(&mut ws, &auth, &uuid, name, "auth_verify").await;
        if latencies.is_empty() {
            m.push_detail(format!("probe:{}:auth_verify", name), false, 0, 0.0, 0.0, 0.0, "no response / probe failed");
            continue;
        }
        m.record(&latencies);
        m.push_detail(
            format!("probe:{}:auth_verify", name),
            true,
            latencies.len() as u128 * 10,
            m.latency_avg_ms, m.latency_min_ms, m.latency_max_ms,
            format!("{} probes", latencies.len()),
        );
    }
    m.notes.push(format!("launched={} connected={}", launched.len(), connected.len()));

    // 4. Cleanup: kill every module we spawned, always.
    cleanup(&mut children);

    m.finalize();
    vec![m]
}

pub(crate) fn cleanup(children: &mut [std::process::Child]) {
    for child in children.iter_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
}