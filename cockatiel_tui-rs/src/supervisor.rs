use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::plugins::Plugin;
use crate::windows::modules::StageDirection;

/// Write a file atomically with mode 0o600: write to a `.tmp-<uuid>` sibling,
/// chmod it to 0o600 BEFORE renaming (so a secret never exists world-readable,
/// not even transiently, and no symlink is followed — we only chmod our own
/// temp inode), fsync it, then rename over the target. `modules.json`/
/// `config.json`/`.env` are written by BOTH the TUI supervisor and the engine —
/// a torn write must never leave a half-written file. The unique temp name also
/// means two concurrent writers can never clobber each other's temp file.
pub fn write_atomic_0600(path: &Path, content: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Clear every VALUE in a module's `.env` + `config.json` — keys and structure
/// stay, leaf values are emptied (`.env`: `KEY=`; `config.json`: scalars → "",
/// arrays → `[]`). Used by the "clear config" action in the modules window.
pub fn clear_module_config(dir: &Path) -> std::io::Result<()> {
    let env_path = dir.join(".env");
    if let Ok(content) = std::fs::read_to_string(&env_path) {
        let mut out = String::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                out.push_str(line);
                out.push('\n');
            } else if let Some((k, _v)) = trimmed.split_once('=') {
                out.push_str(&format!("{}={}\n", k.trim(), ""));
            } else {
                out.push_str(&format!("{}={}\n", trimmed, ""));
            }
        }
        write_atomic_0600(&env_path, &out)?;
    }

    let json_path = dir.join("config.json");
    if let Ok(content) = std::fs::read_to_string(&json_path) {
        if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&content) {
            clear_json_values(&mut root);
            if let Ok(pretty) = serde_json::to_string_pretty(&root) {
                let _ = write_atomic_0600(&json_path, &pretty);
            }
        }
    }
    Ok(())
}

/// Empty a JSON value's leaf values while keeping its object keys/arrays.
fn clear_json_values(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (_k, val) in map.iter_mut() {
                clear_json_values(val);
            }
        }
        serde_json::Value::Array(arr) => arr.clear(),
        other => *other = serde_json::Value::String(String::new()),
    }
}

/// Where the engine lives relative to the TUI crate.
pub fn engine_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cockatiel_engine-rs")
}

pub fn engine_config_path() -> PathBuf {
    engine_dir().join("config.json")
}

/// The TUI's own `config.json` key: launch the engine at startup?
///
/// Named for what it decides rather than for the flag that overrides it, so the
/// file reads as a setting and not as a mirror of one command-line switch.
pub const LAUNCH_ENGINE_KEY: &str = "launch_engine";

/// The TUI's `auto_start` setting: whether autostart-tagged modules are
/// launched automatically (and the paused pipeline resumed) once the TUI
/// connects to the engine — a one-click start for a streamer.
pub const AUTO_START_KEY: &str = "auto_start";

/// The TUI's `launch_engine` setting, or `None` when the file is missing, is not
/// an object, or has no usable value for the key.
///
/// `None` is deliberately not the same as `false`: it means "nothing said", and
/// the caller falls back to the built-in default (launch). A wrong value must
/// never be invented from a typo.
pub fn read_launch_engine_default(path: &Path) -> Option<bool> {
    let content = std::fs::read_to_string(path).ok()?;
    let config: serde_json::Value = serde_json::from_str(&content).ok()?;
    config.get(LAUNCH_ENGINE_KEY).and_then(|v| v.as_bool())
}

/// The TUI's `auto_start` setting, or `None` when the file is missing, is not
/// an object, or has no usable value. `None` means "nothing said" and the
/// caller uses the built-in default (off — autostart modules stay manual).
pub fn read_auto_start(path: &Path) -> Option<bool> {
    let content = std::fs::read_to_string(path).ok()?;
    let config: serde_json::Value = serde_json::from_str(&content).ok()?;
    config.get(AUTO_START_KEY).and_then(|v| v.as_bool())
}

/// Make sure the TUI's `config.json` exists and carries every default key.
///
/// This is THE writer for that file, so a fresh install (an empty directory, or
/// a config.json written before a key existed) gets the key rather than relying
/// on the checked-in one and silently falling back. It MERGES: an operator's
/// other settings in the same file survive, an existing value is never
/// overwritten, and a file that is not a JSON object is left completely alone
/// (it is not ours to interpret, and a missing default is recoverable while a
/// clobbered file is not).
///
/// Written with the same atomic 0600 writer as every other config file the
/// supervisor touches.
pub fn ensure_tui_config(path: &Path) {
    let mut root: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(serde_json::Value::Object(map)) => serde_json::Value::Object(map),
            // Missing, blank, corrupt, or a non-object: start from nothing
            // rather than trying to preserve something we cannot read. Only the
            // first two cases are written at all.
            _ => {
                if path.exists() {
                    return;
                }
                serde_json::Value::Object(serde_json::Map::new())
            }
        },
        Err(_) => serde_json::Value::Object(serde_json::Map::new()),
    };
    let serde_json::Value::Object(map) = &mut root else {
        return;
    };
    // Add each default key independently, so an existing config that predates a
    // key still gains it (the early-return-on-any-key would have skipped adding
    // `auto_start` to a file that already had `launch_engine`).
    let mut changed = false;
    if !map.contains_key(LAUNCH_ENGINE_KEY) {
        map.insert(LAUNCH_ENGINE_KEY.to_string(), serde_json::Value::Bool(true));
        changed = true;
    }
    if !map.contains_key(AUTO_START_KEY) {
        map.insert(AUTO_START_KEY.to_string(), serde_json::Value::Bool(false));
        changed = true;
    }
    if !changed {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(pretty) = serde_json::to_string_pretty(&root) {
        let _ = write_atomic_0600(path, &pretty);
    }
}

/// Path to the engine's self-signed TLS cert, if it exists. The engine writes
/// this on startup; modules are launched with `COCKATIEL_TLS_CERT` set to it so
/// they connect over WSS (the engine rejects plain ws://).
pub fn engine_tls_cert_path() -> Option<PathBuf> {
    let p = engine_dir().join("tls").join("cockatiel-cert.pem");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

pub fn engine_env_path() -> PathBuf {
    engine_dir().join(".env")
}

/// Read a KEY=VALUE pair from a `.env` file.
pub fn read_env_value(path: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{}=", key);
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with(&prefix) {
            let value = &line[prefix.len()..];
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

pub fn modules_registry_path() -> PathBuf {
    engine_dir().join("modules.json")
}

/// Read the engine's modules.json and return the registered identity
/// (instance_uuid7, auth_token) for a module name. A fresh TUI/control-surface
/// process loads its own identity here so it can reconnect to a warm engine via
/// the pinned uuid (the engine's auto-approve now requires the registered
/// instance uuid for control-surface names).
pub fn registered_engine_identity(name: &str) -> Option<(String, String)> {
    registered_engine_identity_at(&modules_registry_path(), name)
}

/// Read a specific modules.json and return the registered identity
/// (instance_uuid7, auth_token) for a module name. Parameterized on the path so
/// unit tests can point it at a temp file instead of the live engine registry.
fn registered_engine_identity_at(path: &Path, name: &str) -> Option<(String, String)> {
    let content = std::fs::read_to_string(path).ok()?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&content).ok()?;
    for e in entries {
        if e.get("name").and_then(|v| v.as_str()) == Some(name) {
            let uuid = e
                .get("instance_uuid7")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let token = e
                .get("auth_token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !uuid.is_empty() && !token.is_empty() {
                return Some((uuid, token));
            }
        }
    }
    None
}

/// Best-effort startup pass: tighten the permissions of known secret-bearing
/// files (engine/user-db `.env`, engine `config.json`/`modules.json`) and every
/// module `.env` found under the repo's `modules/` tree to 0o600. Fixes files
/// left world-readable by earlier non-atomic writers. Logs failures, never
/// crashes — a lax file just stays lax until its next 0600 write.
pub fn remediate_secret_file_permissions() {
    for p in [
        engine_env_path(),
        user_db_env_path(),
        engine_config_path(),
        modules_registry_path(),
    ] {
        chmod_0600_best_effort(&p);
    }
    if let Some(modules_dir) = Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(|p| p.join("modules")) {
        chmod_env_files_best_effort(&modules_dir);
    }
}

fn chmod_0600_best_effort(path: &Path) {
    if !path.exists() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!("[supervisor] could not tighten permissions on {}: {}", path.display(), e);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn chmod_env_files_best_effort(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            chmod_env_files_best_effort(&path);
        } else if path.file_name().map(|f| f == ".env").unwrap_or(false) {
            chmod_0600_best_effort(&path);
        }
    }
}

/// Read engine address info: port from config.json (a setting), PIN from the
/// engine's `.env` (a secret, with a legacy config.json fallback).
pub fn read_engine_addr() -> Option<(u16, u32)> {
    let content = std::fs::read_to_string(engine_config_path()).ok()?;
    let config: serde_json::Value = serde_json::from_str(&content).ok()?;
    let port = config.get("port").and_then(|v| v.as_u64()).unwrap_or(1111) as u16;
    let pin = read_env_value(&engine_env_path(), "COCKATIEL_PIN")
        .and_then(|v| v.parse().ok())
        .or_else(|| config.get("paring_pin").and_then(|v| v.as_u64()).map(|v| v as u32))
        .unwrap_or(0);
    Some((port, pin))
}

/// Default user-database backup path (sibling of the live DB file).
pub fn user_db_backup_path() -> PathBuf {
    user_db_dir().join("user_data_backup.db")
}

/// Launch the engine as a child process (owned by the TUI).
/// The engine is always pointed at the user database the supervisor launches.
pub fn launch_engine() -> Result<Child, String> {
    let dir = engine_dir();
    let binary = dir.join("target").join("release").join("cockatiel-engine-rs");
    let binary = if binary.exists() {
        binary
    } else {
        dir.join("target").join("debug").join("cockatiel-engine-rs")
    };
    if !binary.exists() {
        return Err(format!("Engine binary not found at {}", binary.display()));
    }

    let log_path = dir.join("engine.log");
    // Bound the log file: rotate any engine.log past 10 MB before the engine
    // opens it (rotating an fd the engine already holds wouldn't take effect).
    rotate_log(&log_path, 10 * 1024 * 1024, 3);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&binary);
    cmd.current_dir(&dir)
        .env("USER_DB_HOST", "127.0.0.1")
        .env("USER_DB_PORT", USER_DB_DEFAULT_PORT.to_string())
        .env("USER_DB_TOKEN", user_db_token())
        .env("USER_DB_BACKUP_PATH", user_db_backup_path().to_string_lossy().to_string())
        .stdout(Stdio::from(log_file.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log_file));
    // Own process group (PGID = child PID) so a group TERM/KILL later reaches
    // the child AND everything it spawns — no orphaned grandchildren.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| format!("Failed to launch engine: {}", e))
}

