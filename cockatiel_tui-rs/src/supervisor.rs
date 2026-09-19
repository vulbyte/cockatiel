use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::plugins::Plugin;

/// Where the engine lives relative to the TUI crate.
pub fn engine_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cockatiel_engine-rs")
}

pub fn engine_config_path() -> PathBuf {
    engine_dir().join("config.json")
}

pub fn modules_registry_path() -> PathBuf {
    engine_dir().join("modules.json")
}

/// Read engine address info from the engine's config.json.
pub fn read_engine_addr() -> Option<(u16, u32)> {
    let content = std::fs::read_to_string(engine_config_path()).ok()?;
    let config: serde_json::Value = serde_json::from_str(&content).ok()?;
    let port = config.get("port").and_then(|v| v.as_u64()).unwrap_or(1111) as u16;
    let pin = config.get("paring_pin").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    Some((port, pin))
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
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| e.to_string())?;
    Command::new(&binary)
        .current_dir(&dir)
        .env("USER_DB_HOST", "127.0.0.1")
        .env("USER_DB_PORT", USER_DB_DEFAULT_PORT.to_string())
        .env("USER_DB_TOKEN", USER_DB_DEFAULT_TOKEN)
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
        .env("USER_DB_TOKEN", USER_DB_DEFAULT_TOKEN)
        .env("USER_DB_PATH", dir.join("user_data.db").to_string_lossy().to_string())
        .stdout(Stdio::from(log_file.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| format!("Failed to launch user database: {}", e))
}

/// Build the launch command for a plugin: <launch_command> <command_flags> -- --ip --port --pin
fn build_module_command(p: &Plugin, port: u16, pin: u32) -> Vec<String> {
    let m = &p.manifest;
    let mut parts: Vec<String> = m.launch_command.split_whitespace().map(String::from).collect();
    for flag in &m.command_flags {
        parts.push(flag.clone());
    }
    if parts.iter().any(|s| s == "run" || s == "start" || s == "exec") {
        parts.push("--".into());
    }
    parts.push("--ip".into());
    parts.push("127.0.0.1".into());
    parts.push("--port".into());
    parts.push(port.to_string());
    parts.push("--pin".into());
    parts.push(pin.to_string());
    parts
}

/// Launch a plugin as a TUI-owned child process. Module stdout/stderr are piped
/// so the TUI can surface them in the log window (and forward them to the
/// engine for the timeline database). Terminal modules open their own window.
pub fn launch_plugin(p: &Plugin, port: u16, pin: u32) -> Result<Child, String> {
    if p.manifest.terminal {
        return launch_terminal_module(p, port, pin);
    }
    let parts = build_module_command(p, port, pin);
    Command::new(&parts[0])
        .args(&parts[1..])
        .current_dir(&p.directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to launch '{}': {}", p.manifest.name, e))
}

/// Escape a string for embedding inside a double-quoted shell / AppleScript string.
fn shell_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('`', "\\`")
}

/// Launch a `terminal: true` module inside a NEW terminal window so it gets a
/// real TTY (stdin/stdout). Cross-platform: macOS (Terminal.app), Linux (first
/// available terminal emulator), Windows (new console window).
///
/// Note: on macOS the command is dispatched to Terminal.app, so teardown of the
/// window is best-effort (the terminal emulator owns the process).
fn launch_terminal_module(p: &Plugin, port: u16, pin: u32) -> Result<Child, String> {
    let parts = build_module_command(p, port, pin);
    let cmd_line = parts.join(" ");
    let dir = p.directory.to_string_lossy().to_string();
    let run = format!("cd \"{}\" && {}", shell_quote(&dir), cmd_line);

    match std::env::consts::OS {
        "macos" => {
            let script = format!(
                "tell application \"Terminal\" to do script \"{}\"",
                shell_quote(&run)
            );
            Command::new("osascript")
                .arg("-e")
                .arg(&script)
                .spawn()
                .map_err(|e| format!("Failed to launch '{}' in Terminal.app: {}", p.manifest.name, e))
        }
        "windows" => {
            Command::new("cmd")
                .args(["/C", "start", "", "cmd", "/K"])
                .arg(&run)
                .spawn()
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
                    .spawn();
                match result {
                    Ok(child) => return Ok(child),
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
        let _ = std::fs::write(&path, pretty);
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
        let _ = std::fs::write(&path, pretty);
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
        let _ = std::fs::write(&path, pretty);
    }
}

/// A managed running process (module or engine).
pub struct ManagedProcess {
    pub child: Child,
}

impl ManagedProcess {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub type ProcessTable = HashMap<String, Arc<Mutex<ManagedProcess>>>;