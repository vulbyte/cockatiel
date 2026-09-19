use futures_util::{SinkExt, StreamExt};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tracing::{info, warn};
use tracing_subscriber::FmtSubscriber;

use cockatiel_client::CockatielClient;
use cockatiel_client::proto::{container::Payload, *};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    #[serde(default = "default_language")]
    language: String,
    #[serde(default)]
    custom_ranges: Vec<[u32; 2]>,
    #[serde(default)]
    allow_emoji: bool,
    #[serde(default)]
    allow_expressive: bool,
    #[serde(default = "default_reason")]
    flag_reason: String,
}

fn default_language() -> String {
    "latin".to_string()
}

fn default_reason() -> String {
    "out-of-language".to_string()
}

fn load_config() -> Config {
    std::fs::read_to_string("config.json")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| {
            let default = Config {
                language: default_language(),
                custom_ranges: Vec::new(),
                allow_emoji: false,
                allow_expressive: false,
                flag_reason: default_reason(),
            };
            if let Ok(pretty) = serde_json::to_string_pretty(&default) {
                let _ = std::fs::write("config.json", pretty);
            }
            default
        })
}

#[derive(Debug, Clone, Deserialize)]
struct Languages {
    #[serde(default)]
    presets: HashMap<String, Vec<[u32; 2]>>,
}

fn load_languages() -> HashMap<String, Vec<[u32; 2]>> {
    std::fs::read_to_string("languages.json")
        .ok()
        .and_then(|s| serde_json::from_str::<Languages>(&s).ok())
        .map(|l| l.presets)
        .unwrap_or_default()
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

/// Characters commonly used for expressive/emote messages (e.g. ඞ, (๑ > ᴗ < ๑)).
fn is_expressive(c: char) -> bool {
    matches!(
        c,
        'ඞ' | '๑'
            | 'ᴗ'
            | 'ಠ'
            | '益'
            | 'ʕ'
            | 'ʔ'
            | '⊙'
            | '☉'
            | '♥'
            | '♡'
            | '✧'
            | '❀'
            | '★'
            | '☆'
    )
}

fn in_ranges(c: char, ranges: &[[u32; 2]]) -> bool {
    let cp = c as u32;
    ranges.iter().any(|r| cp >= r[0] && cp <= r[1])
}

fn violates_language(
    message: &str,
    ranges: &[[u32; 2]],
    allow_emoji: bool,
    allow_expressive: bool,
) -> bool {
    for c in message.chars() {
        if c.is_whitespace() || in_ranges(c, ranges) {
            continue;
        }
        if allow_emoji && is_emoji(c) {
            continue;
        }
        if allow_expressive && is_expressive(c) {
            continue;
        }
        return true;
    }
    false
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let config = load_config();
    let all_langs = load_languages();
    let mut ranges = all_langs.get(&config.language).cloned().unwrap_or_default();
    ranges.extend(config.custom_ranges.clone());
    info!(
        "Language-constrainer active: language='{}' ({} ranges, emoji={}, expressive={})",
        config.language,
        ranges.len(),
        config.allow_emoji,
        config.allow_expressive
    );

    let client = CockatielClient::connect("language_constrainer.json").await?;
    let (mut write, mut read) = client.stream.split();
    let auth_token = client.auth_token.clone();
    let instance_uuid = client.instance_uuid7.clone();
    let module_name = client.config.module_name.clone();

    while let Some(msg) = read.next().await {
        let Ok(WsMessage::Binary(data)) = msg else {
            continue;
        };
        let Ok(container) = Container::decode(data.as_ref()) else {
            continue;
        };

        match container.payload {
            Some(Payload::MessagePreProcess(pre)) => {
                let Some(chat) = &pre.raw_message else {
                    continue;
                };
                let uuid = pre.message_uuid7.clone();
                let original = chat.raw_message.clone();

                // Out-of-language messages are held for audit: the engine
                // broadcasts an audit prompt (Approve/Reject) to connected UIs.
                if violates_language(
                    &original,
                    &ranges,
                    config.allow_emoji,
                    config.allow_expressive,
                ) {
                    warn!(
                        "Message [{}] violates language '{}' — flagging for audit",
                        uuid, config.language
                    );
                    let flag = Container {
                        version: 1,
                        auth_token: auth_token.clone(),
                        module_name: module_name.clone(),
                        module_instance_uuid7: instance_uuid.clone(),
                        payload: Some(Payload::AuditFlag(AuditFlag {
                            message_uuid7: uuid.clone(),
                            reason: config.flag_reason.clone(),
                            origin: module_name.clone(),
                        })),
                    };
                    let mut buf = Vec::new();
                    if flag.encode(&mut buf).is_ok() {
                        let _ = write.send(WsMessage::Binary(buf.into())).await;
                    }
                }

                // Ack pre_process (content preserved — the audit hold prevents
                // it from being shown until a moderator releases it).
                let reply = Container {
                    version: 1,
                    auth_token: auth_token.clone(),
                    module_name: module_name.clone(),
                    module_instance_uuid7: instance_uuid.clone(),
                    payload: Some(Payload::MessagePreProcess(MessagePreProcess {
                        message_uuid7: uuid,
                        raw_message: Some(ChatMessage {
                            platform: chat.platform.clone(),
                            raw_data: chat.raw_data.clone(),
                            raw_message: original,
                            user_uuid7: chat.user_uuid7.clone(),
                            command: chat.command.clone(),
                            user_data: chat.user_data.clone(),
                        }),
                    })),
                };
                let mut buf = Vec::new();
                if reply.encode(&mut buf).is_ok() {
                    let _ = write.send(WsMessage::Binary(buf.into())).await;
                }
            }
            _ => {}
        }
    }

    Ok(())
}