/// Where the user database service lives relative to the TUI crate.
pub fn user_db_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cockatiel_user_database-rs")
}

pub const USER_DB_DEFAULT_PORT: u16 = 9736;

/// The user-database `.env` (secrets + settings for the service).
pub fn user_db_env_path() -> PathBuf {
    user_db_dir().join(".env")
}

/// Insert or update a `KEY=VALUE` pair in a `.env` file, preserving every other
/// key/comment, and creating the file (and its parent dir) if missing. Written
/// atomically with 0o600 permissions. Failure is logged, never fatal — the
/// caller still has the generated value in hand.
fn upsert_env_key(path: &Path, key: &str, value: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut out = String::new();
    let prefix = format!("{}=", key);
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let mut found = false;
            for line in content.lines() {
                if line.trim().starts_with(&prefix) {
                    out.push_str(&format!("{}={}\n", key, value));
                    found = true;
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            if !found {
                out.push_str(&format!("{}={}\n", key, value));
            }
        }
        Err(_) => out.push_str(&format!("{}={}\n", key, value)),
    }
    if let Err(e) = write_atomic_0600(path, &out) {
        crate::app::supervisor_log_global(format!(
            "[supervisor] failed to persist {} in {}: {}",
            key,
            path.display(),
            e
        ));
    }
}

/// The shared user-database auth token, stored in the service's `.env`. When
/// `USER_DB_TOKEN` is missing from that file a fresh random token is generated
/// (never a publicly known constant), persisted to the `.env` via the atomic
/// 0o600 writer, and returned — the engine and user_db are launched with the
/// same value, so they always agree.
pub fn user_db_token() -> String {
    let path = user_db_env_path();
    if let Some(token) = read_env_value(&path, "USER_DB_TOKEN").filter(|t| !t.trim().is_empty()) {
        return token;
    }
    let token = uuid::Uuid::new_v4().to_string();
    upsert_env_key(&path, "USER_DB_TOKEN", &token);
    token
}

/// Launch the user database service as a child process (owned by the TUI).
/// Databases are an expected core of the engine (unlike dynamic modules), so
/// the supervisor always starts them.
pub fn launch_user_db() -> Result<Child, String> {
    let dir = user_db_dir();
    let binary = dir.join("target").join("release").join("cockatiel-user-database");
    let binary = if binary.exists() {
        binary
    } else {
        dir.join("target").join("debug").join("cockatiel-user-database")
    };
    if !binary.exists() {
        return Err(format!("User DB binary not found at {}", binary.display()));
    }

    let log_path = dir.join("userdb.log");
    let log_file = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&binary);
    cmd.current_dir(&dir)
        .env("USER_DB_PORT", USER_DB_DEFAULT_PORT.to_string())
        .env("USER_DB_TOKEN", user_db_token())
        .env("USER_DB_PATH", dir.join("user_data.db").to_string_lossy().to_string())
        .env("USER_DB_BACKUP_PATH", user_db_backup_path().to_string_lossy().to_string())
        .stdout(Stdio::from(log_file.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log_file));
    // Own process group (PGID = child PID) so a group TERM/KILL later reaches
    // the child AND everything it spawns — no orphaned grandchildren.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| format!("Failed to launch user database: {}", e))
}

/// Build the launch command for a plugin: <launch_command> <command_flags> -- --ip --port --pin --name
fn build_module_command(p: &Plugin, port: u16, pin: u32) -> Vec<String> {
    let m = &p.manifest;
    let mut parts: Vec<String> = m.launch_command.split_whitespace().map(String::from).collect();
    for flag in &m.command_flags {
        parts.push(flag.clone());
    }
    append_conn_args(&mut parts, port, pin, &m.name);
    parts
}

/// Append the engine connection args (-- --ip --port --pin --name) to a
/// command line. `--name` pins the module's identity to its manifest name so
/// two modules can never collide on a blank/"unnamed_module" identity — the
/// engine rejects the placeholder name.
fn append_conn_args(parts: &mut Vec<String>, port: u16, pin: u32, name: &str) {
    if parts.iter().any(|s| s == "run" || s == "start" || s == "exec") {
        parts.push("--".into());
    }
    parts.push("--ip".into());
    parts.push("127.0.0.1".into());
    parts.push("--port".into());
    parts.push(port.to_string());
    parts.push("--pin".into());
    parts.push(pin.to_string());
    if !name.trim().is_empty() {
        parts.push("--name".into());
        parts.push(name.to_string());
    }
}

/// The current OS key used in a manifest's `binary` map.
fn os_key() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        _ => "linux",
    }
}

/// The current CPU architecture key used in a manifest's `binary` map.
fn arch_key() -> &'static str {
    std::env::consts::ARCH
}

/// Resolve the prebuilt binary path for this OS + arch (relative to the module
/// dir), or None when no usable route exists. A legacy flat route (key "*")
/// applies to any arch, and a single-entry route is accepted regardless of its
/// arch key (the file itself is still checked for existence by callers).
fn module_binary(p: &Plugin) -> Option<PathBuf> {
    let os_routes = p.manifest.binary.0.get(os_key())?;
    let arch = arch_key();
    let rel = os_routes
        .get(arch)
        .or_else(|| os_routes.get("*"))
        .or_else(|| {
            if os_routes.len() == 1 {
                os_routes.values().next()
            } else {
                None
            }
        })?;
    if rel.trim().is_empty() {
        return None;
    }
    Some(p.directory.join(rel))
}

/// True when the prebuilt binary is older than any source file in the module
/// directory (so running it would silently run stale code). A module with no
/// `Cargo.toml` (pure-binary, no source) is never stale.
fn binary_is_stale(dir: &Path, bin: &Path) -> bool {
    if !dir.join("Cargo.toml").exists() {
        return false;
    }
    let Ok(bin_mtime) = std::fs::metadata(bin).and_then(|m| m.modified()) else {
        return false;
    };

    fn newest_source_mtime(dir: &Path, best: Option<std::time::SystemTime>) -> Option<std::time::SystemTime> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return best;
        };
        let mut best = best;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "target" || name == ".git" || name == "node_modules" || name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let mt = if path.is_dir() {
                newest_source_mtime(&path, None)
            } else {
                entry.metadata().ok().and_then(|m| m.modified().ok())
            };
            if mt.is_some() && (best.is_none() || mt.unwrap() > best.unwrap()) {
                best = mt;
            }
        }
        best
    }

    match newest_source_mtime(dir, None) {
        Some(newest) => newest > bin_mtime,
        None => false,
    }
}

/// Size-capped log rotation: if `path` exceeds `max_bytes`, shift the existing
/// `.1..=keep` generations and roll `path` into a fresh file. Called before a
/// child process opens the log, so it always starts on a fresh, bounded file.
pub fn rotate_log(path: &Path, max_bytes: u64, keep: usize) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.len() <= max_bytes {
        return;
    }
    for i in (1..keep).rev() {
        let from = format!("{}.{}", path.display(), i);
        let to = format!("{}.{}", path.display(), i + 1);
        let _ = std::fs::rename(&from, &to);
    }
    let _ = std::fs::rename(path, format!("{}.1", path.display()));
    let _ = std::fs::File::create(path);
}

/// Record a successful build route (this OS + arch → binary path) back into
/// the module's `cockatiel_module_info.json`. Only called after a build that
/// actually produced the binary, so there is never a route to a binary that
/// failed to build.
fn record_binary_route(p: &Plugin, path: &Path) {
    let manifest_path = p.directory.join(crate::plugins::MANIFEST_FILENAME);
    let Ok(data) = std::fs::read_to_string(&manifest_path) else { return };
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&data) else { return };
    let rel = path
        .strip_prefix(&p.directory)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    let os = os_key().to_string();
    let arch = arch_key().to_string();

    if root.get("binary").is_none() {
        root["binary"] = serde_json::json!({});
    }
    let binary = root["binary"].as_object_mut().unwrap();
    let os_entry = binary.entry(os).or_insert_with(|| serde_json::json!({}));
    if os_entry.is_object() {
        os_entry
            .as_object_mut()
            .unwrap()
            .insert(arch, serde_json::json!(rel));
    } else {
        let mut m = serde_json::Map::new();
        m.insert(arch, serde_json::json!(rel));
        *os_entry = serde_json::Value::Object(m);
    }

    if let Ok(pretty) = serde_json::to_string_pretty(&root) {
        let _ = std::fs::write(&manifest_path, pretty);
        crate::app::supervisor_log_global(format!(
            "[supervisor] registered binary route {} / {} → {} for {}",
            os_key(),
            arch_key(),
            rel,
            p.manifest.name
        ));
    }
}

/// Launch an arbitrary argv in a NEW terminal window, returning once the window
/// has been asked for.
///
/// This exists for the detached/pop-out window. A pop-out is a full-screen
/// ratatui app, so spawning it with inherited stdio put a SECOND renderer with
/// its own diff buffer on the same tty as the main UI — the two interleaved
/// escape sequences, which is what made stray text appear at the cursor, inside
/// a window, and at the bottom of the screen pushing the layout up. A detached
/// window must therefore own a terminal of its own, exactly like a terminal
/// module does.
pub fn spawn_in_new_terminal(argv: &[String], title: &str) -> Result<(), String> {
    if argv.is_empty() {
        return Err("no command to launch".to_string());
    }
    // Each element is quoted so it survives both the outer shell's quote
    // stripping and the inner `sh -c` re-parse, same reasoning as
    // `spawn_terminal_from_parts`.
    let cmd_line = argv
        .iter()
        .map(|a| nested_shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let marker = format!("cockatiel:{}", title);
    let run = format!(
        "printf '\\033]0;{}\\007'; sh -c '{}'",
        shell_quote(&marker),
        // The inner sh -c is single-quoted, so its payload must not contain a
        // raw single quote; nested_shell_quote already escaped it.
        cmd_line
    );

    match std::env::consts::OS {
        "macos" => {
            let script = format!(
                "tell application \"Terminal\"\nactivate\nset wins to (every window whose name contains \"{}\")\nif (count of wins) is 0 then\ndo script \"{}\"\nelse\nset w to item 1 of wins\ndo script \"{}\" in w\nend if\nend tell",
                apple_quote(&marker),
                apple_quote(&run),
                apple_quote(&run),
            );
            let mut cmd = Command::new("osascript");
            cmd.arg("-e")
                .arg(&script)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            cmd.spawn()
                .map(|_| ())
                .map_err(|e| format!("Failed to open a Terminal window for '{}': {}", title, e))
        }
        "windows" => Command::new("cmd")
            .args(["/C", "start", "", "cmd", "/K"])
            .arg(&run)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open a console for '{}': {}", title, e)),
        _ => {
            let candidates: &[(&str, &[&str])] = &[
                ("x-terminal-emulator", &["-e", "sh", "-c"]),
                ("gnome-terminal", &["--", "sh", "-c"]),
                ("konsole", &["-e", "sh", "-c"]),
                ("xterm", &["-e", "sh", "-c"]),
            ];
            let mut last = String::from("no terminal emulator found");
            for (emu, args) in candidates {
                let res = {
                    let mut cmd = Command::new(emu);
                    cmd.args(*args)
                        .arg(&run)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::CommandExt;
                        cmd.process_group(0);
                    }
                    cmd.spawn().map(|_| ())
                };
                match res {
                    Ok(()) => return Ok(()),
                    Err(e) => last = format!("{}: {}", emu, e),
                }
            }
            Err(format!("Failed to open a terminal for '{}' ({})", title, last))
        }
    }
}

