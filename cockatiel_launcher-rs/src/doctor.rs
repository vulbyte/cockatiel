//! `cockatiel doctor` — boot an ISOLATED Cockatiel stack and run the existing
//! test-runner compliance suites against it.
//!
//! Every mode gets a throwaway sandbox under `<root>/.doctor/<mode>-<rand>/`
//! with its own engine config, databases, TLS cert and freshly minted
//! credentials. Nothing here touches the real install: the engine and user-db
//! are spawned with a sandbox cwd and sandbox-only environment, the test-runner
//! is pointed at the sandbox's TLS cert, and both children are killed and
//! reaped on every exit path (success, error or early return) by a drop guard.

use std::fs::{self, File};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// The marker the test-runner prints immediately before its JSON summary.
const RESULT_MARKER: &str = "__RESULT_JSON__";

/// Which pipeline boot state a sandbox is tested in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Paused,
    Unpaused,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Paused => "paused",
            Mode::Unpaused => "unpaused",
        }
    }

    /// Parse a `--mode` value into the modes to run, in order.
    pub fn parse(value: &str) -> Result<Vec<Mode>, String> {
        match value {
            "paused" => Ok(vec![Mode::Paused]),
            "unpaused" => Ok(vec![Mode::Unpaused]),
            "both" => Ok(vec![Mode::Paused, Mode::Unpaused]),
            other => Err(format!(
                "unknown mode: {} (expected paused, unpaused or both)",
                other
            )),
        }
    }
}

/// Everything `doctor` needs to know to run a compliance pass.
pub struct DoctorOptions {
    pub root: PathBuf,
    pub test_runner: Option<PathBuf>,
    pub modules_dir: Option<PathBuf>,
    pub modes: Vec<Mode>,
    pub quick: bool,
    pub soak: bool,
    pub iterations: u64,
    pub duration_secs: u64,
    pub keep: bool,
}

/// The summed result of one suite invocation, plus the raw parsed payload.
#[derive(Debug, Clone, Serialize)]
pub struct SuiteResult {
    pub suite: String,
    pub passed: u64,
    pub failed: u64,
    pub raw: serde_json::Value,
}

/// The result of one pipeline mode: did it boot, and what did its suites say.
#[derive(Debug, Clone, Serialize)]
pub struct ModeReport {
    pub mode: String,
    pub booted: bool,
    pub error: Option<String>,
    pub suites: Vec<SuiteResult>,
}

/// The overall doctor verdict.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub modes: Vec<ModeReport>,
}

/// Run every requested mode and aggregate the verdict.
///
/// A missing component binary is fatal up front; a mode that fails to boot is
/// recorded as a failed [`ModeReport`] rather than an `Err`, so one bad mode
/// still lets the others run.
pub fn run(opts: &DoctorOptions) -> Result<DoctorReport, String> {
    let bins = resolve_binaries(&opts.root, opts.test_runner.as_deref())?;
    let modules = opts
        .modules_dir
        .clone()
        .unwrap_or_else(|| opts.root.join("modules"));

    let mut modes = Vec::new();
    for mode in &opts.modes {
        modes.push(run_mode(opts, &bins, &modules, *mode));
    }

    let ok = modes.iter().all(|m| {
        m.booted && m.error.is_none() && m.suites.iter().all(|s| s.failed == 0)
    });
    Ok(DoctorReport { ok, modes })
}

// ── binaries ────────────────────────────────────────────────────────

#[derive(Debug)]
struct Binaries {
    engine: PathBuf,
    user_db: PathBuf,
    test_runner: PathBuf,
}

/// Resolve the three component binaries under `root`, failing with one clear
/// message if any is missing.
fn resolve_binaries(root: &Path, test_runner: Option<&Path>) -> Result<Binaries, String> {
    let suffix = std::env::consts::EXE_SUFFIX;
    let engine = root
        .join("engine")
        .join(format!("cockatiel-engine-rs{}", suffix));
    let user_db = root
        .join("user-db")
        .join(format!("cockatiel-user-database{}", suffix));
    let test_runner = test_runner
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("bin").join(format!("cockatiel-test-runner{}", suffix)));

    if !engine.is_file() || !user_db.is_file() || !test_runner.is_file() {
        return Err(format!(
            "engine/user-db/test-runner not found under {}; run `cockatiel install` first",
            root.display()
        ));
    }

    Ok(Binaries {
        engine,
        user_db,
        test_runner,
    })
}

