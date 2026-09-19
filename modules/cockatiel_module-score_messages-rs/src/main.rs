use futures_util::{SinkExt, StreamExt};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
struct Rule {
    toggle: bool,
    score: i64,
}

impl Default for Rule {
    fn default() -> Self {
        Self { toggle: true, score: 1 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrequencyRule {
    toggle: bool,
    score: i64,
    #[serde(default = "default_interval")]
    interval_secs: u64,
}

impl Default for FrequencyRule {
    fn default() -> Self {
        Self {
            toggle: true,
            score: -2,
            interval_secs: default_interval(),
        }
    }
}

fn default_interval() -> u64 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Config {
    #[serde(default)]
    punctuation: Rule,
    #[serde(default)]
    question: Rule,
    #[serde(default)]
    length: Rule,
    #[serde(default)]
    spam: Rule,
    #[serde(default)]
    no_spacing: Rule,
    #[serde(default)]
    wordless: Rule,
    #[serde(default)]
    emoji: Rule,
    #[serde(default)]
    frequency: FrequencyRule,
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
        punctuation: Rule { toggle: true, score: 1 },
        question: Rule { toggle: true, score: 2 },
        length: Rule { toggle: true, score: 1 },
        spam: Rule { toggle: true, score: -5 },
        no_spacing: Rule { toggle: true, score: -2 },
        wordless: Rule { toggle: true, score: -3 },
        emoji: Rule { toggle: true, score: 1 },
        frequency: FrequencyRule::default(),
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
    let emoji_answer = prompt_for_input(
        write_ws,
        prompt_rx,
        auth_token,
        module_name,
        instance_uuid,
        "Configure score-messages module",
        "No saved config was found. Enable the emoji rule (reward messages containing emoji)?",
        "Enable emoji rule? (y/n)",
        PromptKind::Boolean,
        60,
    )
    .await;

    let freq_answer = prompt_for_input(
        write_ws,
        prompt_rx,
        auth_token,
        module_name,
        instance_uuid,
        "Configure score-messages module",
        "Enable the frequency rule (punish users who post again too soon)?",
        "Enable frequency rule? (y/n)",
        PromptKind::Boolean,
        60,
    )
    .await;

    let mut default = default_config();
    default.emoji.toggle = emoji_answer
        .as_deref()
        .map(|c| c.trim().eq_ignore_ascii_case("y") || c.trim().eq_ignore_ascii_case("yes"))
        .unwrap_or(default.emoji.toggle);
    default.frequency.toggle = freq_answer
        .as_deref()
        .map(|c| c.trim().eq_ignore_ascii_case("y") || c.trim().eq_ignore_ascii_case("yes"))
        .unwrap_or(default.frequency.toggle);

    if let Ok(pretty) = serde_json::to_string_pretty(&default) {
        let _ = std::fs::write("config.json", pretty);
    }
    default
}

/// Is this token made of repeated characters (spam: bbbbb, asdlfkjsqaldkfja)?
fn looks_like_spam_token(token: &str) -> bool {
    let t = token.to_lowercase();
    if t.len() < 4 {
        return false;
    }
    let chars: Vec<char> = t.chars().collect();
    // Repeated single char, or no vowels (keyboard mashing).
    let all_same = chars.iter().all(|c| *c == chars[0]);
    if all_same {
        return true;
    }
    let has_vowel = chars.iter().any(|c| "aeiou".contains(*c));
    !has_vowel && t.len() >= 5
}

fn is_wordless(message: &str) -> bool {
    // Uses a crude trigram check: if most tokens contain no vowels, treat as wordless.
    let tokens: Vec<&str> = message.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    let wordlike = tokens
        .iter()
        .filter(|t| t.chars().any(|c| "aeiou".contains(c.to_ascii_lowercase())))
        .count();
    (wordlike as f64 / tokens.len() as f64) < 0.5
}

/// Common emoji Unicode ranges.
fn is_emoji(c: char) -> bool {
    let cp = c as u32;
    (0x1F300..=0x1F5FF).contains(&cp)
        || (0x1F600..=0x1F64F).contains(&cp)
        || (0x1F680..=0x1F6FF).contains(&cp)
        || (0x1F900..=0x1F9FF).contains(&cp)
        || (0x1FA70..=0x1FAFF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0xFE00..=0xFE0F).contains(&cp)
        || (0x1F000..=0x1F0FF).contains(&cp)
}

/// Score a message and return the total delta. Positive = reward, negative = punishment.
fn score_message(message: &str, cfg: &Config) -> (i64, Vec<String>) {
    let mut delta = 0i64;
    let mut notes = Vec::new();

    let trimmed = message.trim();

    // Punctuation: reward for ending with ., !, ?.
    if cfg.punctuation.toggle {
        if let Some(last) = trimmed.chars().last() {
            if ".!?".contains(last) {
                delta += cfg.punctuation.score;
                notes.push("punctuation".into());
            }
        }
    }

    // Questions: reward for ending with ?.
    if cfg.question.toggle && trimmed.ends_with('?') {
        delta += cfg.question.score;
        notes.push("question".into());
    }

    // Length: reward for substantial messages (>= 40 chars).
    if cfg.length.toggle && trimmed.chars().count() >= 40 {
        delta += cfg.length.score;
        notes.push("length".into());
    }

    // Spam: repeated-char tokens / keyboard mash.
    if cfg.spam.toggle {
        let spam = trimmed.split_whitespace().any(looks_like_spam_token);
        if spam {
            delta += cfg.spam.score;
            notes.push("spam".into());
        }
    }

    // No spacing: long message with very few spaces.
    if cfg.no_spacing.toggle {
        let len = trimmed.chars().count();
        let spaces = trimmed.chars().filter(|c| *c == ' ').count();
        if len >= 20 && spaces == 0 {
            delta += cfg.no_spacing.score;
            notes.push("no_spacing".into());
        }
    }

    // Wordless: mostly vowel-less tokens.
    if cfg.wordless.toggle && is_wordless(trimmed) {
        delta += cfg.wordless.score;
        notes.push("wordless".into());
    }

    // Emoji: reward messages that include an emoji (engagement).
    if cfg.emoji.toggle && trimmed.chars().any(is_emoji) {
        delta += cfg.emoji.score;
        notes.push("emoji".into());
    }

    (delta, notes)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let client = CockatielClient::connect("score_messages.json").await?;
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
            let mut last_msg: HashMap<String, Instant> = HashMap::new();
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
                        let text = chat.raw_message.clone();
                        let uuid = pre.message_uuid7.clone();
                        let user_id = chat.user_uuid7.clone();

                        let (mut delta, mut notes) = score_message(&text, &config);

                        // Frequency: punish posting again too soon after the last message.
                        if config.frequency.toggle && !user_id.is_empty() {
                            let now = Instant::now();
                            if let Some(prev) = last_msg.get(&user_id) {
                                if now.duration_since(*prev) < Duration::from_secs(config.frequency.interval_secs) {
                                    delta += config.frequency.score;
                                    notes.push("frequency".into());
                                }
                            }
                            last_msg.insert(user_id.clone(), now);
                        }

                        if delta == 0 || uuid.is_empty() {
                            continue;
                        }

                        // Apply the score to the user via the engine's mod query.
                        let query_id = if delta > 0 { "mod_commend" } else { "mod_reprimand" };
                        let payload = if user_id.chars().filter(|c| *c == '-').count() == 4 && user_id.len() == 36 {
                            serde_json::json!({
                                "uuid7": user_id,
                                "reason": format!("score-messages: {}", notes.join(",")),
                            })
                        } else {
                            serde_json::json!({
                                "platform": chat.platform,
                                "handle": user_id,
                                "reason": format!("score-messages: {}", notes.join(",")),
                            })
                        };

                        let query = Container {
                            version: 1,
                            auth_token: auth_token.clone(),
                            module_name: module_name.clone(),
                            module_instance_uuid7: instance_uuid.clone(),
                            payload: Some(Payload::DatabaseQuery(DatabaseQuery {
                                query_id: query_id.to_string(),
                                sql: payload.to_string(),
                                params: vec![],
                            })),
                        };
                        let mut buf = Vec::new();
                        if query.encode(&mut buf).is_ok() {
                            let mut w = write_shared.lock().await;
                            let _ = w.send(WsMessage::Binary(buf.into())).await;
                        }

                        warn!(
                            "[score] uuid={} delta={} notes={:?} ({} user={})",
                            uuid, delta, notes, query_id, user_id
                        );
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
    info!("Score-messages module active");

    // Keep the process alive; the read task does all the work.
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}