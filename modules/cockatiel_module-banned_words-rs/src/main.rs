
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tracing::{info, warn};
use tracing_subscriber::FmtSubscriber;

use cockatiel_client::{proto::container::Payload, proto::*, CockatielClient, PromptKind};

type WsWriteHalf = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    WsMessage,
>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    banned_words: Vec<String>,
    #[serde(default = "default_mode")]
    censor_mode: String,
    #[serde(default)]
    replace_word: String,
    #[serde(default)]
    flag_for_review: bool,
    /// "none" | "censor" | "replace" — apply to the WHOLE message/sentence
    /// instead of just the offending token.
    #[serde(default = "default_sentence_mode")]
    sentence_mode: String,
    #[serde(default)]
    replace_sentence: String,
}

fn default_mode() -> String {
    "soft".to_string()
}

fn default_sentence_mode() -> String {
    "none".to_string()
}

/// Leet-speak translation table (common substitutions).
fn leet_translate(s: &str) -> String {
    s.chars()
        .map(|c| match c.to_ascii_lowercase() {
            '3' => 'e',
            '4' | '@' => 'a',
            '0' => 'o',
            '1' | '!' => 'i',
            '5' | '$' => 's',
            '7' => 't',
            'z' => 's',
            other => other,
        })
        .collect()
}

/// Collapse whitespace and remove all spaces (for "no-spaces" / "extra-spaces" detection).
fn strip_all_spaces(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace() && *c != '_').collect()
}

/// Check if the message contains any banned word, across variants.
/// Returns the first banned word matched and which variant it was.
fn detect_banned(message: &str, banned: &[String]) -> Option<(String, &'static str)> {
    let lowered = message.to_lowercase();
    for word in banned {
        let w = word.to_lowercase();
        if w.is_empty() {
            continue;
        }
        // Token-level detection: for each whitespace-delimited token, compare
        // the token (original, leet-translated, and space-stripped) to the word.
        for token in lowered.split_whitespace() {
            if token == w {
                return Some((w.clone(), "exact"));
            }
            let leet = leet_translate(token);
            if leet == w {
                return Some((w.clone(), "leet"));
            }
            let nospace = strip_all_spaces(token);
            if nospace == w || nospace.contains(&w) {
                return Some((w.clone(), "no-spaces"));
            }
        }
        // Whole-message fallback: a space-stripped message containing the word.
        let stripped = strip_all_spaces(&lowered);
        if stripped.contains(&w) {
            return Some((w.clone(), "no-spaces"));
        }
    }
    None
}

/// Censor a single token (already known to contain a banned word).
fn censor_token(token: &str, mode: &str, replace_word: &str) -> String {
    match mode {
        "hard" => "*".repeat(token.chars().count()),
        "mid" => {
            let mut s = String::new();
            for (i, ch) in token.chars().enumerate() {
                s.push(if i == 0 { ch } else { '*' });
            }
            s
        }
        "replace" => {
            if replace_word.is_empty() {
                "*".repeat(token.chars().count())
            } else {
                replace_word.to_string()
            }
        }
        // "soft" (default): keep first + last letter, censor the middle.
        _ => {
            let chars: Vec<char> = token.chars().collect();
            if chars.len() <= 2 {
                "*".repeat(chars.len())
            } else {
                let mut s = String::new();
                s.push(chars[0]);
                for _ in 1..chars.len() - 1 {
                    s.push('*');
                }
                s.push(chars[chars.len() - 1]);
                s
            }
        }
    }
}

/// Censor any token in the message that matches a banned word (any variant).
/// Non-matching tokens are preserved exactly (including whitespace).
fn censor_message(message: &str, banned: &[String], mode: &str, replace_word: &str) -> String {
    let mut result = String::new();
    let mut rest = message;
    while !rest.is_empty() {
        // Find the next whitespace-delimited token.
        let mut end = rest.len();
        for (i, ch) in rest.char_indices() {
            if ch.is_whitespace() {
                end = i;
                break;
            }
        }
        let (token, remainder) = rest.split_at(end);
        let matched = banned.iter().any(|word| {
            let w = word.to_lowercase();
            if w.is_empty() {
                return false;
            }
            let t = token.to_lowercase();
            t == w
                || leet_translate(&t) == w
                || strip_all_spaces(&t) == w
                || strip_all_spaces(&t).contains(&w)
        });
        if matched {
            result.push_str(&censor_token(token, mode, replace_word));
        } else {
            result.push_str(token);
        }
        result.push_str(&remainder[..0]); // empty
        rest = &remainder;
        // Preserve the delimiter (the split_at consumed it as part of remainder).
        if !rest.is_empty() && end < message.len() {
            // remainder starts with the whitespace that ended the token.
            result.push_str(&rest[..1]);
            rest = &rest[1..];
        }
    }
    result
}

