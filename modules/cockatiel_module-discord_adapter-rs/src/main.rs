use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use cockatiel_client::{proto::container::Payload, proto::*, CockatielClient, PromptKind};

// GUILD_MESSAGES (1<<9) + MESSAGE_CONTENT (1<<15). MESSAGE_CONTENT is a
// privileged intent — enable it in the Discord Developer Portal for the bot.
const DISCORD_INTENTS: u64 = (1 << 9) | (1 << 15);
const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const REST_API: &str = "https://discord.com/api/v10";

type WsWriteHalf = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    WsMessage,
>;

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct DiscordAdapterConfig {
    bot_token: Option<String>,
    guild_id: Option<String>,
    #[serde(default)]
    channels: Option<Vec<String>>,
}

fn load_adapter_config() -> Option<DiscordAdapterConfig> {
    let path = PathBuf::from("config.json");
    if !path.exists() {
        return None;
    }
    let data = std::fs::read_to_string(path).ok()?;
    let json_val: serde_json::Value = serde_json::from_str(&data).ok()?;
    if let Some(mod_spec) = json_val.get("module_specific") {
        serde_json::from_value(mod_spec.clone()).ok()
    } else {
        None
    }
}

fn save_adapter_config(bot_token: &str, guild_id: &str, channels: &[String]) {
    let path = PathBuf::from("config.json");
    let mut json_val = if let Ok(data) = std::fs::read_to_string(&path) {
        serde_json::from_str::<serde_json::Value>(&data).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    let mut spec = json!({
        "bot_token": bot_token,
        "guild_id": guild_id,
    });
    if !channels.is_empty() {
        spec["channels"] = json!(channels);
    }
    json_val["module_specific"] = spec;
    if let Ok(pretty) = serde_json::to_string_pretty(&json_val) {
        let _ = std::fs::write(&path, pretty);
    }
}

/// Setup guide text — reused both as the printed guide (TUI log window, since
/// module stdout is captured) and as the `details` for engine prompts.
fn setup_guide_text() -> String {
    "\
==================================================
  Discord Adapter — Setup Required
==================================================
  1. Create a bot at https://discord.com/developers/applications
     - New Application -> Bot -> Reset Token, copy it.
  2. Enable the \"Message Content\" intent (Privileged Gateway Intents).
  3. Invite the bot to your server:
     OAuth2 -> URL Generator -> scopes: bot (+ applications.commands)
     Permissions: Read Messages, Send Messages, Moderate Members.
  4. Enable Developer Mode: Discord Settings -> Advanced -> Developer Mode.
     - Right-click your server -> Copy Server ID.
     - (Optional) Right-click a channel -> Copy Channel ID.
  5. In the TUI: select discord-adapter -> press c, then enter:
     - Bot Token (discord.com/developers)
     - Server (Guild) ID
     - Channel IDs (one per line; empty = monitor all channels)
=================================================="
        .to_string()
}

/// Clean a command target: `<@!123>` / `<@123>` mentions -> id, `@name` -> name.
fn clean_target(raw: &str) -> String {
    let mut t = raw.trim().to_string();
    if t.starts_with("<@") {
        t = t.trim_start_matches("<@").trim_start_matches('!').to_string();
        t = t.trim_end_matches('>').to_string();
    } else {
        t = t.trim_start_matches('@').to_string();
    }
    t
}

/// Parse a moderator command (!ban / !timeout) from a chat message.
/// Returns (query_id, payload_json). The actor (message author) is included.
fn parse_mod_command(message: &str, author: &str) -> Option<(String, serde_json::Value)> {
    let trimmed = message.trim();
    let lower = trimmed.to_lowercase();

    if lower.starts_with("!ban") {
        let args = trimmed[5..].trim();
        let (target, rest) = match args.split_once(char::is_whitespace) {
            Some((t, r)) => (t, r),
            None => (args, ""),
        };
        let target = clean_target(target);
        if target.is_empty() {
            return None;
        }
        return Some((
            "mod_ban".to_string(),
            serde_json::json!({
                "platform": "discord",
                "handle": target,
                "reason": rest.trim().to_string(),
                "actor": { "platform": "discord", "handle": author },
            }),
        ));
    }

    if lower.starts_with("!timeout") {
        let args = trimmed[9..].trim();
        let mut parts = args.split_whitespace();
        let target = parts.next().unwrap_or("").to_string();
        let target = clean_target(&target);
        if target.is_empty() {
            return None;
        }
        let mut duration_secs = 300i64;
        let mut reason = String::new();
        if let Some(d) = parts.next() {
            if let Ok(secs) = d.parse::<i64>() {
                duration_secs = secs;
            } else {
                reason = d.to_string();
            }
        }
        let rest: Vec<&str> = parts.collect();
        if !rest.is_empty() {
            if !reason.is_empty() {
                reason = format!("{} {}", reason, rest.join(" "));
            } else {
                reason = rest.join(" ");
            }
        }
        return Some((
            "mod_timeout".to_string(),
            serde_json::json!({
                "platform": "discord",
                "handle": target,
                "duration_secs": duration_secs,
                "reason": reason,
                "actor": { "platform": "discord", "handle": author },
            }),
        ));
    }

    None
}

// ── Discord REST helpers ──────────────────────────────────────────────

async fn send_discord_message(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    msg: &str,
) -> Result<(), String> {
    let body = json!({ "content": msg });
    let resp = client
        .post(format!("{}/channels/{}/messages", REST_API, channel_id))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("send request failed: {}", e))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let text = resp.text().await.unwrap_or_default();
        Err(format!("Discord send {}: {}", status, text))
    }
}