/// Build a compiled module: run its `build_command`/`build_flags` (or
/// `cargo build --release` for cargo, or the launch command for runtimes).
async fn build_module(p: &Plugin) -> Result<(), String> {
    let (cmd, flags): (Vec<String>, Vec<String>) =
        if let Some(bc) = &p.manifest.build_command {
            let mut c: Vec<String> = bc.split_whitespace().map(String::from).collect();
            if c.is_empty() {
                c.push("cargo".to_string());
            }
            (c, p.manifest.build_flags.clone())
        } else if p.manifest.launch_command.split_whitespace().next() == Some("cargo") {
            (vec!["cargo".to_string()], vec!["build".to_string(), "--release".to_string()])
        } else {
            (
                p.manifest.launch_command.split_whitespace().map(String::from).collect(),
                p.manifest.command_flags.clone(),
            )
        };

    crate::app::supervisor_log_global(format!(
        "[supervisor] building {}: {} {}",
        p.manifest.name,
        cmd.join(" "),
        flags.join(" ")
    ));
    // Pipe + capture build output: if it inherited the TUI's stdout/stderr,
    // cargo's ANSI progress bars would corrupt the ratatui screen. Errors are
    // reported in the returned error so the operator can still diagnose.
    let output = tokio::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .args(&flags)
        .current_dir(&p.directory)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("build failed to spawn for '{}': {}", p.manifest.name, e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
        return Err(format!("build failed for '{}': {}", p.manifest.name, tail));
    }
    Ok(())
}

/// Resolve the (command, args) pair to actually execute for a module:
///  - prebuilt binary (this OS) exists and not force-rebuilding → run it
///  - binary configured but missing / force-rebuilding → build it, then run it
///  - no binary configured (interpreted) → run launch_command + flags
async fn module_run_parts(p: &Plugin, port: u16, pin: u32, force_rebuild: bool) -> Result<(String, Vec<String>), String> {
    let bin = module_binary(p);
    // A stale prebuilt binary (source edited since it was built) counts as
    // absent so the module rebuilds instead of silently running old code.
    let bin_fresh = bin.as_ref().map(|b| b.exists() && !binary_is_stale(&p.directory, b)).unwrap_or(false);
    let use_bin = !force_rebuild && bin_fresh;
    if use_bin {
        let mut parts = vec![bin.unwrap().to_string_lossy().to_string()];
        append_conn_args(&mut parts, port, pin, &p.manifest.name);
        return Ok((parts.remove(0), parts));
    }
    if let Some(bin) = bin {
        build_module(p).await?;
        // Only a successful build is registered — a failed build never leaves a
        // route pointing at a binary that doesn't exist.
        record_binary_route(p, &bin);
        let mut parts = vec![bin.to_string_lossy().to_string()];
        append_conn_args(&mut parts, port, pin, &p.manifest.name);
        return Ok((parts.remove(0), parts));
    }
    let mut parts = build_module_command(p, port, pin);
    Ok((parts.remove(0), parts))
}

/// How a module launch resolves its command. The (potentially slow) build
/// happens inside `resolve_launch`, which the TUI runs on a background task so
/// the UI never blocks on a cold `cargo build`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// Run the prebuilt binary; rebuild only if it's missing or stale.
    Prebuilt,
    /// Force a rebuild from source. If it fails, roll back to the prebuilt
    /// binary (the "system issue" case) — running even a stale binary beats
    /// running nothing.
    Rebuild,
}

/// Resolve the (command, args) to launch a module under the given mode, doing
/// any build inline. Slow for cold builds — run this on a background task.
pub async fn resolve_launch(p: &Plugin, port: u16, pin: u32, mode: LaunchMode) -> Result<(String, Vec<String>), String> {
    match mode {
        LaunchMode::Prebuilt => module_run_parts(p, port, pin, false).await,
        LaunchMode::Rebuild => {
            match module_run_parts(p, port, pin, true).await {
                Ok(parts) => Ok(parts),
                Err(_) => run_force_binary(p, port, pin).ok_or_else(|| {
                    format!("rebuild failed for '{}' and there is no prebuilt binary to roll back to", p.manifest.name)
                }),
            }
        }
    }
}

/// Run the configured prebuilt binary directly (no build, no staleness check).
fn run_force_binary(p: &Plugin, port: u16, pin: u32) -> Option<(String, Vec<String>)> {
    let bin = module_binary(p)?;
    if !bin.exists() {
        return None;
    }
    let mut parts = vec![bin.to_string_lossy().to_string()];
    append_conn_args(&mut parts, port, pin, &p.manifest.name);
    Some((parts.remove(0), parts))
}

/// Extract `--pin <value>` from the resolved args and return (value, args
/// without the pin pair). The pin is moved off the command line (visible in
/// `ps`) and delivered to the module via the `COCKATIEL_PIN` env var instead.
fn strip_pin_from_args(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut pin = None;
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--pin" && i + 1 < args.len() {
            pin = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    (pin, out)
}