// ── per-mode orchestration ──────────────────────────────────────────

fn run_mode(opts: &DoctorOptions, bins: &Binaries, modules: &Path, mode: Mode) -> ModeReport {
    let mode_name = mode.as_str().to_string();
    let sandbox = opts
        .root
        .join(".doctor")
        .join(format!("{}-{}", mode_name, rand_suffix()));

    // The guard owns both children; dropping it kills + reaps them no matter
    // how `boot_and_test` returns.
    let mut guard = ChildGuard::default();
    let result = boot_and_test(opts, bins, modules, mode, &sandbox, &mut guard);
    drop(guard);

    if !opts.keep {
        let _ = fs::remove_dir_all(&sandbox);
    }

    match result {
        Ok(suites) => ModeReport {
            mode: mode_name,
            booted: true,
            error: None,
            suites,
        },
        Err(error) => ModeReport {
            mode: mode_name,
            booted: false,
            error: Some(error),
            suites: Vec::new(),
        },
    }
}

fn boot_and_test(
    opts: &DoctorOptions,
    bins: &Binaries,
    modules: &Path,
    mode: Mode,
    sandbox: &Path,
    guard: &mut ChildGuard,
) -> Result<Vec<SuiteResult>, String> {
    let engine_dir = sandbox.join("engine");
    let userdb_dir = sandbox.join("user-db");
    fs::create_dir_all(&engine_dir)
        .map_err(|e| format!("create {}: {}", engine_dir.display(), e))?;
    fs::create_dir_all(&userdb_dir)
        .map_err(|e| format!("create {}: {}", userdb_dir.display(), e))?;

    let engine_port = free_port();
    let mut userdb_port = free_port();
    while userdb_port == engine_port {
        userdb_port = free_port();
    }
    let pin = random_pin();
    let token = random_token();

    let config = config_json(&engine_dir, engine_port, mode == Mode::Paused);
    fs::write(engine_dir.join("config.json"), config)
        .map_err(|e| format!("write engine config: {}", e))?;

    // The hardening suite impersonates a real registered module (`banned-words`)
    // to exercise name-trust and the gated-query surface. A brand-new engine has
    // an empty registry, so those setups would be rejected before the check even
    // runs. Seed an auto-approved entry — the engine's own auto_config pass
    // preserves existing entries, so this survives startup.
    fs::write(engine_dir.join("modules.json"), seeded_registry_json())
        .map_err(|e| format!("write engine registry: {}", e))?;

    // 1. user-db, bound to loopback with sandbox-only paths.
    let mut userdb = Command::new(&bins.user_db);
    userdb
        .current_dir(&userdb_dir)
        .env("USER_DB_PORT", userdb_port.to_string())
        .env("USER_DB_TOKEN", &token)
        .env("USER_DB_PATH", userdb_dir.join("user_data.db"))
        .env("USER_DB_BACKUP_PATH", userdb_dir.join("user_data_backup.db"))
        .env("USER_DB_BIND", "127.0.0.1");
    redirect(&mut userdb, &sandbox.join("userdb.log"))?;
    guard.push(
        userdb
            .spawn()
            .map_err(|e| format!("spawn user-db {}: {}", bins.user_db.display(), e))?,
    );

    if !wait_for_port(userdb_port, Duration::from_secs(15)) {
        return Err(format!(
            "user-db did not open 127.0.0.1:{} within 15s (see {})",
            userdb_port,
            sandbox.join("userdb.log").display()
        ));
    }

    // 2. engine, pointed at the user-db and the sandbox.
    let mut engine = Command::new(&bins.engine);
    engine
        .current_dir(&engine_dir)
        .env("USER_DB_HOST", "127.0.0.1")
        .env("USER_DB_PORT", userdb_port.to_string())
        .env("USER_DB_TOKEN", &token)
        .env("USER_DB_BACKUP_PATH", userdb_dir.join("user_data_backup.db"))
        .env("COCKATIEL_PIN", pin.to_string())
        .env(
            "COCKATIEL_START_PAUSED",
            if mode == Mode::Paused { "true" } else { "false" },
        );
    let rank_chart = opts.root.join("rank_chart.json");
    if rank_chart.is_file() {
        engine.env("COCKATIEL_RANK_CHART", &rank_chart);
    }
    redirect(&mut engine, &sandbox.join("engine.log"))?;
    guard.push(
        engine
            .spawn()
            .map_err(|e| format!("spawn engine {}: {}", bins.engine.display(), e))?,
    );

    let cert = engine_dir.join("tls").join("cockatiel-cert.pem");
    if !wait_for_engine(engine_port, &cert, Duration::from_secs(25)) {
        return Err(format!(
            "engine did not open 127.0.0.1:{} and write {} within 25s (see {})",
            engine_port,
            cert.display(),
            sandbox.join("engine.log").display()
        ));
    }

    // 3. the compliance suites.
    let mut results = Vec::new();
    for suite in suites_for(opts) {
        results.push(run_suite(
            bins,
            opts,
            modules,
            &engine_dir,
            engine_port,
            pin,
            &suite,
        ));
    }
    Ok(results)
}