// ── Discord gateway ───────────────────────────────────────────────────

/// Connect to the Discord gateway, identify, heartbeat, and forward
/// MESSAGE_CREATE events to the engine as preprocessed messages. When the bot
/// token is rejected (op 9 / close 4004), it asks the operator for a fresh
/// token via the prompt subwindow instead of retrying the bad token forever.
async fn run_discord_gateway(
    token: String,
    channels: Vec<String>,
    guild_id: String,
    member_cache: Arc<Mutex<HashMap<String, String>>>,
    engine_write: Arc<Mutex<WsWriteHalf>>,
    send_token: Arc<Mutex<String>>,
    send_channels: Arc<Mutex<Vec<String>>>,
    prompt_rx: &mut mpsc::UnboundedReceiver<PromptResponse>,
    auth_token: &str,
    module_name: &str,
    instance_uuid: &str,
) {
    let mut channel_set: std::collections::HashSet<String> = channels.iter().cloned().collect();
    let mut channels = channels;
    let mut token = token;
    let mut guild_id = guild_id;
    let mut auth_failures = 0u32;
    let mut guild_mismatches = 0u32;
    let mut first_ready = true;
    loop {
        let (mut ws, _) = match tokio_tungstenite::connect_async(GATEWAY_URL).await {
            Ok(c) => c,
            Err(e) => {
                error!(
                    "Failed to connect to Discord gateway ({}). Likely causes: invalid bot token, the Message Content intent is not enabled in the Developer Portal, or the bot is not in the guild. Retrying in 5s...",
                    e
                );
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        // Wait for OP 10 HELLO for the heartbeat interval, then IDENTIFY.
        let mut heartbeat_interval_ms = 41250u64;
        let mut seq: Option<u64> = None;

        // First read the HELLO.
        match ws.next().await {
            Some(Ok(WsMessage::Text(txt))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                    if v.get("op").and_then(|o| o.as_u64()) == Some(10) {
                        heartbeat_interval_ms = v["d"]["heartbeat_interval"].as_u64().unwrap_or(41250);
                    }
                }
            }
            Some(Ok(WsMessage::Binary(_))) => {}
            _ => {}
        }

        let identify = json!({
            "op": 2,
            "d": {
                "token": &token,
                "intents": DISCORD_INTENTS,
                "properties": { "os": "linux", "browser": "cockatiel", "device": "cockatiel" },
            }
        });
        if ws.send(WsMessage::Text(identify.to_string())).await.is_err() {
            continue;
        }
        info!("Discord gateway identified. Heartbeat every {} ms.", heartbeat_interval_ms);

        let mut heartbeat = tokio::time::interval(std::time::Duration::from_millis(heartbeat_interval_ms));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        'conn: loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    let _ = ws.send(WsMessage::Text(json!({ "op": 1, "d": seq }).to_string())).await;
                }
                msg = ws.next() => {
                    let Some(msg) = msg else { break 'conn; };
                    let Ok(msg) = msg else { break 'conn; };
                    let text = match msg {
                        WsMessage::Text(t) => t,
                        WsMessage::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                        WsMessage::Close(_) => break 'conn,
                        _ => continue,
                    };
                    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&text) else {
                        continue;
                    };
                    let op = payload.get("op").and_then(|o| o.as_u64()).unwrap_or(0);
                    if let Some(s) = payload.get("s").and_then(|s| s.as_u64()) {
                        seq = Some(s);
                    }
                    // op 9 = Invalid Session (bad token / session expired).
                    if op == 9 {
                        warn!("Discord gateway reported an invalid session (op 9) — the bot token may be wrong.");
                        auth_failures += 1;
                        break 'conn;
                    }
                    if op == 0 {
                        let t = payload.get("t").and_then(|v| v.as_str()).unwrap_or("");
                        let d = payload.get("d");
                        match t {
                            "READY" => {
                                info!("Discord gateway ready (bot: {}).", d.and_then(|x| x.get("user")).and_then(|u| u.get("username")).and_then(|u| u.as_str()).unwrap_or("?"));

                                // READY carries `d.guilds` — the servers the bot
                                // belongs to. On the FIRST ready of each launch we
                                // ask the operator which server + channels to
                                // monitor (the bot now knows what it can access);
                                // on reconnects we only re-ask if the configured
                                // guild turned out to be inaccessible.
                                let bot_guilds: Vec<String> = d
                                    .and_then(|x| x.get("guilds"))
                                    .and_then(|g| g.as_array())
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|g| g.get("id").and_then(|i| i.as_str()))
                                            .map(|s| s.to_string())
                                            .collect()
                                    })
                                    .unwrap_or_default();

                                let guild_valid = !guild_id.is_empty() && bot_guilds.contains(&guild_id);

                                if first_ready || !guild_valid {
                                    let was_first = first_ready;
                                    first_ready = false;

                                    if !guild_valid {
                                        error!(
                                            "Discord bot is NOT in configured server {} — it can only access: [{}]. No messages will arrive from this server.",
                                            guild_id,
                                            bot_guilds.join(", ")
                                        );
                                        guild_mismatches += 1;
                                    }

                                    // Numbered list of servers the bot CAN access.
                                    let mut accessible_list = String::new();
                                    for (i, g) in bot_guilds.iter().enumerate() {
                                        accessible_list.push_str(&format!("  {}: {}\n", i + 1, g));
                                    }
                                    let accessible = if bot_guilds.is_empty() {
                                        "  (none — the bot has no server access at all)".to_string()
                                    } else {
                                        accessible_list.trim_end().to_string()
                                    };

                                    let title = if was_first {
                                        "Choose the Discord Server"
                                    } else {
                                        "Discord Server Not Accessible"
                                    };
                                    let why = if was_first {
                                        "Choose which server you want to monitor."
                                    } else {
                                        "The bot is not a member of the server you configured."
                                    };

                                    if let Some(choice) = prompt_for_input(
                                        &engine_write,
                                        prompt_rx,
                                        auth_token,
                                        module_name,
                                        instance_uuid,
                                        title,
                                        &format!(
                                            "{}\n\n\
                                             Your bot can currently only access these servers:\n{}\n\n\
                                             Pick how to proceed:\n\
                                             1. Enter the NUMBER of the server above you want to monitor (e.g. 1), or\n\
                                             2. Paste the correct Server (Guild) ID directly.\n\n\
                                             To find a Server ID: Settings > Advanced > Developer Mode >\n\
                                             right-click the server name > Copy Server ID. If the server isn't\n\
                                             listed, invite the bot to it first.\n\n\
                                             Leave empty (or press Cancel) to keep retrying.",
                                            why,
                                            accessible
                                        ),
                                        "Server number or Guild ID",
                                        PromptKind::String,
                                        300,
                                    )
                                    .await
                                    {
                                        let trimmed = choice.trim().to_string();
                                        if trimmed.is_empty() || trimmed == "0" {
                                            warn!("No server selected — keeping current setting and retrying.");
                                        } else if let Ok(idx) = trimmed.parse::<usize>() {
                                            if idx >= 1 && idx <= bot_guilds.len() {
                                                guild_id = bot_guilds[idx - 1].clone();
                                                info!("Discord Server ID set to {} (selection {}).", guild_id, idx);
                                            } else {
                                                warn!("Server number {} is out of range (1..={}). Keeping current setting.", idx, bot_guilds.len());
                                            }
                                        } else {
                                            // Not a number — treat as a direct Guild ID.
                                            guild_id = trimmed.clone();
                                            info!("Discord Server ID set to {} (pasted).", guild_id);
                                        }
                                    }

                                    // Only ask about channels on the first ready of
                                    // this launch (smoother reconnects).
                                    if was_first {
                                        if let Some(ch_choice) = prompt_for_input(
                                            &engine_write,
                                            prompt_rx,
                                            auth_token,
                                            module_name,
                                            instance_uuid,
                                            "Discord Channels",
                                            "Which channels should the bot monitor?\n\n\
                                             Enter channel IDs separated by commas (e.g. 123,456,789).\n\
                                             Leave empty to monitor ALL channels in the selected server.\n\n\
                                             To find channel IDs: Settings > Advanced > Developer Mode >\n\
                                             right-click a channel > Copy Channel ID.",
                                            "Channel IDs (comma-separated, empty = all)",
                                            PromptKind::String,
                                            300,
                                        )
                                        .await
                                        {
                                            let trimmed = ch_choice.trim();
                                            if trimmed.is_empty() {
                                                channels.clear();
                                            } else {
                                                channels = trimmed
                                                    .split(',')
                                                    .map(|c| c.trim().to_string())
                                                    .filter(|c| !c.is_empty())
                                                    .collect();
                                            }
                                            channel_set = channels.iter().cloned().collect();
                                            *send_channels.lock().await = channels.clone();
                                        }
                                    }

                                    save_adapter_config(&token, &guild_id, &channels);
                                    info!("Discord configuration: server={} channels={} (reconnecting...)", guild_id, channels.len());
                                    // Reconnect to apply the selection (or to keep
                                    // retrying on cancel — the mismatch backoff
                                    // below stops prompt spam).
                                    break 'conn;
                                } else {
                                    // All good — clear any mismatch counter.
                                    guild_mismatches = 0;
                                    info!("Discord bot confirmed in configured server {}.", guild_id);
                                }
                            }
                            "MESSAGE_CREATE" => {
                                if let Some(d) = d {
                                    handle_message_create(
                                        d,
                                        &channel_set,
                                        &member_cache,
                                        &engine_write,
                                        auth_token,
                                        module_name,
                                        instance_uuid,
                                        &guild_id,
                                    )
                                    .await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        warn!("Discord gateway disconnected. Reconnecting in 5s...");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        // If the operator didn't fix the server ID (or cancelled), back off
        // longer so we don't re-prompt every few seconds in a tight loop.
        if guild_mismatches > 0 {
            guild_mismatches = 0;
            warn!("Discord server (guild) ID still not accessible — will retry in 60s (no more prompting until then).");
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }

        // Repeated auth failures mean the saved token is bad — ask the operator
        // for a fresh one via the prompt subwindow instead of looping forever.
        if auth_failures >= 2 {
            auth_failures = 0;
            info!("Prompting for a new Discord bot token (previous token was rejected)...");
            if let Some(new_token) = prompt_for_input(
                &engine_write,
                prompt_rx,
                auth_token,
                module_name,
                instance_uuid,
                "Discord Bot Token Required",
                "Discord rejected the current bot token (invalid session / authentication failed).\n\n\
                 Paste a valid bot token from the Discord Developer Portal\n\
                 (discord.com/developers > your app > Bot > Reset Token).\n\n\
                 The token is masked so it never shows on screen.",
                "Bot Token",
                PromptKind::Credential,
                300,
            )
            .await
            {
                let trimmed = new_token.trim().to_string();
                if !trimmed.is_empty() {
                    token = trimmed.clone();
                    *send_token.lock().await = token.clone();
                    save_adapter_config(&token, &guild_id, &channels);
                    info!("Discord bot token updated.");
                }
            }
        }
    }
}

async fn handle_message_create(
    d: &serde_json::Value,
    channel_set: &std::collections::HashSet<String>,
    member_cache: &Arc<Mutex<HashMap<String, String>>>,
    engine_write: &Arc<Mutex<WsWriteHalf>>,
    auth_token: &str,
    module_name: &str,
    instance_uuid: &str,
    guild_id: &str,
) {
    let is_bot = d.get("author").and_then(|a| a.get("bot")).and_then(|b| b.as_bool()).unwrap_or(false);
    if is_bot {
        return;
    }
    // Only forward messages from the configured server. "All channels" means
    // all channels *within* this server, not every server the bot happens to
    // be in — this also stops cross-server leakage when the ID is wrong.
    let msg_guild = d.get("guild_id").and_then(|g| g.as_str()).unwrap_or("");
    if !guild_id.is_empty() && msg_guild != guild_id {
        return;
    }
    let author = d.get("author").and_then(|a| a.get("username")).and_then(|u| u.as_str()).unwrap_or("Unknown");
    let author_id = d.get("author").and_then(|a| a.get("id")).and_then(|u| u.as_str()).unwrap_or("");
    let channel_id = d.get("channel_id").and_then(|c| c.as_str()).unwrap_or("");
    let content = d.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
    if content.is_empty() {
        return;
    }
    if !channel_set.is_empty() && !channel_set.contains(channel_id) {
        return;
    }

    // Cache the author id -> username so `<@id>` mention targets can resolve.
    if !author_id.is_empty() {
        member_cache.lock().await.insert(author_id.to_string(), author.to_string());
    }

    let pre = MessagePreProcess {
        message_uuid7: String::new(),
        raw_message: Some(ChatMessage {
            platform: "discord".into(),
            raw_data: serde_json::to_string(d).unwrap_or_default().into_bytes(),
            raw_message: content.clone(),
            user_uuid7: author.to_string(),
            command: None,
            user_data: None,
        }),
    };
    let container = Container {
        version: 1,
        auth_token: auth_token.to_string(),
        module_name: module_name.to_string(),
        module_instance_uuid7: instance_uuid.to_string(),
        payload: Some(Payload::MessagePreProcess(pre)),
    };
    let mut buf = Vec::new();
    if container.encode(&mut buf).is_ok() {
        let mut write = engine_write.lock().await;
        let _ = write.send(WsMessage::Binary(buf.into())).await;
    }

    // Handle moderator commands (!ban / !timeout).
    if let Some((qid, payload)) = parse_mod_command(&content, author) {
        info!("Mod command detected: {} target={}", qid, payload);
        // Resolve a mention target (id) to a cached username if possible.
        let query = Container {
            version: 1,
            auth_token: auth_token.to_string(),
            module_name: module_name.to_string(),
            module_instance_uuid7: instance_uuid.to_string(),
            payload: Some(Payload::DatabaseQuery(DatabaseQuery {
                query_id: qid,
                sql: payload.to_string(),
                params: vec![],
            })),
        };
        let mut qbuf = Vec::new();
        if query.encode(&mut qbuf).is_ok() {
            let mut write = engine_write.lock().await;
            let _ = write.send(WsMessage::Binary(qbuf.into())).await;
        }
    }
}

// ── main ──────────────────────────────────────────────────────────────

/// Send a Prompt to the engine (forwarded to connected UIs) and wait for the
/// operator's response (`PromptResponse.reason`). Returns None on cancel/timeout.
async fn prompt_for_input(
    engine_write: &Arc<Mutex<WsWriteHalf>>,
    prompt_rx: &mut mpsc::UnboundedReceiver<PromptResponse>,
    auth_token: &str,
    module_name: &str,
    instance_uuid: &str,
    title: &str,
    details: &str,
    input_label: &str,
    kind: PromptKind,
    timeout: u32,
) -> Option<String> {
    let prompt_id = uuid::Uuid::now_v7().to_string();
    let prompt_type = match kind {
        PromptKind::Boolean => PromptType::Boolean,
        PromptKind::String => PromptType::String,
        PromptKind::Credential => PromptType::Credential,
    };
    let prompt = Prompt {
        prompt_id_uuid7: prompt_id.clone(),
        prompt: title.to_string(),
        details: details.to_string(),
        yes_dialog: "Submit".to_string(),
        no_dialog: "Cancel".to_string(),
        timeout,
        origin: module_name.to_string(),
        origin_uuid7: String::new(),
        instructions: String::new(),
        link: String::new(),
        input_label: input_label.to_string(),
        prompt_type: prompt_type as i32,
    };
    let container = Container {
        version: 1,
        auth_token: auth_token.to_string(),
        module_name: module_name.to_string(),
        module_instance_uuid7: instance_uuid.to_string(),
        payload: Some(Payload::Prompt(prompt)),
    };
    let mut buf = Vec::new();
    if container.encode(&mut buf).is_err() {
        return None;
    }
    {
        let mut write = engine_write.lock().await;
        if write.send(WsMessage::Binary(buf.into())).await.is_err() {
            return None;
        }
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout as u64 + 10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(10), prompt_rx.recv()).await {
            Ok(Some(resp)) if resp.prompt_id_uuid7 == prompt_id => {
                return if resp.accepted {
                    Some(resp.reason)
                } else {
                    None
                };
            }
            Ok(Some(_)) => continue, // a different prompt's response
            Ok(None) => return None,
            // The 10s poll interval elapsed with no response yet: keep waiting
            // until the real deadline rather than auto-cancelling.
            Err(_) => continue,
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder().with_max_level(Level::INFO).finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    info!("Starting Discord Adapter Module...");

    // Load config (non-interactive fast path — the TUI supplies credentials).
    let mut bot_token = std::env::var("DISCORD_BOT_TOKEN").unwrap_or_default();
    let mut guild_id = std::env::var("DISCORD_GUILD_ID").unwrap_or_default();
    let mut channels: Vec<String> = Vec::new();

    // Connect to the engine first so prompts can be surfaced to connected UIs
    // before the adapter is configured.
    let cockatiel = CockatielClient::connect("discord_adapter.json").await?;
    let auth_token = cockatiel.auth_token.clone();
    let instance_uuid = cockatiel.instance_uuid7.clone();
    let module_name = cockatiel.config.module_name.clone();
    let (write, read) = cockatiel.stream.split();

    let engine_write: Arc<Mutex<WsWriteHalf>> = Arc::new(Mutex::new(write));
    let member_cache: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let http = reqwest::Client::new();

    // Send state for the read task, populated once config resolves.
    let send_token: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let send_channels: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // Channel carrying PromptResponses from the engine to the config loop, so
    // `prompt_for_input` can await the operator's typed answer.
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<PromptResponse>();

    // Read task: engine -> adapter (SendToPlatforms / PromptResponse).
    {
        let token_state = send_token.clone();
        let channels_state = send_channels.clone();
        let http = http.clone();
        tokio::spawn(async move {
            let mut read = read;
            while let Some(msg) = read.next().await {
                let Ok(WsMessage::Binary(data)) = msg else { continue };
                let Ok(container) = Container::decode(data.as_ref()) else { continue };
                let Some(payload) = container.payload else { continue };
                match payload {
                    Payload::SendToPlatforms(send) => {
                        let token = token_state.lock().await.clone();
                        let channels = channels_state.lock().await.clone();
                        // Send to every monitored channel.
                        if channels.is_empty() {
                            warn!("SendToPlatforms received but no channels configured to send to.");
                            continue;
                        }
                        for ch in &channels {
                            match send_discord_message(&http, &token, ch, &send.msg).await {
                                Ok(()) => info!("Sent to Discord channel {}: {}", ch, send.msg),
                                Err(e) => error!("SendToPlatforms failed on {}: {}", ch, e),
                            }
                        }
                    }
                    Payload::PromptResponse(resp) => {
                        // Forward operator answers to the awaiting prompt.
                        let _ = prompt_tx.send(resp);
                    }
                    _ => {}
                }
            }
        });
    }

    // ── Config phase ───────────────────────────────────────────────────
    // Only the bot token is required here. The server (guild) and channels
    // are chosen at READY time, once the bot can list the servers it actually
    // has access to.
    if bot_token.is_empty() {
        if let Some(saved) = load_adapter_config() {
            if let Some(st) = saved.bot_token {
                if !st.is_empty() {
                    bot_token = st;
                }
            }
            if let Some(sg) = saved.guild_id {
                if !sg.is_empty() {
                    guild_id = sg;
                }
            }
            if let Some(sc) = saved.channels {
                if !sc.is_empty() {
                    channels = sc;
                }
            }
        }
    }

    if bot_token.is_empty() {
        // Prompt for the bot token (masked credential).
        if let Some(val) = prompt_for_input(
            &engine_write,
            &mut prompt_rx,
            &auth_token,
            &module_name,
            &instance_uuid,
            "Discord Bot Token Required",
            &setup_guide_text(),
            "Bot Token",
            PromptKind::Credential,
            120,
        )
        .await
        {
            bot_token = val.trim().to_string();
        }

        // Fallback: if the operator cancelled, keep polling config.json so the
        // TUI can still supply a token via file.
        while bot_token.is_empty() {
            if let Some(saved) = load_adapter_config() {
                if let Some(st) = saved.bot_token {
                    if !st.is_empty() {
                        bot_token = st;
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    // Publish the token to the read task (channels/guild are finalized at
    // READY and synced by the gateway).
    *send_token.lock().await = bot_token.clone();
    *send_channels.lock().await = channels.clone();

    // Discord gateway task.
    info!("Connecting to Discord gateway...");
    run_discord_gateway(
        bot_token,
        channels,
        guild_id,
        member_cache,
        engine_write,
        send_token,
        send_channels,
        &mut prompt_rx,
        &auth_token,
        &module_name,
        &instance_uuid,
    )
    .await;

    Ok(())
}