/// Spawn a non-terminal module's child from an already-resolved command.
/// Fast — called on the main loop once `resolve_launch` reports back.
/// stdin is set to null so a module can never consume the operator's TUI
/// keystrokes (modules that need interactive input use engine prompts instead).
/// The PIN is stripped from argv and injected as `COCKATIEL_PIN` instead.
pub fn spawn_from_parts(p: &Plugin, cmd: &str, args: &[String]) -> Result<Child, String> {
    let (pin, clean_args) = strip_pin_from_args(args);
    let mut command = Command::new(cmd);
    command
        .args(&clean_args)
        .current_dir(&p.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(pin) = pin {
        command.env("COCKATIEL_PIN", pin);
    }
    if let Some(cert) = engine_tls_cert_path() {
        command.env("COCKATIEL_TLS_CERT", cert);
    }
    // Own process group (PGID = child PID) so a group TERM/KILL later reaches
    // the module AND anything it spawns — no orphaned grandchildren.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .spawn()
        .map_err(|e| format!("Failed to launch '{}': {}", p.manifest.name, e))
}

/// Escape a string for embedding inside a SINGLE-quoted `sh -c '...'` string.
/// The value sits between literal single quotes, so a `'` in the value would
/// terminate the string and inject arbitrary commands (the module manifest's
/// untrusted `name`/`command_flags` land here). Escape it with the POSIX idiom
/// `'\''` — close the quote, emit an escaped literal quote, reopen. `\`, `"`
/// and backtick are literal inside single quotes but are still backslash-escaped
/// for defense-in-depth (a value may be re-embedded in a double-quoted context).
fn shell_quote(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('\'', "'\\''")
}

/// Encode ONE command-line element (command or arg) for the NESTED shell
/// wrapper the supervisor builds: the assembled `run` script is parsed by an
/// OUTER shell, whose `sh -c '...'` string is stripped and then RE-PARSED as
/// code by the INNER shell (the module's actual interpreter). An element must
/// therefore survive two shell passes as a single word — a bare value with `'`,
/// `;`, `&`, `|`, spaces etc. would be split or turned into command separators
/// when the inner shell re-parses it.
///
/// Do it in two steps:
///   1. wrap the value in single quotes with the POSIX `'` idiom for the INNER
///      shell (`'<value>'` → the whole value is ONE literal word for it), then
///   2. run the same idiom over that wrapping so the OUTER shell's single-quoted
///      `sh -c '...'` treats the inner quotes as literal text.
///
/// The value round-trips byte-for-byte and no metacharacter reaches a command
/// boundary in either shell.
fn nested_shell_quote(s: &str) -> String {
    let inner = format!("'{}'", s.replace('\'', "'\\''"));
    inner.replace('\'', "'\\''")
}

/// Escape a string for embedding inside a DOUBLE-quoted sh string (`cd "..."`).
/// `\`/`"`/`` ` ``/`$` are escaped so they stay literal. A `'` needs NO escaping
/// here and must NOT get the single-quote idiom — inside double quotes that
/// would decode to three literal quotes instead of one.
fn shell_double_quote(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
}

/// Escape a string for embedding inside a double-quoted AppleScript string
/// (`do script "..."`). AppleScript decodes `\` and `"`. A literal `'` needs no
/// escaping here — applying the POSIX single-quote idiom would corrupt the shell
/// syntax the embedded script contains.
fn apple_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Is a process with `pid` alive? Uses `kill -0` (signal 0, no-op probe) on
/// Unix. On Windows the pidfile mechanism isn't used, so this always reports
/// alive (terminal modules there are detected via the engine's liveness probe).
pub fn pid_alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Spawn a `terminal: true` module inside a terminal window so it gets a real
/// TTY (stdin/stdout). Cross-platform: macOS (Terminal.app), Linux (first
/// available terminal emulator), Windows (new console window).
///
/// Returns the child, a window marker ("cockatiel:<name>") and a pidfile path
/// so the supervisor can close that exact window and kill the real process
/// later. On macOS the command is dispatched to Terminal.app; the module is
/// `exec`'d over the window's shell, so the pid written to the pidfile IS the
/// module's process id. The marker is STABLE (no per-launch UUID) so a
/// relaunch REUSES the module's existing window instead of opening a fresh one
/// every time — Terminal.app is unreliable about programmatic window close, so
/// a close-then-reopen launch can stack windows (e.g. during a crash-loop).
/// Any stale process for the same module is killed first, so a module never
/// ends up with two instances.
pub fn spawn_terminal_from_parts(
    p: &Plugin,
    cmd: &str,
    args: &[String],
) -> Result<(Child, Option<String>, Option<PathBuf>), String> {
    // The PIN must not appear in the shell command line (visible in `ps`);
    // export it in the wrapper script instead.
    let (pin, clean_args) = strip_pin_from_args(args);
    let pin_export = match pin {
        Some(pin) => format!("export COCKATIEL_PIN='{}'; ", shell_quote(&pin)),
        None => String::new(),
    };
    // Point the module at the engine's TLS cert so it connects over WSS.
    let tls_export = match engine_tls_cert_path() {
        Some(cert) => format!("export COCKATIEL_TLS_CERT='{}'; ", shell_quote(&cert.to_string_lossy())),
        None => String::new(),
    };
    // EVERY element is individually quoted before joining: the command line is
    // embedded inside the nested `sh -c '...'` wrapper below, where each element
    // must survive BOTH the outer shell's quote-stripping AND the inner shell's
    // re-parse — nested_shell_quote makes each one a single literal word in both
    // passes, so a malicious value can't terminate the wrapper, split into extra
    // commands, or smuggle `;`/`&`/`|` to a command boundary.
    let cmd_line = std::iter::once(nested_shell_quote(cmd))
        .chain(clean_args.iter().map(|a| nested_shell_quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    let dir = p.directory.to_string_lossy().to_string();
    // Stable marker: the module's window is found and REUSED by this name on
    // every launch, and closed by it on kill. A per-launch UUID would leave
    // relaunched modules unable to find (or close) their own window.
    let marker = format!("cockatiel:{}", p.manifest.name);
    // PER-LAUNCH pidfile: a relaunch must never read the previous instance's
    // stale pid. A single shared `cockatiel-<name>.pid` meant the freshly
    // spawned monitor could read the OLD dead pid before the new shell wrote
    // its own → a false "starting" crash → spurious rebuild loop (and the
    // rebuild's kill could even hit a still-starting instance).
    let pidfile = std::env::temp_dir().join(format!(
        "cockatiel-{}-{}.pid",
        p.manifest.name,
        uuid::Uuid::now_v7()
    ));
    // Title the tab with the marker, then run the module in the FOREGROUND so a
    // full-screen TUI module owns the terminal (backgrounding a TUI module
    // breaks it: the shell gives the bg job /dev/null stdin, so ratatui fails
    // with ENXIO and the module exits). A tiny `sh -c` wrapper writes its OWN
    // pid (`$$`) then `exec`s the module — so the pidfile ends up holding the
    // module's real pid, while the window's outer shell stays alive (required
    // for the window to close later: Terminal.app refuses to close a window
    // whose shell has exited).
    let run = format!(
        "{}{}printf '\\033]0;{}\\007'; cd \"{}\" && sh -c 'echo $$ > \"{}\"; exec {}'",
        tls_export,
        pin_export,
        shell_quote(&marker),
        shell_double_quote(&dir),
        shell_quote(&pidfile.to_string_lossy()),
        cmd_line,
    );

    match std::env::consts::OS {
        "macos" => {
            // Dedupe: kill any lingering process from a previous launch of this
            // module (scanning its per-launch pidfiles), then REUSE the module's
            // existing Terminal window (if any) for the relaunch instead of
            // opening a new one. Opening a fresh window per launch is what
            // stacked windows: Terminal.app's programmatic close is slow/
            // unreliable, so a crash-loop relaunch outpaced the cleanup and
            // left stale windows behind. Reusing one window keeps exactly one
            // per module.
            kill_stale_terminal_processes(&p.manifest.name);
            let script = format!(
                "tell application \"Terminal\"\nactivate\nset wins to (every window whose name contains \"{}\")\nif (count of wins) is 0 then\ndo script \"{}\"\nelse\nset w to item 1 of wins\nrepeat with i from (count of wins) to 2 by -1\nclose (item i of wins) saving no\nend repeat\ndo script \"{}\" in w\nend if\nend tell",
                apple_quote(&marker),
                apple_quote(&run),
                apple_quote(&run),
            );
            let mut cmd = Command::new("osascript");
            cmd.arg("-e").arg(&script).stdout(Stdio::null()).stderr(Stdio::null());
            // Own process group (PGID = child PID) so a group TERM/KILL later
            // reaches the launcher and anything it spawned.
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            cmd.spawn()
                .map(|child| (child, Some(marker), Some(pidfile)))
                .map_err(|e| format!("Failed to launch '{}' in Terminal.app: {}", p.manifest.name, e))
        }
        "windows" => {
            Command::new("cmd")
                .args(["/C", "start", "", "cmd", "/K"])
                .arg(&run)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(|child| (child, None, None))
                .map_err(|e| format!("Failed to launch '{}' in a new console: {}", p.manifest.name, e))
        }
        "linux" => {
            // Try common terminal emulators in order of preference.
            let candidates: &[(&str, &[&str])] = &[
                ("x-terminal-emulator", &["-e", "sh", "-c"]),
                ("gnome-terminal", &["--", "sh", "-c"]),
                ("konsole", &["-e", "sh", "-c"]),
                ("xterm", &["-e", "sh", "-c"]),
            ];
            for (emu, args) in candidates {
                let result = {
                    let mut cmd = Command::new(emu);
                    cmd.args(*args).arg(&run).stdout(Stdio::null()).stderr(Stdio::null());
                    // Own process group (PGID = child PID) so a group TERM/KILL
                    // later reaches the emulator AND the module it spawns.
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::CommandExt;
                        cmd.process_group(0);
                    }
                    cmd.spawn()
                };
                match result {
                    Ok(child) => return Ok((child, None, None)),
                    Err(_) => continue,
                }
            }
            Err(format!(
                "Failed to launch '{}' in a terminal: no supported terminal emulator found (tried x-terminal-emulator, gnome-terminal, konsole, xterm). Launch it manually from the module directory.",
                p.manifest.name
            ))
        }
        other => Err(format!(
            "Terminal module '{}' launch not supported on OS '{}' — launch it manually.",
            p.manifest.name, other
        )),
    }
}

/// Kill the real module process recorded in `pidfile` (macOS terminal modules).
/// The pidfile holds the shell's pid, and the module was `exec`'d over the
/// shell, so this is the module's pid. TERM first, escalate to KILL.
fn kill_terminal_process(pidfile: &Path) {
    let pid: i32 = match std::fs::read_to_string(pidfile)
        .ok()
        .and_then(|s| s.trim().parse().ok())
    {
        Some(pid) => pid,
        None => return,
    };
    kill_pid(pid);
}

/// TERM then (if still alive) KILL a pid, waiting briefly between signals.
fn kill_pid(pid: i32) {
    for (signal, wait) in [("TERM", 800u64), ("KILL", 400u64)] {
        let _ = Command::new("kill")
            .arg(format!("-{}", signal))
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let mut gone = false;
        for _ in 0..wait / 100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let alive = Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                gone = true;
                break;
            }
        }
        if gone {
            break;
        }
    }
}

/// The `kill(1)` argv for signaling a whole process GROUP: `kill -<signal> -<pgid>`.
/// The leading `-` on the pgid is what makes kill address the group (a negative
/// pid), so the child AND its descendants all receive the signal together.
fn group_signal_args(signal: &str, pgid: i32) -> Vec<String> {
    vec![format!("-{}", signal), format!("-{}", pgid)]
}

/// Signal an entire process group. `pgid` is the group leader's pid — the
/// child's own pid, since every supervisor child is spawned with
/// `process_group(0)`. Best-effort; failures are ignored.
fn kill_process_group(pgid: i32, signal: &str) {
    let _ = Command::new("kill")
        .args(group_signal_args(signal, pgid))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Wait up to `timeout` for `child` to exit, polling `try_wait` in 100 ms
/// steps. Returns true once the child is reaped (or can no longer be
/// inspected); false if it is still running when the deadline passes.
fn wait_for_child(child: &mut Child, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            // Already reaped / can't be inspected — treat as gone.
            Err(_) => return true,
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Kill any still-alive module process left behind by a PREVIOUS launch of the
/// same terminal module, by scanning its per-launch pidfiles
/// (`cockatiel-<name>-*.pid`). This is the launch-time dedupe now that pidfiles
/// are unique per launch (a single shared pidfile caused the stale-pid race).
fn kill_stale_terminal_processes(name: &str) {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let prefix = format!("cockatiel-{}-", name);
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname) = fname.to_str() else { continue };
        if !fname.starts_with(&prefix) || !fname.ends_with(".pid") {
            continue;
        }
        let pid: i32 = match std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            Some(pid) => pid,
            None => continue,
        };
        let alive = Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if alive {
            crate::app::supervisor_log_global(format!(
                "[supervisor] killing stale {} process (pid {})",
                name, pid
            ));
            kill_pid(pid);
        }
    }
}

/// Best-effort close of every Terminal.app window whose title contains
/// `marker` (macOS only). Terminal only reliably closes the FRONT window, so we
/// bring the marker window to the front (`frontmost`), verify it really is
/// window 1, and only then close it — never touching a user's unrelated
/// window. `saving no` force-closes. Terminal is slow/reluctant, so this
/// retries for several seconds. The module process should already be dead.
/// Close every pop-out window this TUI opened. The detached windows are
/// separate processes (reparented to init, so the parent cannot wait on them),
/// and they talk to THIS process — so on a clean exit they would linger,
/// reconnecting to a port that is about to disappear. The window title is the
/// same `cockatiel:popout:<name>` marker `spawn_in_new_terminal` sets.
pub fn close_popout_windows(names: &[String]) {
    for name in names {
        close_terminal_windows(&format!("cockatiel:popout:{}", name));
    }
}

pub fn close_terminal_windows(marker: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("osascript")
            .arg("-e")
            .arg("tell application \"Terminal\" to activate")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(std::time::Duration::from_millis(300));

        for _ in 0..16 {
            // Bring the marker window to the front. Terminal needs ~0.5s to
            // process the reorder before `close window 1` will work.
            let _ = Command::new("osascript")
                .arg("-e")
                .arg("tell application \"Terminal\"")
                .arg("-e")
                .arg(format!(
                    "set frontmost of (first window whose name contains \"{}\") to true",
                    apple_quote(marker)
                ))
                .arg("-e")
                .arg("end tell")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            std::thread::sleep(std::time::Duration::from_millis(500));

            // Only close window 1 if it is actually the marker window.
            let name = Command::new("osascript")
                .arg("-e")
                .arg("tell application \"Terminal\" to get name of window 1")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default();
            if !name.contains(marker) {
                // Not the marker window (or already gone) — break rather than
                // risk closing a user's unrelated window.
                break;
            }
            let _ = Command::new("osascript")
                .arg("-e")
                .arg("tell application \"Terminal\" to close window 1 saving no")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            std::thread::sleep(std::time::Duration::from_millis(500));

            // Stop early once the marker window is gone.
            let remaining = Command::new("osascript")
                .arg("-e")
                .arg(format!(
                    "tell application \"Terminal\" to get name of (every window whose name contains \"{}\")",
                    apple_quote(marker)
                ))
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default();
            if remaining.trim().is_empty() {
                break;
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = marker;
    }
}

/// Write a module into the engine's modules.json. Modules are registered as
/// known but NOT auto-authorized: the engine sends an "allow this module to
/// connect?" prompt (routed to the TUI) on the first connect, then persists
/// the approval.
pub fn register_module(name: &str, position: &str, priority: i32) {
    let path = modules_registry_path();
    let mut registry: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default();

    if let Some(existing) = registry.iter_mut().find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name)) {
        existing["position"] = serde_json::json!(position);
        existing["priority"] = serde_json::json!(priority);
        // Preserve a previously granted approval — only brand-new modules are
        // registered as not-auto-authorized so the first connect prompts.
    } else {
        registry.push(serde_json::json!({
            "name": name,
            "instance_uuid7": uuid::Uuid::now_v7().to_string(),
            "position": position,
            "priority": priority,
            "auto_auth": false,
            "auth_token": ""
        }));
    }

    if let Ok(pretty) = serde_json::to_string_pretty(&registry) {
        let _ = write_atomic_0600(&path, &pretty);
    }
}