/// Which suites to run for the given options.
fn suites_for(opts: &DoctorOptions) -> Vec<String> {
    let mut suites: Vec<String> = if opts.quick {
        vec!["screening".to_string()]
    } else {
        vec![
            "screening".to_string(),
            "hardening".to_string(),
            "probe".to_string(),
        ]
    };
    if opts.soak {
        suites.push("soak".to_string());
    }
    suites
}

#[allow(clippy::too_many_arguments)]
fn run_suite(
    bins: &Binaries,
    opts: &DoctorOptions,
    modules: &Path,
    engine_dir: &Path,
    engine_port: u16,
    pin: u32,
    suite: &str,
) -> SuiteResult {
    let cert = engine_dir.join("tls").join("cockatiel-cert.pem");
    let mut cmd = Command::new(&bins.test_runner);
    cmd.arg("--suite")
        .arg(suite)
        .arg("--ip")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(engine_port.to_string())
        .arg("--pin")
        .arg(pin.to_string())
        .arg("--install-root")
        .arg(&opts.root)
        .arg("--modules-dir")
        .arg(modules)
        .arg("--iterations")
        .arg(opts.iterations.to_string())
        .arg("--json")
        .env("COCKATIEL_TLS_CERT", &cert)
        .env("COCKATIEL_ENGINE_DIR", engine_dir);
    if suite == "soak" {
        cmd.arg("--duration-secs").arg(opts.duration_secs.to_string());
    }

    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            match parse_result_json(&stdout) {
                Ok((passed, failed, raw)) => SuiteResult {
                    suite: suite.to_string(),
                    passed,
                    failed,
                    raw,
                },
                Err(err) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let note = format!(
                        "{}\nstdout tail:\n{}\nstderr tail:\n{}",
                        err,
                        tail(&stdout),
                        tail(&stderr)
                    );
                    SuiteResult {
                        suite: suite.to_string(),
                        passed: 0,
                        failed: 1,
                        raw: serde_json::json!({ "error": note }),
                    }
                }
            }
        }
        Err(e) => SuiteResult {
            suite: suite.to_string(),
            passed: 0,
            failed: 1,
            raw: serde_json::json!({ "error": format!("failed to run test-runner: {}", e) }),
        },
    }
}

/// Extract `passed`/`failed` summed across the metrics array the test-runner
/// prints after `__RESULT_JSON__`.
fn parse_result_json(stdout: &str) -> Result<(u64, u64, serde_json::Value), String> {
    let idx = stdout
        .find(RESULT_MARKER)
        .ok_or_else(|| "test-runner output missing __RESULT_JSON__ marker".to_string())?;
    let payload = stdout[idx + RESULT_MARKER.len()..].trim_start();
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| format!("invalid __RESULT_JSON__ payload: {}", e))?;
    let array = value
        .as_array()
        .ok_or_else(|| "__RESULT_JSON__ payload is not an array".to_string())?;

    let mut passed = 0u64;
    let mut failed = 0u64;
    for metric in array {
        passed += metric.get("passed").and_then(|v| v.as_u64()).unwrap_or(0);
        failed += metric.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
    }
    Ok((passed, failed, value))
}

