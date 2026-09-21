use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::plugins::Plugin;

/// Write a file atomically: write to a temp sibling then rename over the
/// target. `modules.json`/`config.json` are written by BOTH the TUI supervisor
/// and the engine — a torn write must never leave a half-written JSON.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, content)?;
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
        std::fs::write(&env_path, out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
        }
    }

    let json_path = dir.join("config.json");
    if let Ok(content) = std::fs::read_to_string(&json_path) {
        if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&content) {
            clear_json_values(&mut root);
            if let Ok(pretty) = serde_json::to_string_pretty(&root) {
                let _ = std::fs::write(&json_path, pretty);
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
    Command::new(&binary)
        .current_dir(&dir)
        .env("USER_DB_HOST", "127.0.0.1")
        .env("USER_DB_PORT", USER_DB_DEFAULT_PORT.to_string())
        .env("USER_DB_TOKEN", user_db_token())
        .env("USER_DB_BACKUP_PATH", user_db_backup_path().to_string_lossy().to_string())
        .stdout(Stdio::from(log_file.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| format!("Failed to launch engine: {}", e))
}

/// Where the user database service lives relative to the TUI crate.
pub fn user_db_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cockatiel_user_database-rs")
}

pub const USER_DB_DEFAULT_TOKEN: &str = "userdb-default-token";
pub const USER_DB_DEFAULT_PORT: u16 = 9736;

/// The user-database `.env` (secrets + settings for the service).
pub fn user_db_env_path() -> PathBuf {
    user_db_dir().join(".env")
}

/// The shared user-database auth token, stored in the service's `.env`.
/// Defaults to `USER_DB_DEFAULT_TOKEN` if the file doesn't exist yet (the
/// engine and user_db are launched with the same value, so they always agree).
pub fn user_db_token() -> String {
    read_env_value(&user_db_env_path(), "USER_DB_TOKEN")
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| USER_DB_DEFAULT_TOKEN.to_string())
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
    Command::new(&binary)
        .current_dir(&dir)
        .env("USER_DB_PORT", USER_DB_DEFAULT_PORT.to_string())
        .env("USER_DB_TOKEN", user_db_token())
        .env("USER_DB_PATH", dir.join("user_data.db").to_string_lossy().to_string())
        .env("USER_DB_BACKUP_PATH", user_db_backup_path().to_string_lossy().to_string())
        .stdout(Stdio::from(log_file.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log_file))
        .spawn()
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
        eprintln!(
            "[supervisor] registered binary route {} / {} → {} for {}",
            os_key(),
            arch_key(),
            rel,
            p.manifest.name
        );
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
    command
        .spawn()
        .map_err(|e| format!("Failed to launch '{}': {}", p.manifest.name, e))
}

/// Escape a string for embedding inside a double-quoted shell / AppleScript string.
fn shell_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('`', "\\`")
}

/// Spawn a `terminal: true` module inside a NEW terminal window so it gets a
/// real TTY (stdin/stdout). Cross-platform: macOS (Terminal.app), Linux (first
/// available terminal emulator), Windows (new console window).
///
/// Returns the child, a UNIQUE window marker ("cockatiel:<name>:<uuid>") and a
/// pidfile path so the supervisor can close that exact window and kill the real
/// process later. On macOS the command is dispatched to Terminal.app; the
/// module is `exec`'d over the window's shell, so the pid written to the
/// pidfile IS the module's process id. Any stale window/process for the same
/// module is cleaned up first, so a module never ends up with two instances.
pub fn spawn_terminal_from_parts(
    p: &Plugin,
    cmd: &str,
    args: &[String],
) -> Result<(Child, Option<String>, Option<PathBuf>), String> {
    // The PIN must not appear in the shell command line (visible in `ps`);
    // export it in the wrapper script instead.
    let (pin, clean_args) = strip_pin_from_args(args);
    let pin_export = match pin {
        Some(pin) => format!("export COCKATIEL_PIN={}; ", shell_quote(&pin)),
        None => String::new(),
    };
    let cmd_line = format!("{} {}", shell_quote(cmd), clean_args.join(" "));
    let dir = p.directory.to_string_lossy().to_string();
    // Unique per launch so a close can't target a freshly relaunched window.
    let marker = format!("cockatiel:{}:{}", p.manifest.name, uuid::Uuid::now_v7());
    let dedupe_prefix = format!("cockatiel:{}:", p.manifest.name);
    let pidfile = std::env::temp_dir().join(format!("cockatiel-{}.pid", p.manifest.name));
    // Title the tab with the marker, then run the module in the FOREGROUND so a
    // full-screen TUI module owns the terminal (backgrounding a TUI module
    // breaks it: the shell gives the bg job /dev/null stdin, so ratatui fails
    // with ENXIO and the module exits). A tiny `sh -c` wrapper writes its OWN
    // pid (`$$`) then `exec`s the module — so the pidfile ends up holding the
    // module's real pid, while the window's outer shell stays alive (required
    // for the window to close later: Terminal.app refuses to close a window
    // whose shell has exited).
    let run = format!(
        "{}printf '\\033]0;{}\\007'; cd \"{}\" && sh -c 'echo $$ > \"{}\"; exec {}'",
        pin_export,
        shell_quote(&marker),
        shell_quote(&dir),
        shell_quote(&pidfile.to_string_lossy()),
        cmd_line,
    );

    match std::env::consts::OS {
        "macos" => {
            // Dedupe: kill a stale process from a previous launch of this
            // module (its pidfile survives) and close stale windows, so a
            // relaunch can't stack two running instances or windows.
            kill_terminal_process(&pidfile);
            let script = format!(
                "tell application \"Terminal\"\nactivate\ntry\nclose (every window whose name contains \"{}\") saving no\nend try\ndo script \"{}\"\nend tell",
                shell_quote(&dedupe_prefix),
                shell_quote(&run),
            );
            Command::new("osascript")
                .arg("-e")
                .arg(&script)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
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
                let result = Command::new(emu)
                    .args(*args)
                    .arg(&run)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
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

/// Best-effort close of every Terminal.app window whose title contains
/// `marker` (macOS only). Terminal only reliably closes the FRONT window, so we
/// bring the marker window to the front (`frontmost`), verify it really is
/// window 1, and only then close it — never touching a user's unrelated
/// window. `saving no` force-closes. Terminal is slow/reluctant, so this
/// retries for several seconds. The module process should already be dead.
fn close_terminal_windows(marker: &str) {
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
                    shell_quote(marker)
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
                    shell_quote(marker)
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
        let _ = write_atomic(&path, &pretty);
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
        let _ = write_atomic(&path, &pretty);
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
        let _ = write_atomic(&path, &pretty);
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
            let _ = self.child.kill();
            let _ = self.child.wait();
            return;
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
}