/// Register a module that is already trusted (e.g. a duplicate copy of an
/// approved module) with `auto_auth: true`, so its first connection is approved
/// without a fresh operator prompt.
pub fn register_module_approved(name: &str, position: &str, priority: i32) {
    let path = modules_registry_path();
    let mut registry: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default();

    if let Some(existing) = registry.iter_mut().find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name)) {
        existing["position"] = serde_json::json!(position);
        existing["priority"] = serde_json::json!(priority);
        existing["auto_auth"] = serde_json::json!(true);
    } else {
        registry.push(serde_json::json!({
            "name": name,
            "instance_uuid7": uuid::Uuid::now_v7().to_string(),
            "position": position,
            "priority": priority,
            "auto_auth": true,
            "auth_token": ""
        }));
    }

    if let Ok(pretty) = serde_json::to_string_pretty(&registry) {
        let _ = write_atomic_0600(&path, &pretty);
    }
}

/// Map a plugin's `capabilities` string to a config.json ordering list key.
fn config_list_key(capabilities: &str) -> &'static str {
    match capabilities {
        "input" | "inputs" | "connection" => "inputs",
        "preprocess" => "preprocessModules",
        "inprocess" => "inprocessModules",
        "postprocess" | "output" | "outputs" | "display" => "postprocessModules",
        _ => "preprocessModules",
    }
}

/// Insert the plugin into the engine's config.json ordering list (by capability).
/// Creates the file / key if missing; preserves other fields.
pub fn add_to_ordering(name: &str, capabilities: &str, priority: i32) {
    let path = engine_config_path();
    let mut root: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let key = config_list_key(capabilities);
    let entry = serde_json::json!({ "name": name, "priority": priority });

    if root.get(key).is_none() {
        root[key] = serde_json::json!([]);
    }
    if let Some(list) = root[key].as_array_mut() {
        if let Some(existing) = list.iter_mut().find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name)) {
            existing["priority"] = serde_json::json!(priority);
        } else {
            list.push(entry);
        }
    }

    if let Ok(pretty) = serde_json::to_string_pretty(&root) {
        let _ = write_atomic_0600(&path, &pretty);
    }
}

/// Remove the plugin from the engine's config.json ordering lists.
pub fn remove_from_ordering(name: &str) {
    let path = engine_config_path();
    let Ok(data) = std::fs::read_to_string(&path) else { return };
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&data) else { return };
    for key in ["inputs", "preprocessModules", "inprocessModules", "postprocessModules"] {
        if let Some(list) = root.get_mut(key).and_then(|v| v.as_array_mut()) {
            list.retain(|e| e.get("name").and_then(|v| v.as_str()) != Some(name));
        }
    }
    if let Ok(pretty) = serde_json::to_string_pretty(&root) {
        let _ = write_atomic_0600(&path, &pretty);
    }
}

/// The priority a moved module keeps when its ordering entry has none recorded.
///
/// The same default `add_to_ordering` uses for a module with no explicit
/// priority, so a stage move never silently drops an operator-tuned value but
/// also never invents a fancier one for an entry that was written bare.
const DEFAULT_PRIORITY: i32 = 100;

/// The recorded priority of `name` in the ordering list for `from`, if it has
/// one. Read from the CURRENT stage's list so an operator-tuned priority
/// survives a stage move rather than being reset to the default.
fn read_priority(root: &serde_json::Value, name: &str, from: &str) -> Option<i32> {
    root.get(config_list_key(from))?
        .as_array()?
        .iter()
        .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name))
        .and_then(|e| e.get("priority").and_then(|v| v.as_i64()).map(|p| p as i32))
}

/// The move one Shift+arrow resolves to, once the engine's config ordering is
/// known. Split from the resolver so the two ways a move can end (a jump into a
/// different stage, or a reorder within the in-process chain) are matchable on
/// their own instead of as a bundle of strings.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StageMove {
    /// Move into a different stage. `to` is `preprocess`|`inprocess`|
    /// `postprocess`.
    JumpTo { to: &'static str },
    /// Reorder within in-process: place `name` immediately before `other`.
    Before { other: String },
    /// Reorder within in-process: place `name` immediately after `other`.
    After { other: String },
}

/// Resolve a Shift+arrow DIRECTION against the engine's config ordering.
///
/// The semantics come from what each stage IS:
///  - pre-process and post-process are UNORDERED async fanouts, so a shift
///    toward in-process from either is a single JUMP into in-process (appended
///    at the end), and the reverse direction is a no-op — there is nothing
///    earlier than pre, nothing later than post.
///  - in-process is an ORDERED sequential chain, so within it a shift
///    REORDERS: shifted up swaps with the module directly above it, shifted
///    down swaps with the module directly below it. A module already at the
///    chain's head (up) or tail (down) falls out into the neighbouring
///    UNORDERED stage, where it is inserted alphabetically (the only
///    deterministic order an unordered stage has).
///  - an `input` adapter feeds the pipeline rather than running inside it, so
///    it is never moved in either direction.
///
/// `from` is the module's CURRENT position as the engine reports it (`output`
/// is the engine's alias for post-process, the same mapping the modules
/// window's `group_for_position` uses). The in-process ORDER comes from the
/// `inprocessModules` list, which is why this lives here and not in the window:
/// the window's `module_entries` are alphabetical and have no chain order.
/// Returns `None` for a no-op.
fn resolve_stage_move(
    root: &serde_json::Value,
    name: &str,
    from: &str,
    direction: StageDirection,
) -> Option<StageMove> {
    let from = if from == "output" { "postprocess" } else { from };
    if from == "input" {
        return None;
    }
    let members = |key: &str| -> Vec<String> {
        root.get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    match (from, direction) {
        ("preprocess", StageDirection::Earlier) => None,
        ("postprocess", StageDirection::Later) => None,
        ("preprocess", StageDirection::Later) => Some(StageMove::JumpTo { to: "inprocess" }),
        ("postprocess", StageDirection::Earlier) => Some(StageMove::JumpTo { to: "inprocess" }),
        ("inprocess", StageDirection::Earlier) => {
            let chain = members("inprocessModules");
            let idx = chain.iter().position(|m| m == name)?;
            if idx == 0 {
                // At the head of the chain: fall out into the unordered
                // pre-process stage (alphabetical insertion).
                Some(StageMove::JumpTo { to: "preprocess" })
            } else {
                // Swap with the module directly above: `name` ends up
                // immediately BEFORE the module that was above it.
                Some(StageMove::Before { other: chain[idx - 1].clone() })
            }
        }
        ("inprocess", StageDirection::Later) => {
            let chain = members("inprocessModules");
            let idx = chain.iter().position(|m| m == name)?;
            if idx + 1 >= chain.len() {
                // At the tail of the chain: fall out into the unordered
                // post-process stage (alphabetical insertion).
                Some(StageMove::JumpTo { to: "postprocess" })
            } else {
                // Swap with the module directly below: `name` ends up
                // immediately AFTER the module that was below it.
                Some(StageMove::After { other: chain[idx + 1].clone() })
            }
        }
        _ => None,
    }
}

/// Apply a stage JUMP: remove `name` from every ordering list, then insert it
/// into the target stage's list, keeping its recorded priority. In-process is
/// appended (the chain grows at the end); pre/post are inserted alphabetically
/// because those stages are unordered and alphabetical is the deterministic
/// order.
fn apply_stage_jump(root: &mut serde_json::Value, name: &str, from: &str, to: &str) {
    let priority = read_priority(root, name, from).unwrap_or(DEFAULT_PRIORITY);
    for key in ["inputs", "preprocessModules", "inprocessModules", "postprocessModules"] {
        if let Some(list) = root.get_mut(key).and_then(|v| v.as_array_mut()) {
            list.retain(|e| e.get("name").and_then(|v| v.as_str()) != Some(name));
        }
    }
    let key = config_list_key(to);
    if root.get(key).is_none() {
        root[key] = serde_json::json!([]);
    }
    if let Some(list) = root[key].as_array_mut() {
        let entry = serde_json::json!({ "name": name, "priority": priority });
        if to == "preprocess" || to == "postprocess" {
            let pos = list.iter().position(|e| {
                e.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n > name)
                    .unwrap_or(false)
            });
            match pos {
                Some(i) => list.insert(i, entry),
                None => list.push(entry),
            }
        } else {
            list.push(entry);
        }
    }
}

/// Apply an in-process REORDER: move `name` so it sits immediately before
/// (`before`) or after (`after`) `other` within the in-process chain. The entry
/// itself is moved, so its recorded fields (priority) survive untouched.
fn apply_inprocess_reorder(root: &mut serde_json::Value, name: &str, other: &str, before: bool) {
    let Some(list) = root.get_mut("inprocessModules").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let Some(idx) = list
        .iter()
        .position(|e| e.get("name").and_then(|v| v.as_str()) == Some(name))
    else {
        return;
    };
    let entry = list.remove(idx);
    // `other`'s position is found AFTER the removal, so the reorder is relative
    // to the post-removal chain (removing `name` can shift `other` by one).
    if let Some(oi) = list
        .iter()
        .position(|e| e.get("name").and_then(|v| v.as_str()) == Some(other))
    {
        let insert_at = if before { oi } else { oi + 1 };
        list.insert(insert_at.min(list.len()), entry);
    } else {
        // `other` vanished (defensive — a concurrent rewrite); put `name` back
        // where it was rather than dropping it.
        list.insert(idx.min(list.len()), entry);
    }
}