/// Send a Prompt to the engine (forwarded to connected UIs) and wait for the
/// operator's response (`PromptResponse.reason`). Returns None on cancel/timeout.
async fn prompt_for_input(
    write_ws: &Arc<AsyncMutex<WsWriteHalf>>,
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
    let mut guard = write_ws.lock().await;
    if guard.send(WsMessage::Binary(buf.into())).await.is_err() {
        return None;
    }
    drop(guard);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout as u64 + 10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(10), prompt_rx.recv()).await {
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
            // until the real deadline (the `timeout` seconds above), rather than
            // bailing out 10 seconds in and auto-cancelling every prompt.
            Err(_) => continue,
        }
    }
    None
}

fn default_config() -> Config {
    Config {
        banned_words: vec!["cheese".to_string(), "badword".to_string()],
        censor_mode: default_mode(),
        replace_word: String::new(),
        flag_for_review: false,
        sentence_mode: default_sentence_mode(),
        replace_sentence: String::new(),
    }
}

async fn load_config(
    write_ws: &Arc<AsyncMutex<WsWriteHalf>>,
    prompt_rx: &mut mpsc::UnboundedReceiver<PromptResponse>,
    auth_token: &str,
    module_name: &str,
    instance_uuid: &str,
) -> Config {
    if let Ok(s) = std::fs::read_to_string("config.json") {
        if let Ok(cfg) = serde_json::from_str(&s) {
            return cfg;
        }
    }

    // No saved config: ask the operator via the engine prompt subwindow
    // instead of silently writing a default.
    let banned_answer = prompt_for_input(
        write_ws,
        prompt_rx,
        auth_token,
        module_name,
        instance_uuid,
        "Configure banned-words module",
        "No saved config was found. Enter the list of words to filter, separated by commas.",
        "Banned words (comma-separated)",
        PromptKind::String,
        60,
    )
    .await;

    let mode_answer = prompt_for_input(
        write_ws,
        prompt_rx,
        auth_token,
        module_name,
        instance_uuid,
        "Configure banned-words module",
        "Enter the censor mode used when a banned word is detected.",
        "censor mode: soft|mid|hard|replace",
        PromptKind::String,
        60,
    )
    .await;

    let default = default_config();
    let config = Config {
        banned_words: banned_answer
            .as_deref()
            .map(|s| {
                s.split(',')
                    .map(|w| w.trim().to_string())
                    .filter(|w| !w.is_empty())
                    .collect::<Vec<String>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or(default.banned_words),
        censor_mode: mode_answer
            .as_deref()
            .filter(|m| matches!(*m, "soft" | "mid" | "hard" | "replace"))
            .map(|m| m.to_string())
            .unwrap_or(default.censor_mode),
        replace_word: String::new(),
        flag_for_review: false,
        sentence_mode: default_sentence_mode(),
        replace_sentence: String::new(),
    };

    if let Ok(pretty) = serde_json::to_string_pretty(&config) {
        let _ = std::fs::write("config.json", pretty);
    }
    config
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    // Connect to the engine as a preprocess module (CLI overrides: --ip/--port/--pin).
    let client = CockatielClient::connect("banned_words.json").await?;
    let (write, mut read) = client.stream.split();
    let write_shared: Arc<AsyncMutex<WsWriteHalf>> = Arc::new(AsyncMutex::new(write));
    let auth_token = client.auth_token.clone();
    let instance_uuid = client.instance_uuid7.clone();
    let module_name = client.config.module_name.clone();

    // Channel carrying PromptResponses from the engine to the config prompt,
    // so `prompt_for_input` can await the operator's typed answer.
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<PromptResponse>();

    // Shared config: the read task must run while config is being loaded (so
    // the config prompt can receive its response), so it reads config from an
    // Arc<Mutex> that is populated right after load_config returns.
    let config_shared: Arc<Mutex<Config>> = Arc::new(Mutex::new(default_config()));

    // Read task: forward PromptResponses to the awaiting prompt AND handle
    // message pre-processing. Spawned BEFORE load_config so prompts work.
    {
        let prompt_tx_task = prompt_tx.clone();
        let config_shared = Arc::clone(&config_shared);
        let write_shared = Arc::clone(&write_shared);
        let auth_token = auth_token.clone();
        let module_name = module_name.clone();
        let instance_uuid = instance_uuid.clone();
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                let Ok(WsMessage::Binary(data)) = msg else { continue };
                let Ok(container) = Container::decode(data.as_ref()) else { continue };

                match container.payload {
                    Some(Payload::PromptResponse(resp)) => {
                        // Forward operator answers to the awaiting prompt.
                        let _ = prompt_tx_task.send(resp);
                    }
                    Some(Payload::MessagePreProcess(pre)) => {
                        let Some(chat) = &pre.raw_message else { continue };
                        let config = config_shared.lock().unwrap().clone();
                        let original = chat.raw_message.clone();
                        let uuid = pre.message_uuid7.clone();

                        let mut flagged = false;
                        let censored = match detect_banned(&original, &config.banned_words) {
                            Some((word, variant)) => {
                                warn!("Banned word '{}' detected via {} variant", word, variant);
                                flagged = true;
                                match config.sentence_mode.as_str() {
                                    // Censor the whole message/sentence.
                                    "censor" => "*".repeat(original.chars().count()),
                                    // Replace the whole message/sentence.
                                    "replace" => {
                                        if config.replace_sentence.is_empty() {
                                            censor_message(&original, &config.banned_words, &config.censor_mode, &config.replace_word)
                                        } else {
                                            config.replace_sentence.clone()
                                        }
                                    }
                                    // Default: censor only the offending token(s).
                                    _ => censor_message(&original, &config.banned_words, &config.censor_mode, &config.replace_word),
                                }
                            }
                            None => original.clone(),
                        };

                        // If flag_for_review, send an Err/Log so the engine surfaces it.
                        if flagged && config.flag_for_review {
                            let log = Container {
                                version: 1,
                                auth_token: auth_token.clone(),
                                module_name: module_name.clone(),
                                module_instance_uuid7: instance_uuid.clone(),
                                payload: Some(Payload::Err(cockatiel_client::proto::Err {
                                    log: format!("[banned-words] flagged message for review: {}", original),
                                    blob: vec![],
                                    trace: String::new(),
                                })),
                            };
                            let mut buf = Vec::new();
                            if log.encode(&mut buf).is_ok() {
                                let mut w = write_shared.lock().await;
                                let _ = w.send(WsMessage::Binary(buf.into())).await;
                            }
                        }

                        // Reply with the (possibly censored) message, same uuid → engine acks pre_process.
                        let reply = Container {
                            version: 1,
                            auth_token: auth_token.clone(),
                            module_name: module_name.clone(),
                            module_instance_uuid7: instance_uuid.clone(),
                            payload: Some(Payload::MessagePreProcess(MessagePreProcess {
                                message_uuid7: uuid.clone(),
                                raw_message: Some(ChatMessage {
                                    platform: chat.platform.clone(),
                                    raw_data: chat.raw_data.clone(),
                                    raw_message: censored.clone(),
                                    user_uuid7: chat.user_uuid7.clone(),
                                    command: chat.command.clone(),
                                    user_data: chat.user_data.clone(),
                                }),
                            })),
                        };
                        let mut buf = Vec::new();
                        if reply.encode(&mut buf).is_ok() {
                            let mut w = write_shared.lock().await;
                            let _ = w.send(WsMessage::Binary(buf.into())).await;
                            if flagged {
                                info!("Censored message ({}): {:?} -> {:?}", uuid, original, censored);
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    let config = load_config(
        &write_shared,
        &mut prompt_rx,
        &auth_token,
        &module_name,
        &instance_uuid,
    )
    .await;
    *config_shared.lock().unwrap() = config.clone();
    info!(
        "Banned-words module active: mode={}, {} word(s)",
        config.censor_mode,
        config.banned_words.len()
    );

    // Keep the process alive; the read task does all the work.
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}