/// The engine's pre-written config. The engine backfills every other key.
fn config_json(dir: &Path, port: u16, paused: bool) -> String {
    let value = serde_json::json!({
        "timeline_database_location": dir.join("cockatiel_data.db").to_string_lossy(),
        "timeline_database_backup_location": dir.join("cockatiel_backup.db").to_string_lossy(),
        "port": port,
        "start_paused": paused,
    });
    serde_json::to_string_pretty(&value).unwrap_or_default()
}

/// Pre-approved module identities the compliance suites impersonate. The
/// hardening suite connects as `banned-words` to test name-trust and the
/// gated-query surface, which needs a registered `auto_auth` entry on a fresh
/// engine. Kept in the same shape CI seeds.
fn seeded_registry_json() -> String {
    // The engine deserializes this into `RegisteredModule`, whose fields are all
    // required — a partial entry (e.g. CI's name/auto_auth/position/priority
    // seed) fails to parse and the whole registry loads empty. `auth_token` may
    // be empty: auto-approval is name-only, the token is only for pinned-identity
    // reconnect which the suites do not need here.
    serde_json::json!([
        {
            "name": "banned-words",
            "instance_uuid7": "00000000-0000-7000-8000-000000000001",
            "position": "inprocess",
            "priority": 100,
            "auto_auth": true,
            "auth_token": ""
        }
    ])
    .to_string()
}

// ── ports, waits, spawning ──────────────────────────────────────────