/// Move a module between/within pipeline stages by a Shift+arrow DIRECTION
/// rather than by a pre-computed target stage.
///
/// `path: None` targets the live engine `config.json` (`engine_config_path()`);
/// `Some` points at a temp file so unit tests never touch the real engine. `from`
/// is the module's CURRENT position as the engine reports it. Reads the four
/// ordering lists, resolves the move against them, rewrites `config.json`, and
/// returns the module's NEW position — `None` when the requested move is a
/// no-op (an `input` adapter, or a module already at the requested stage edge).
///
/// This is the ONLY place the Shift+arrow move is decided. The in-process
/// order lives in the `inprocessModules` list, and the window has no access to
/// it, so the window sends the direction and this resolver does the rest. The
/// engine's config-poll task re-reads the three pipeline lists on change, so a
/// running engine picks the move up live.
pub fn move_module_by_direction(
    path: Option<&Path>,
    name: &str,
    from: &str,
    direction: StageDirection,
) -> Result<Option<String>, String> {
    let live_path;
    let path: &Path = match path {
        Some(p) => p,
        None => {
            live_path = engine_config_path();
            &live_path
        }
    };
    let data = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let mut root: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| format!("cannot parse {}: {}", path.display(), e))?;

    let Some(move_) = resolve_stage_move(&root, name, from, direction) else {
        return Ok(None);
    };
    match move_ {
        StageMove::JumpTo { to } => {
            apply_stage_jump(&mut root, name, from, to);
            if let Ok(pretty) = serde_json::to_string_pretty(&root) {
                let _ = write_atomic_0600(path, &pretty);
            }
            Ok(Some(to.to_string()))
        }
        StageMove::Before { other } => {
            apply_inprocess_reorder(&mut root, name, &other, true);
            if let Ok(pretty) = serde_json::to_string_pretty(&root) {
                let _ = write_atomic_0600(path, &pretty);
            }
            Ok(Some("inprocess".to_string()))
        }
        StageMove::After { other } => {
            apply_inprocess_reorder(&mut root, name, &other, false);
            if let Ok(pretty) = serde_json::to_string_pretty(&root) {
                let _ = write_atomic_0600(path, &pretty);
            }
            Ok(Some("inprocess".to_string()))
        }
    }
}

/// A managed running process (module or engine).
pub struct ManagedProcess {
    pub child: Child,
    /// macOS Terminal.app window marker ("cockatiel:<name>") this module was
    /// launched in — closed on kill so stopping a terminal module also closes
    /// its window. None for non-terminal modules.
    pub terminal_window: Option<String>,
    /// Path to the pid file written by the terminal module's shell (the module
    /// is exec'd over the shell, so the pid IS the module's). Used to kill the
    /// real process, since the stored child is the osascript launcher.
    pub terminal_pidfile: Option<PathBuf>,
}

impl ManagedProcess {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn kill(&mut self) {
        // For terminal modules the child is the (already-exited) osascript
        // launcher — kill the real process via its pid file, then close the
        // window in the background (Terminal is slow to close, so this must not
        // block the UI loop). For regular modules the child is the process.
        if self.terminal_window.is_some() {
            if let Some(pidfile) = &self.terminal_pidfile {
                kill_terminal_process(pidfile);
            }
            if let Some(marker) = &self.terminal_window {
                let marker = marker.clone();
                std::thread::spawn(move || {
                    close_terminal_windows(&marker);
                });
            }
            #[cfg(unix)]
            {
                // Group TERM→KILL covers the launcher and anything it spawned.
                kill_process_group(self.pid() as i32, "TERM");
                wait_for_child(&mut self.child, std::time::Duration::from_secs(3));
                kill_process_group(self.pid() as i32, "KILL");
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            return;
        }
        #[cfg(unix)]
        {
            // Graceful shutdown: TERM the whole process group first so the
            // child (engine/user-db/module) and its descendants can flush
            // state (e.g. SQLite WAL), wait up to ~3s, then KILL anything
            // still alive. The group's PGID equals the child's pid because
            // every supervisor child is spawned with `process_group(0)`.
            kill_process_group(self.pid() as i32, "TERM");
            wait_for_child(&mut self.child, std::time::Duration::from_secs(3));
            kill_process_group(self.pid() as i32, "KILL");
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub type ProcessTable = HashMap<String, Arc<Mutex<ManagedProcess>>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::ModuleManifest;

    fn plugin_with_binary(dir: &Path, bin: &str, exists: bool) -> (Plugin, PathBuf) {
        let dir = dir.to_path_buf();
        let bin_path = dir.join(bin);
        if exists {
            std::fs::create_dir_all(dir.join("target").join("release")).unwrap();
            std::fs::write(&bin_path, "#!/bin/sh\n").unwrap();
        }
        // Route the current OS/arch to the binary (mirrors a written manifest).
        let mut routes = std::collections::HashMap::new();
        let mut arch_map = std::collections::HashMap::new();
        arch_map.insert(arch_key().to_string(), bin.to_string());
        routes.insert(os_key().to_string(), arch_map);
        let manifest = ModuleManifest {
            name: "test-mod".into(),
            description: String::new(),
            version: String::new(),
            capabilities: "output".into(),
            root_file: String::new(),
            launch_command: "cargo".into(),
            command_flags: vec!["run".into(), "--release".into()],
            terminal: false,
            credentials: vec![],
            binary: crate::plugins::BinaryRoutes(routes),
            build_command: Some("cargo".into()),
            build_flags: vec!["build".into(), "--release".into()],
        };
        (Plugin { manifest, directory: dir }, bin_path)
    }

    #[tokio::test]
    async fn prebuilt_binary_is_used_when_present() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-sup-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let (p, bin_path) = plugin_with_binary(&tmp, "target/release/test-mod", true);
        let (cmd, args) = resolve_launch(&p, 9734, 603936, LaunchMode::Prebuilt).await.unwrap();
        assert_eq!(cmd, bin_path.to_string_lossy());
        assert!(args.iter().any(|a| a == "--port"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn missing_binary_triggers_a_build() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-sup-test2-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let (p, _bin_path) = plugin_with_binary(&tmp, "target/release/test-mod", false);
        // Build will fail (no Cargo.toml in the temp dir) → Err is expected.
        let err = resolve_launch(&p, 9734, 603936, LaunchMode::Prebuilt).await.unwrap_err();
        assert!(err.contains("build failed"), "expected a build failure, got: {}", err);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn rebuild_rolls_back_to_a_stale_binary() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-sup-test7-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let (p, bin_path) = plugin_with_binary(&tmp, "target/release/test-mod", true);
        // No Cargo.toml → a forced rebuild fails → roll back to the binary,
        // ignoring staleness (exactly the "system issue" recovery case).
        let (cmd, _args) = resolve_launch(&p, 9734, 603936, LaunchMode::Rebuild).await.unwrap();
        assert_eq!(cmd, bin_path.to_string_lossy());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn binary_never_stale_without_source() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-sup-test3-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let bin = tmp.join("bin");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        // No Cargo.toml → pure-binary module → never stale.
        assert!(!binary_is_stale(&tmp, &bin));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn binary_stale_when_source_is_newer() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-sup-test4-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(tmp.join("src/main.rs"), "fn main() {}\n").unwrap();
        let bin = tmp.join("bin");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        // Age the binary to 2020; the source files keep "now".
        assert!(std::process::Command::new("touch").arg("-t").arg("202001010000").arg(&bin).status().unwrap().success());

        assert!(binary_is_stale(&tmp, &bin));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn binary_fresh_when_source_is_older() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-sup-test5-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(tmp.join("src/main.rs"), "fn main() {}\n").unwrap();
        let bin = tmp.join("bin");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        // Age the source to 2020; the binary keeps "now".
        assert!(std::process::Command::new("touch").arg("-t").arg("202001010000").arg(tmp.join("src/main.rs")).status().unwrap().success());

        assert!(!binary_is_stale(&tmp, &bin));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn clear_module_config_empties_values_keeps_keys() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-clear-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".env"), "# comment\nTOKEN=abc123\nSECRET=s3\n").unwrap();
        std::fs::write(
            tmp.join("config.json"),
            r#"{"model":"mms","port":9734,"servers":{"g":["a","b"]},"channels":["x"],"score":42}"#,
        )
        .unwrap();

        clear_module_config(&tmp).unwrap();

        // .env: keys kept, values emptied, comment preserved.
        let env = std::fs::read_to_string(tmp.join(".env")).unwrap();
        assert!(env.contains("# comment"), "env: {}", env);
        assert!(env.contains("TOKEN=\n"), "env: {}", env);
        assert!(env.contains("SECRET=\n"), "env: {}", env);
        assert!(!env.contains("abc123"), "env: {}", env);

        // config.json: keys/structure kept, scalar values → "", arrays → [].
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg["model"], "", "cfg: {}", cfg);
        assert_eq!(cfg["port"], "", "cfg: {}", cfg);
        assert_eq!(cfg["score"], "", "cfg: {}", cfg);
        assert_eq!(cfg["channels"], serde_json::json!([]), "cfg: {}", cfg);
        assert_eq!(cfg["servers"]["g"], serde_json::json!([]), "cfg: {}", cfg);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A temp `config.json` written from a JSON string, returned with its path.
    fn scratch_config(tag: &str, json: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("cockatiel-move-{}-{}", tag, uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("config.json");
        std::fs::write(&path, json).unwrap();
        (tmp, path)
    }

    /// The module names of an ordering list, in order, for legible assertions.
    fn names(list: &serde_json::Value) -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect()
    }

    #[test]
    fn a_pre_module_shifted_later_jumps_into_inprocess_at_the_end() {
        let (tmp, path) = scratch_config(
            "pre-later",
            r#"{"preprocessModules":[{"name":"clip","priority":100},{"name":"polling","priority":50}],"inprocessModules":[{"name":"banned-words","priority":100}]}"#,
        );
        let new =
            move_module_by_direction(Some(&path), "polling", "preprocess", StageDirection::Later).unwrap();
        assert_eq!(new.as_deref(), Some("inprocess"));
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(names(&root["preprocessModules"]), vec!["clip"]);
        assert_eq!(
            names(&root["inprocessModules"]),
            vec!["banned-words", "polling"],
            "an unordered stage's jump into in-process must be APPENDED at the end"
        );
        assert_eq!(root["inprocessModules"][1]["priority"], 50, "a tuned priority must survive the jump");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_post_module_shifted_earlier_jumps_into_inprocess_at_the_end() {
        let (tmp, path) = scratch_config(
            "post-earlier",
            r#"{"postprocessModules":[{"name":"clip","priority":100},{"name":"term","priority":10}],"inprocessModules":[{"name":"banned-words","priority":100}]}"#,
        );
        let new = move_module_by_direction(Some(&path), "term", "postprocess", StageDirection::Earlier).unwrap();
        assert_eq!(new.as_deref(), Some("inprocess"));
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(names(&root["postprocessModules"]), vec!["clip"]);
        assert_eq!(names(&root["inprocessModules"]), vec!["banned-words", "term"]);
        assert_eq!(root["inprocessModules"][1]["priority"], 10);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_inprocess_module_shifted_up_swaps_with_the_one_above() {
        let (tmp, path) = scratch_config(
            "in-up",
            r#"{"inprocessModules":[{"name":"alpha","priority":100},{"name":"bravo","priority":100},{"name":"charlie","priority":100}]}"#,
        );
        let new = move_module_by_direction(Some(&path), "bravo", "inprocess", StageDirection::Earlier).unwrap();
        assert_eq!(new.as_deref(), Some("inprocess"));
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            names(&root["inprocessModules"]),
            vec!["bravo", "alpha", "charlie"],
            "shifted up must swap with the module directly above"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_inprocess_module_shifted_down_swaps_with_the_one_below() {
        let (tmp, path) = scratch_config(
            "in-down",
            r#"{"inprocessModules":[{"name":"alpha","priority":100},{"name":"bravo","priority":100},{"name":"charlie","priority":100}]}"#,
        );
        let new = move_module_by_direction(Some(&path), "bravo", "inprocess", StageDirection::Later).unwrap();
        assert_eq!(new.as_deref(), Some("inprocess"));
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            names(&root["inprocessModules"]),
            vec!["alpha", "charlie", "bravo"],
            "shifted down must swap with the module directly below"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_first_inprocess_module_shifted_up_falls_out_into_preprocess_alphabetically() {
        let (tmp, path) = scratch_config(
            "in-head",
            r#"{"preprocessModules":[{"name":"zebra","priority":100},{"name":"apple","priority":100}],"inprocessModules":[{"name":"alpha","priority":100},{"name":"bravo","priority":100}]}"#,
        );
        let new = move_module_by_direction(Some(&path), "alpha", "inprocess", StageDirection::Earlier).unwrap();
        assert_eq!(new.as_deref(), Some("preprocess"));
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(names(&root["inprocessModules"]), vec!["bravo"]);
        assert_eq!(
            names(&root["preprocessModules"]),
            vec!["alpha", "zebra", "apple"],
            "pre-process is unordered, so the module must be inserted alphabetically"
        );
        assert_eq!(root["preprocessModules"][0]["priority"], 100);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_last_inprocess_module_shifted_down_falls_out_into_postprocess_alphabetically() {
        let (tmp, path) = scratch_config(
            "in-tail",
            r#"{"inprocessModules":[{"name":"alpha","priority":100},{"name":"bravo","priority":100}],"postprocessModules":[{"name":"zebra","priority":100},{"name":"apple","priority":100}]}"#,
        );
        let new = move_module_by_direction(Some(&path), "bravo", "inprocess", StageDirection::Later).unwrap();
        assert_eq!(new.as_deref(), Some("postprocess"));
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(names(&root["inprocessModules"]), vec!["alpha"]);
        assert_eq!(
            names(&root["postprocessModules"]),
            vec!["bravo", "zebra", "apple"],
            "post-process is unordered, so the module must be inserted alphabetically"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_input_adapter_or_a_stage_edge_is_a_noop_in_either_direction() {
        // input adapters feed the pipeline rather than running inside it.
        let (tmp, path) = scratch_config("input", r#"{"inputs":[{"name":"discord","priority":100}]}"#);
        assert_eq!(
            move_module_by_direction(Some(&path), "discord", "input", StageDirection::Earlier).unwrap(),
            None
        );
        assert_eq!(
            move_module_by_direction(Some(&path), "discord", "input", StageDirection::Later).unwrap(),
            None
        );
        // pre + Earlier: nothing earlier than pre.
        let (tmp2, path2) = scratch_config("pre-edge", r#"{"preprocessModules":[{"name":"clip","priority":100}]}"#);
        assert_eq!(
            move_module_by_direction(Some(&path2), "clip", "preprocess", StageDirection::Earlier).unwrap(),
            None
        );
        // post + Later: nothing later than post.
        let (tmp3, path3) = scratch_config("post-edge", r#"{"postprocessModules":[{"name":"term","priority":100}]}"#);
        assert_eq!(
            move_module_by_direction(Some(&path3), "term", "postprocess", StageDirection::Later).unwrap(),
            None
        );
        for t in [&tmp, &tmp2, &tmp3] {
            let _ = std::fs::remove_dir_all(t);
        }
    }

    #[test]
    fn output_is_treated_as_postprocess_for_the_move() {
        // The engine reports the post-process stage as `output` sometimes; the
        // resolver must not lose the move to the unknown-position fallback.
        let (tmp, path) = scratch_config(
            "output",
            r#"{"postprocessModules":[{"name":"term","priority":100}],"inprocessModules":[{"name":"banned-words","priority":100}]}"#,
        );
        let new = move_module_by_direction(Some(&path), "term", "output", StageDirection::Earlier).unwrap();
        assert_eq!(new.as_deref(), Some("inprocess"));
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(names(&root["inprocessModules"]), vec!["banned-words", "term"]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rotate_log_bounds_and_keeps_generations() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-sup-test6-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("engine.log");
        std::fs::write(&path, "0123456789").unwrap(); // 10 bytes

        // 5-byte cap → rotates into .1, fresh file is empty.
        rotate_log(&path, 5, 3);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(std::fs::read_to_string(tmp.join("engine.log.1")).unwrap(), "0123456789");

        // Second rotation shifts .1 → .2.
        std::fs::write(&path, "0123456789").unwrap();
        rotate_log(&path, 5, 3);
        assert_eq!(std::fs::read_to_string(tmp.join("engine.log.1")).unwrap(), "0123456789");
        assert_eq!(std::fs::read_to_string(tmp.join("engine.log.2")).unwrap(), "0123456789");

        // Under cap → untouched.
        rotate_log(&path, 5, 3);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Live (manual): launches a real Terminal.app window via the supervisor's
    /// terminal spawn, then verifies kill() kills the real process. Run with:
    /// cargo test --release live_terminal -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_terminal_spawn_and_kill() {
        let manifest = crate::plugins::ModuleManifest {
            name: "liveterm".into(),
            description: String::new(),
            version: String::new(),
            capabilities: String::new(),
            root_file: String::new(),
            launch_command: String::new(),
            command_flags: vec![],
            terminal: true,
            credentials: vec![],
            binary: Default::default(),
            build_command: None,
            build_flags: vec![],
        };
        let plugin = Plugin {
            manifest,
            directory: std::path::PathBuf::from("/tmp"),
        };
        let (child, marker, pidfile) = spawn_terminal_from_parts(&plugin, "/bin/sleep", &["90".to_string()])
            .expect("spawn");
        eprintln!("marker={:?} pidfile={:?}", marker, pidfile);
        std::thread::sleep(std::time::Duration::from_secs(2));
        let pid = std::fs::read_to_string(pidfile.as_ref().unwrap())
            .expect("pidfile written")
            .trim()
            .parse::<i32>()
            .expect("pid parses");
        eprintln!("module pid={}", pid);
        assert!(libc_kill_alive(pid), "module should be running");

        let mut proc = ManagedProcess {
            child,
            terminal_window: marker,
            terminal_pidfile: pidfile,
        };
        proc.kill();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!libc_kill_alive(pid), "module should be dead after kill()");
        eprintln!("process killed; waiting for async window close...");
        // The window close is BEST-EFFORT (Terminal.app is unreliable about
        // programmatic window close) and runs on a background thread — give it
        // time, but don't hard-fail on it.
        std::thread::sleep(std::time::Duration::from_secs(12));
        let remaining = std::process::Command::new("osascript")
            .arg("-e")
            .arg("tell application \"Terminal\" to get name of (every window whose name contains \"cockatiel:liveterm\")")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        eprintln!(
            "window close result: {} (best-effort — not a hard assertion)",
            if remaining.trim().is_empty() {
                "closed"
            } else {
                "still open (process is dead; user may close it)"
            }
        );
    }

    /// Live (manual): a crash + relaunch must REUSE the module's existing
    /// window — never stack a second one. This is the regression guard for the
    /// "term-chat opens 4 windows" bug. Run with:
    /// cargo test --release live_terminal_relaunch -- --ignored --nocapture
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn live_terminal_relaunch_reuses_window() {
        let manifest = crate::plugins::ModuleManifest {
            name: "liveterm".into(),
            description: String::new(),
            version: String::new(),
            capabilities: String::new(),
            root_file: String::new(),
            launch_command: String::new(),
            command_flags: vec![],
            terminal: true,
            credentials: vec![],
            binary: Default::default(),
            build_command: None,
            build_flags: vec![],
        };
        let plugin = Plugin {
            manifest,
            directory: std::path::PathBuf::from("/tmp"),
        };

        let count_windows = || {
            std::process::Command::new("osascript")
                .arg("-e")
                .arg("tell application \"Terminal\" to get name of (every window whose name contains \"cockatiel:liveterm\")")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default()
                .trim()
                .to_string()
        };

        // First launch: one window.
        let (_child1, marker1, pidfile1) =
            spawn_terminal_from_parts(&plugin, "/bin/sleep", &["90".to_string()]).expect("spawn 1");
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert_eq!(count_windows().lines().count(), 1, "first launch must open exactly one window");
        let pid1 = std::fs::read_to_string(pidfile1.as_ref().unwrap())
            .expect("pidfile 1 written")
            .trim()
            .parse::<i32>()
            .expect("pid parses");

        // Simulate a crash: kill the module's real process.
        kill_terminal_process(pidfile1.as_ref().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(600));
        assert!(!libc_kill_alive(pid1), "module should be dead after simulated crash");

        // Relaunch (the supervisor's crash ladder path): must reuse the SAME
        // window, not stack a second one.
        let (_child2, marker2, pidfile2) =
            spawn_terminal_from_parts(&plugin, "/bin/sleep", &["90".to_string()]).expect("spawn 2");
        std::thread::sleep(std::time::Duration::from_secs(2));
        let windows = count_windows();
        eprintln!("windows after relaunch: {:?}", windows);
        assert_eq!(windows.lines().count(), 1, "relaunch must reuse the existing window — found {} windows", windows.lines().count());
        let pid2 = std::fs::read_to_string(pidfile2.as_ref().unwrap())
            .expect("pidfile 2 written")
            .trim()
            .parse::<i32>()
            .expect("pid parses");
        assert!(libc_kill_alive(pid2), "relaunched module should be running");

        // Cleanup: kill the relaunched process, best-effort close windows.
        let mut proc = ManagedProcess {
            child: _child2,
            terminal_window: marker2,
            terminal_pidfile: pidfile2,
        };
        proc.kill();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!libc_kill_alive(pid2), "relaunched module should be dead after cleanup");
        let _ = marker1;
    }

    #[cfg(target_os = "macos")]
    fn libc_kill_alive(pid: i32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    fn libc_kill_alive(pid: i32) -> bool {
        let _ = pid;
        true
    }

    #[test]
    fn group_signal_args_targets_the_process_group() {
        // `kill -TERM -123` signals the whole group whose leader is pid 123 —
        // the leading `-` on the pgid is what selects the group.
        assert_eq!(group_signal_args("TERM", 123), vec!["-TERM", "-123"]);
        assert_eq!(group_signal_args("KILL", 456), vec!["-KILL", "-456"]);
    }

    #[test]
    fn shell_quote_escapes_single_quotes_for_sh_single_quote_context() {
        // The value is embedded inside `sh -c '...'`: a literal `'` would close
        // the string and inject commands. The POSIX idiom must round-trip it.
        assert_eq!(shell_quote("a'b"), "a'\\''b");
        assert_eq!(shell_quote("'; rm -rf ~; '"), "'\\''; rm -rf ~; '\\''");
        // Existing escaping is preserved alongside the new single-quote idiom.
        assert_eq!(shell_quote("a\"b`c\\d"), "a\\\"b\\`c\\\\d");
        // Everything else passes through untouched.
        assert_eq!(shell_quote("cargo run --release"), "cargo run --release");
    }

    #[test]
    fn nested_shell_quote_survives_both_shell_passes() {
        // A simple value becomes a single-quoted word wrapped again for the
        // outer shell's `sh -c '...'` string.
        assert_eq!(nested_shell_quote("foo bar"), "'\\''foo bar'\\''");
        // The inner sh must receive the value byte-for-byte.
        for v in ["plain", "with space", "semi;colon", "amp&ersand", "pipe|s", "quote'", "back`tick", "dollar$"] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("sh -c 'exec /bin/echo {}'", nested_shell_quote(v)))
                .output()
                .unwrap_or_else(|_| panic!("sh spawn for {:?}", v));
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                format!("{}\n", v),
                "value {:?} did not round-trip through both shells",
                v
            );
        }
    }

    #[test]
    fn terminal_cmd_line_quotes_every_element() {
        // Regression: malicious metacharacters in manifest-derived args must not
        // break out of the nested `sh -c '...'` wrapper.
        let malicious = [
            "x';touch /tmp/cockatiel-pwned;'".to_string(),
            "x&touch /tmp/cockatiel-pwned2".to_string(),
            "x|cat;touch /tmp/cockatiel-pwned3".to_string(),
        ];
        for arg in &malicious {
            let cmd_line = std::iter::once(nested_shell_quote("/bin/echo"))
                .chain(std::iter::once(nested_shell_quote(arg)))
                .collect::<Vec<_>>()
                .join(" ");
            // Mirrors the production wrapper (outer `sh -c <run>` → inner sh).
            let run = format!("sh -c 'echo $$ > /dev/null; exec {}'", cmd_line);
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&run)
                .output()
                .expect("sh spawn");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                format!("{}\n", arg),
                "arg {:?} did not round-trip",
                arg
            );
        }
        assert!(
            !std::path::Path::new("/tmp/cockatiel-pwned").exists()
                && !std::path::Path::new("/tmp/cockatiel-pwned2").exists()
                && !std::path::Path::new("/tmp/cockatiel-pwned3").exists(),
            "command injection executed a malicious payload"
        );
        let _ = std::fs::remove_file("/tmp/cockatiel-pwned");
        let _ = std::fs::remove_file("/tmp/cockatiel-pwned2");
        let _ = std::fs::remove_file("/tmp/cockatiel-pwned3");
    }

    #[test]
    fn shell_double_quote_and_apple_quote_leave_single_quotes_alone() {
        // The `cd "..."` and AppleScript `do script "..."` contexts must NOT get
        // the single-quote idiom — it would decode to three literal quotes.
        assert_eq!(shell_double_quote("a'b"), "a'b");
        assert_eq!(shell_double_quote("a\"b$c"), "a\\\"b\\$c");
        assert_eq!(apple_quote("sh -c 'echo hi'"), "sh -c 'echo hi'");
        assert_eq!(apple_quote("a\"b\\c"), "a\\\"b\\\\c");
    }

    #[cfg(unix)]
    #[test]
    fn nested_shell_quote_neutralizes_metacharacter_injection() {
        // `exec` alone doesn't stop `&`/`|` in an unquoted arg from splitting
        // into extra commands on the inner shell; nested_shell_quote must.
        for arg in ["x;touch /tmp/cockatiel-pwned;", "x&touch /tmp/cockatiel-pwned", "x|touch /tmp/cockatiel-pwned"] {
            let cmd_line = std::iter::once(nested_shell_quote("/bin/echo"))
                .chain(std::iter::once(nested_shell_quote(arg)))
                .collect::<Vec<_>>()
                .join(" ");
            let run = format!("sh -c 'echo $$ > /dev/null; exec {}'", cmd_line);
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&run)
                .output()
                .expect("sh spawn");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                format!("{}\n", arg),
                "arg {:?} did not round-trip",
                arg
            );
        }
        assert!(
            !std::path::Path::new("/tmp/cockatiel-pwned").exists(),
            "command injection executed a malicious payload"
        );
        let _ = std::fs::remove_file("/tmp/cockatiel-pwned");
    }

    #[test]
    fn write_atomic_0600_sets_mode_and_replaces() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-w0600-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("secrets.env");
        write_atomic_0600(&path, "TOKEN=abc").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "TOKEN=abc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Overwrite keeps atomicity + mode.
        write_atomic_0600(&path, "TOKEN=def").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "TOKEN=def");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // No temp files are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {:?}", leftovers.len());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The TUI's own `config.json` is what a fresh install reads its launch
    /// default from, so the writer that generates the file has to emit that key
    /// — and has to do it as a MERGE, because the same file is the operator's to
    /// put other settings in.
    #[test]
    fn the_tui_config_writer_emits_the_launch_default_without_clobbering() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-tuicfg-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("config.json");

        // Nothing said yet -> the built-in default, i.e. "launch the engine".
        assert_eq!(read_launch_engine_default(&path), None);

        // A fresh install: the file is generated, and it says the default.
        ensure_tui_config(&path);
        assert!(path.is_file(), "a fresh install must get a config.json");
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written[LAUNCH_ENGINE_KEY], serde_json::json!(true));
        assert_eq!(read_launch_engine_default(&path), Some(true));

        // Idempotent, and an operator's own value is never overwritten.
        ensure_tui_config(&path);
        assert_eq!(read_launch_engine_default(&path), Some(true));
        std::fs::write(&path, r#"{"launch_engine": false, "operator_setting": 7}"#).unwrap();
        ensure_tui_config(&path);
        assert_eq!(read_launch_engine_default(&path), Some(false), "--no-engine by config must survive a write");
        let merged: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(merged["operator_setting"], serde_json::json!(7), "another key must survive");

        // A key added later still lands in a file that predates it...
        std::fs::write(&path, r#"{"operator_setting": 7}"#).unwrap();
        ensure_tui_config(&path);
        assert_eq!(read_launch_engine_default(&path), Some(true));
        let merged: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(merged["operator_setting"], serde_json::json!(7));

        // ...and a file we cannot read as an object is left completely alone
        // rather than replaced. A missing default is recoverable; a clobbered
        // operator config is not.
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        ensure_tui_config(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1, 2, 3]");
        assert_eq!(read_launch_engine_default(&path), None, "an array is not a config object");

        // A non-boolean value is "nothing said", never a guessed `false`.
        std::fs::write(&path, r#"{"launch_engine": "yes"}"#).unwrap();
        assert_eq!(read_launch_engine_default(&path), None);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The TUI's `config.json` sits in the same directory `find_engine_addr`
    /// probes for an engine address. If that probe answered from a file holding
    /// none of the address keys, every operator would silently get the built-in
    /// default port — and the engine's real port would never be discovered.
    #[test]
    fn the_tui_config_does_not_hijack_engine_address_discovery() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-addr-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        // The TUI's own file, written by the writer above.
        let tui = tmp.join("config.json");
        ensure_tui_config(&tui);
        let tui_config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&tui).unwrap()).unwrap();
        let has_addr_key = ["engine_ip", "engine_port", "engine_pin"]
            .iter()
            .any(|k| tui_config.get(*k).is_some());
        assert!(!has_addr_key, "the TUI config must not look like an engine-address file");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn upsert_env_key_generates_and_preserves() {        let tmp = std::env::temp_dir().join(format!("cockatiel-upsert-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join(".env");
        std::fs::write(&path, "# comment\nPORT=9736\n").unwrap();

        // Updates an existing key, preserves others + comments.
        upsert_env_key(&path, "PORT", "9740");
        // Inserts a new key.
        upsert_env_key(&path, "USER_DB_TOKEN", "tok-123");
        // Reads back through the same reader the supervisor uses.
        assert_eq!(read_env_value(&path, "USER_DB_TOKEN").as_deref(), Some("tok-123"));
        assert_eq!(read_env_value(&path, "PORT").as_deref(), Some("9740"));

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# comment"), "content: {}", content);
        assert!(!content.contains("9736"), "old PORT value not replaced: {}", content);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // Creating the file from scratch (missing parent dir) also works.
        let nested = tmp.join("nested").join("sub").join(".env");
        upsert_env_key(&nested, "USER_DB_TOKEN", "tok-456");
        assert_eq!(read_env_value(&nested, "USER_DB_TOKEN").as_deref(), Some("tok-456"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn registered_engine_identity_returns_matching_entry() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-regid-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("modules.json");
        std::fs::write(
            &path,
            r#"[
              {"name":"other-mod","instance_uuid7":"00000000-0000-7000-8000-000000000002","auth_token":"tok-other","position":"5","priority":2,"auto_auth":true},
              {"name":"cockatiel-tui","instance_uuid7":"00000000-0000-7000-8000-000000000001","auth_token":"tok-tui","position":"4","priority":1,"auto_auth":true}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            registered_engine_identity_at(&path, "cockatiel-tui"),
            Some(("00000000-0000-7000-8000-000000000001".to_string(), "tok-tui".to_string()))
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn registered_engine_identity_missing_file_or_name_is_none() {
        let tmp = std::env::temp_dir().join(format!("cockatiel-regid2-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();

        // Missing file → None.
        let missing = tmp.join("does-not-exist.json");
        assert_eq!(registered_engine_identity_at(&missing, "cockatiel-tui"), None);

        // Missing name → None.
        let path = tmp.join("modules.json");
        std::fs::write(
            &path,
            r#"[
              {"name":"other-mod","instance_uuid7":"00000000-0000-7000-8000-000000000002","auth_token":"tok-other","position":"5","priority":2,"auto_auth":true}
            ]"#,
        )
        .unwrap();
        assert_eq!(registered_engine_identity_at(&path, "cockatiel-tui"), None);

        // Entry with empty uuid/token is treated as no identity.
        std::fs::write(
            &path,
            r#"[
              {"name":"cockatiel-tui","instance_uuid7":"","auth_token":"tok-tui","position":"4","priority":1,"auto_auth":true}
            ]"#,
        )
        .unwrap();
        assert_eq!(registered_engine_identity_at(&path, "cockatiel-tui"), None);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}