/// Bind an ephemeral loopback port, then release it. The caller uses the port
/// number; the listener is already gone by the time this returns. The port is
/// re-bound once before being handed back, so a transient collision (another
/// process grabbing the port in the drop/return window) picks a fresh one.
fn free_port() -> u16 {
    for _ in 0..100 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback port");
        let port = listener
            .local_addr()
            .expect("read ephemeral local_addr")
            .port();
        drop(listener);
        if let Ok(recheck) = TcpListener::bind(("127.0.0.1", port)) {
            drop(recheck);
            return port;
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback port");
    listener
        .local_addr()
        .expect("read ephemeral local_addr")
        .port()
}

fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if port_open(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn wait_for_engine(port: u16, cert: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if port_open(port) && cert.is_file() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

fn redirect(cmd: &mut Command, log_path: &Path) -> Result<(), String> {
    let file = File::create(log_path).map_err(|e| format!("create {}: {}", log_path.display(), e))?;
    let err_file = file
        .try_clone()
        .map_err(|e| format!("clone log handle {}: {}", log_path.display(), e))?;
    cmd.stdout(Stdio::from(file)).stderr(Stdio::from(err_file));
    Ok(())
}

/// Kills and reaps the spawned children when dropped, on every exit path.
#[derive(Default)]
struct ChildGuard {
    children: Vec<Child>,
}

impl ChildGuard {
    fn push(&mut self, child: Child) {
        self.children.push(child);
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ── randomness (no new deps) ────────────────────────────────────────

/// A cheap unique value from wall-clock nanos, pid and a process-local counter.
fn seed_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((std::process::id() as u64) << 32)
}

fn random_pin() -> u32 {
    100_000 + (seed_u64() % 900_000) as u32
}

fn random_token() -> String {
    let mut token = String::new();
    for _ in 0..4 {
        token.push_str(&format!("{:016x}", seed_u64()));
    }
    token
}

fn rand_suffix() -> String {
    format!("{:x}", seed_u64())
}

/// The last 2000 characters of `s`, for failure notes.
fn tail(s: &str) -> String {
    const MAX: usize = 2000;
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(MAX);
    chars[start..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cockatiel_doctor_{}_{}_{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_json_has_required_keys_and_flips_pause() {
        let dir = PathBuf::from("/tmp/cockatiel-doctor-engine");
        let paused: serde_json::Value = serde_json::from_str(&config_json(&dir, 12345, true)).unwrap();
        assert_eq!(paused["port"], 12345);
        assert_eq!(paused["start_paused"], true);
        assert!(paused["timeline_database_location"]
            .as_str()
            .unwrap()
            .ends_with("cockatiel_data.db"));
        assert!(paused["timeline_database_backup_location"]
            .as_str()
            .unwrap()
            .ends_with("cockatiel_backup.db"));

        let unpaused: serde_json::Value =
            serde_json::from_str(&config_json(&dir, 12345, false)).unwrap();
        assert_eq!(unpaused["start_paused"], false);
        assert_eq!(unpaused["port"], 12345);
    }

    #[test]
    fn seeded_registry_entries_are_complete_for_the_engine_parser() {
        // The engine deserializes each entry into `RegisteredModule` with all
        // fields required; a partial entry makes the whole registry load empty
        // (the bug that silently broke the hardening setup). Pin every field.
        let value: serde_json::Value = serde_json::from_str(&seeded_registry_json()).unwrap();
        let arr = value.as_array().unwrap();
        assert!(!arr.is_empty());
        let first = arr[0].as_object().unwrap();
        for key in [
            "name",
            "instance_uuid7",
            "position",
            "priority",
            "auto_auth",
            "auth_token",
        ] {
            assert!(first.contains_key(key), "seed entry missing {key}");
        }
        assert_eq!(first["auto_auth"], true);
        assert!(!first["instance_uuid7"].as_str().unwrap().is_empty());
    }

    #[test]
    fn parse_result_json_sums_passed_and_failed() {
        let stdout = "noise\n__RESULT_JSON__[\n  {\"name\":\"screening\",\"passed\":3,\"failed\":1},\n  {\"name\":\"probe\",\"passed\":2,\"failed\":0}\n]\n";
        let (passed, failed, raw) = parse_result_json(stdout).unwrap();
        assert_eq!(passed, 5);
        assert_eq!(failed, 1);
        assert!(raw.is_array());
    }

    #[test]
    fn parse_result_json_errors_without_marker() {
        let err = parse_result_json("all good, no marker here").unwrap_err();
        assert!(err.contains("__RESULT_JSON__"), "{}", err);
    }

    #[test]
    fn free_port_is_bindable_immediately() {
        // The OS can hand a just-released ephemeral port to an unrelated
        // process, so allow a couple of draws before declaring the helper bad.
        for _ in 0..100 {
            let port = free_port();
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
                drop(listener);
                return;
            }
        }
        panic!("free_port never produced a bindable port");
    }

    #[test]
    fn missing_binary_uses_the_install_hint() {
        let root = temp_root("missing");
        let err = resolve_binaries(&root, None).unwrap_err();
        assert!(
            err.contains("engine/user-db/test-runner not found under"),
            "{}",
            err
        );
        assert!(err.contains("cockatiel install"), "{}", err);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn binary_resolution_finds_all_three() {
        let root = temp_root("bins");
        let suffix = std::env::consts::EXE_SUFFIX;
        let engine = root.join("engine").join(format!("cockatiel-engine-rs{}", suffix));
        let user_db = root
            .join("user-db")
            .join(format!("cockatiel-user-database{}", suffix));
        let test_runner = root
            .join("bin")
            .join(format!("cockatiel-test-runner{}", suffix));
        for path in [&engine, &user_db, &test_runner] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"x").unwrap();
        }

        let bins = resolve_binaries(&root, None).unwrap();
        assert_eq!(bins.engine, engine);
        assert_eq!(bins.user_db, user_db);
        assert_eq!(bins.test_runner, test_runner);

        let override_path = root.join("custom-runner");
        fs::write(&override_path, b"x").unwrap();
        let bins = resolve_binaries(&root, Some(&override_path)).unwrap();
        assert_eq!(bins.test_runner, override_path);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mode_parse_handles_known_and_unknown() {
        assert_eq!(Mode::parse("paused").unwrap(), vec![Mode::Paused]);
        assert_eq!(Mode::parse("unpaused").unwrap(), vec![Mode::Unpaused]);
        assert_eq!(
            Mode::parse("both").unwrap(),
            vec![Mode::Paused, Mode::Unpaused]
        );
        assert!(Mode::parse("sideways").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn child_guard_kills_and_reaps() {
        let mut guard = ChildGuard::default();
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        guard.push(child);
        drop(guard);

        std::thread::sleep(Duration::from_millis(200));
        let status = Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "child {} should have been reaped", pid);
    }
}
