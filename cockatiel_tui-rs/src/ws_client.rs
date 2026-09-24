use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as WsMessage};

use cockatiel_client::proto::*;
use cockatiel_client::proto::container::Payload;

use crate::db;

#[derive(Clone)]
pub enum WsEvent {
    Connected,
    Disconnected,
    Log { source: String, message: String, #[allow(dead_code)] event_type: i32 },
    QueryResult { #[allow(dead_code)] query_id: String, #[allow(dead_code)] result: DatabaseQueryResult },
    StatsUpdate(db::GlobalStats),
    ConnectionInfo { ip: String, port: u16, pin: u32 },
    Prompt(Prompt),
}

#[derive(Debug, Clone)]
pub enum WsCommand {
    SendQuery { query_id: String, sql: String },
    SendPromptResponse { prompt_id: String, accepted: bool, reason: String },
    SendLog { source: String, message: String },
}

pub struct WsClient {
    pub ip: String,
    pub port: u16,
    pub pin: u32,
    pub auth_token: String,
    pub instance_uuid7: String,
    pub event_tx: mpsc::UnboundedSender<WsEvent>,
    pub command_rx: mpsc::UnboundedReceiver<WsCommand>,
    pub stats: db::GlobalStats,
    pub parent_mode: bool,
}

impl WsClient {
    pub fn new(ip: String, port: u16, pin: u32, event_tx: mpsc::UnboundedSender<WsEvent>, command_rx: mpsc::UnboundedReceiver<WsCommand>) -> Self {
        Self {
            ip,
            port,
            pin,
            auth_token: String::new(),
            instance_uuid7: String::new(),
            event_tx,
            command_rx,
            stats: db::GlobalStats::default(),
            parent_mode: false,
        }
    }

    pub fn new_as_child(parent_addr: String, parent_token: String, event_tx: mpsc::UnboundedSender<WsEvent>, command_rx: mpsc::UnboundedReceiver<WsCommand>) -> Self {
        let parts: Vec<&str> = parent_addr.split(':').collect();
        let ip = parts.first().unwrap_or(&"127.0.0.1").to_string();
        let port = parts.get(1).unwrap_or(&"0").parse().unwrap_or(0);

        Self {
            ip,
            port,
            pin: 0,
            auth_token: parent_token.clone(),
            instance_uuid7: parent_token,
            event_tx,
            command_rx,
            stats: db::GlobalStats::default(),
            parent_mode: true,
        }
    }

    pub async fn run(&mut self) {
        loop {
            match self.connect_and_run().await {
                Ok(_) => {
                    let _ = self.event_tx.send(WsEvent::Disconnected);
                }
                Err(_e) => {
                    let _ = self.event_tx.send(WsEvent::Disconnected);
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    async fn connect_and_run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // WSS when the supervisor set COCKATIEL_TLS_CERT (engine only accepts WSS).
        let (scheme, connector): (&str, Option<tokio_tungstenite::Connector>) =
            match std::env::var("COCKATIEL_TLS_CERT") {
                Ok(path) if !path.trim().is_empty() => {
                    let cfg = pinned_tls_config(&path)?;
                    ("wss", Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(cfg))))
                }
                _ => ("ws", None),
            };
        let url = format!("{}://{}:{}", scheme, self.ip, self.port);

        let result = match &connector {
            Some(c) => tokio_tungstenite::connect_async_tls_with_config(&url, None, false, Some(c.clone())).await,
            None => connect_async(&url).await,
        };
        let (ws_stream, _) = result?;
        let (mut write, mut read) = ws_stream.split();

        // Single handshake: send ConnectionRequest with PIN (or auth_token for reconnection)
        let module_name = if self.parent_mode { "cockatiel-tui-child" } else { "cockatiel-tui" };
        let request = Container {
            version: 1,
            auth_token: if self.auth_token.is_empty() { String::new() } else { self.auth_token.clone() },
            module_name: module_name.into(),
            module_instance_uuid7: if self.instance_uuid7.is_empty() { String::new() } else { self.instance_uuid7.clone() },
            payload: Some(Payload::ConnectionRequest(ConnectionRequest {
                pin: self.pin as i32,
                process_position: 4,
                priority: 1,
                module_instance_uuid7: if self.instance_uuid7.is_empty() { String::new() } else { self.instance_uuid7.clone() },
            })),
        };

        let mut buf = Vec::new();
        request.encode(&mut buf)?;
        write.send(WsMessage::Binary(buf.into())).await?;

        let response_msg = read.next().await;
        let response_msg = response_msg.ok_or("No response from engine")??;
        let WsMessage::Binary(data) = response_msg else {
            return Err("Expected binary response".into());
        };

        let response = Container::decode(data.as_ref())?;
        match response.payload {
            Some(Payload::ConnectionRequestReturn(ret)) => {
                if ret.module_instance_uuid7.is_empty() {
                    return Err("Engine rejected connection (empty UUID)".into());
                }
                self.auth_token = response.auth_token;
                self.instance_uuid7 = ret.module_instance_uuid7.clone();
            }
            _ => {
                return Err("Unexpected response from engine".into());
            }
        }

        let _ = self.event_tx.send(WsEvent::Connected);
        let _ = self.event_tx.send(WsEvent::ConnectionInfo {
            ip: self.ip.clone(),
            port: self.port,
            pin: self.pin,
        });

        if self.parent_mode {
            // Parent mode: the child re-authenticates with its assigned uuid
            // (which the parent's WsServer waits for), then both receives
            // forwarded events (logs, query results) and forwards its own
            // one-shot queries back to the parent.
            self.auth_token = self.instance_uuid7.clone();
            let auth_cont = Container {
                version: 1,
                auth_token: self.instance_uuid7.clone(),
                module_name: "cockatiel-tui-child".into(),
                module_instance_uuid7: self.instance_uuid7.clone(),
                payload: Some(Payload::ConnectionRequest(ConnectionRequest {
                    pin: 0,
                    process_position: 4,
                    priority: 1,
                    module_instance_uuid7: self.instance_uuid7.clone(),
                })),
            };
            let mut auth_buf = Vec::new();
            if auth_cont.encode(&mut auth_buf).is_ok() {
                let _ = write.send(WsMessage::Binary(auth_buf)).await;
            }

            let auth_token = self.auth_token.clone();
            let instance_uuid7 = self.instance_uuid7.clone();
            loop {
                tokio::select! {
                    msg = read.next() => {
                        match msg {
                            Some(Ok(WsMessage::Binary(data))) => {
                                let container = match Container::decode(data.as_ref()) {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };
                                if container.auth_token != auth_token {
                                    continue;
                                }
                                match container.payload {
                                    Some(Payload::Log(log)) => {
                                        let _ = self.event_tx.send(WsEvent::Log {
                                            source: container.module_name,
                                            message: log.log,
                                            event_type: 1,
                                        });
                                    }
                                    Some(Payload::DatabaseQueryResult(result)) => {
                                        let query_id = result.query_id.clone();
                                        db::update_stats_from_query(&mut self.stats, &query_id, &result);
                                        let _ = self.event_tx.send(WsEvent::QueryResult {
                                            query_id,
                                            result,
                                        });
                                        let _ = self.event_tx.send(WsEvent::StatsUpdate(self.stats.clone()));
                                    }
                                    _ => {}
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break,
                        }
                    }
                    cmd = self.command_rx.recv() => {
                        let Some(cmd) = cmd else { break };
                        match cmd {
                            WsCommand::SendQuery { query_id, sql } => {
                                let container = Container {
                                    version: 1,
                                    auth_token: auth_token.clone(),
                                    module_name: "cockatiel-tui-child".into(),
                                    module_instance_uuid7: instance_uuid7.clone(),
                                    payload: Some(Payload::DatabaseQuery(DatabaseQuery {
                                        query_id,
                                        sql,
                                        params: Vec::new(),
                                    })),
                                };
                                let mut buf = Vec::new();
                                if container.encode(&mut buf).is_ok() {
                                    let _ = write.send(WsMessage::Binary(buf.into())).await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        } else {
            // Engine mode: send queries + receive results
            let mut query_interval = tokio::time::interval(Duration::from_secs(2));
            let mut initial_tick = true;
            let auth_token = self.auth_token.clone();
            let instance_uuid7 = self.instance_uuid7.clone();

            loop {
                tokio::select! {
                    msg = read.next() => {
                        match msg {
                            Some(Ok(WsMessage::Binary(data))) => {
                                let container = Container::decode(data.as_ref())?;
                                // NOTE: The engine authenticates every message it
                                // processes before routing, so a container reaching
                                // this loop is already trusted. Do NOT filter on
                                // `auth_token` here: the engine broadcasts engine
                                // Log/prompt payloads with an empty token and
                                // forwards module prompts with the *origin* module's
                                // token, neither of which matches the TUI's own
                                // token. Filtering here silently drops every prompt.
                                match container.payload {
                                    Some(Payload::Log(log)) => {
                                        let _ = self.event_tx.send(WsEvent::Log {
                                            source: container.module_name,
                                            message: log.log,
                                            event_type: 1,
                                        });
                                    }
                                    Some(Payload::DatabaseQueryResult(result)) => {
                                        let query_id = result.query_id.clone();
                                        let is_userdb = query_id.starts_with("userdb_");
                                        let is_test = query_id == "test_run";
                                        db::update_stats_from_query(&mut self.stats, &query_id, &result);
                                        if is_userdb {
                                            // Surface user-db responses in the log window.
                                            let blob = String::from_utf8_lossy(&result.result_blob);
                                            let msg = if result.success {
                                                format!("[userdb] {}: {}", query_id, blob)
                                            } else {
                                                format!("[userdb] {} FAILED: {}", query_id, result.error)
                                            };
                                            let _ = self.event_tx.send(WsEvent::Log {
                                                source: "userdb".into(),
                                                message: msg,
                                                event_type: if result.success { 1 } else { 3 },
                                            });
                                        }
                                        if is_test {
                                            // The runner already emits live [test] log lines;
                                            // the final result blob is the JSON summary.
                                            let blob = String::from_utf8_lossy(&result.result_blob);
                                            let msg = if result.success {
                                                format!("[test] suite finished:\n{}", blob)
                                            } else {
                                                format!("[test] suite FAILED: {}", result.error)
                                            };
                                            let _ = self.event_tx.send(WsEvent::Log {
                                                source: "test".into(),
                                                message: msg,
                                                event_type: if result.success { 1 } else { 3 },
                                            });
                                        }
                                        let _ = self.event_tx.send(WsEvent::QueryResult {
                                            query_id,
                                            result,
                                        });
                                        let _ = self.event_tx.send(WsEvent::StatsUpdate(self.stats.clone()));
                                    }
                                    Some(Payload::Err(err)) => {
                                        let _ = self.event_tx.send(WsEvent::Log {
                                            source: container.module_name,
                                            message: err.log,
                                            event_type: 3,
                                        });
                                    }
                                    Some(Payload::ModuleControlResult(result)) => {
                                        let _ = self.event_tx.send(WsEvent::Log {
                                            source: "engine".into(),
                                            message: result.message,
                                            event_type: if result.success { 1 } else { 3 },
                                        });
                                    }
                                    Some(Payload::Prompt(prompt)) => {
                                        let _ = self.event_tx.send(WsEvent::Prompt(prompt));
                                    }
                                    _ => {}
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break,
                        }
                    }
                    cmd = self.command_rx.recv() => {
                        let Some(cmd) = cmd else { break };
                        match cmd {
                            WsCommand::SendQuery { query_id, sql } => {
                                let container = Container {
                                    version: 1,
                                    auth_token: auth_token.clone(),
                                    module_name: "cockatiel-tui".into(),
                                    module_instance_uuid7: instance_uuid7.clone(),
                                    payload: Some(Payload::DatabaseQuery(DatabaseQuery {
                                        query_id,
                                        sql,
                                        params: Vec::new(),
                                    })),
                                };
                                let mut buf = Vec::new();
                                if container.encode(&mut buf).is_ok() {
                                    let _ = write.send(WsMessage::Binary(buf.into())).await;
                                }
                            }
                            WsCommand::SendPromptResponse { prompt_id, accepted, reason } => {
                                let container = Container {
                                    version: 1,
                                    auth_token: auth_token.clone(),
                                    module_name: "cockatiel-tui".into(),
                                    module_instance_uuid7: instance_uuid7.clone(),
                                    payload: Some(Payload::PromptResponse(PromptResponse {
                                        prompt_id_uuid7: prompt_id,
                                        accepted,
                                        reason,
                                    })),
                                };
                                let mut buf = Vec::new();
                                if container.encode(&mut buf).is_ok() {
                                    let _ = write.send(WsMessage::Binary(buf.into())).await;
                                }
                            }
                            WsCommand::SendLog { source, message } => {
                                let container = Container {
                                    version: 1,
                                    auth_token: auth_token.clone(),
                                    module_name: "cockatiel-tui".into(),
                                    module_instance_uuid7: instance_uuid7.clone(),
                                    payload: Some(Payload::Log(Log {
                                        log: format!("[{}] {}", source, message),
                                        blob: Vec::new(),
                                    })),
                                };
                                let mut buf = Vec::new();
                                if container.encode(&mut buf).is_ok() {
                                    let _ = write.send(WsMessage::Binary(buf.into())).await;
                                }
                            }
                        }
                    }
                    _ = query_interval.tick() => {
                        if initial_tick {
                            initial_tick = false;
                            continue;
                        }
                        for (query_id, sql) in db::get_pending_queries() {
                            let container = Container {
                                version: 1,
                                auth_token: auth_token.clone(),
                                module_name: "cockatiel-tui".into(),
                                module_instance_uuid7: instance_uuid7.clone(),
                                payload: Some(Payload::DatabaseQuery(DatabaseQuery {
                                    query_id: query_id.to_string(),
                                    sql,
                                    params: Vec::new(),
                                })),
                            };
                            let mut buf = Vec::new();
                            if container.encode(&mut buf).is_ok() {
                                let _ = write.send(WsMessage::Binary(buf.into())).await;
                            }
                        }
                    }
                }
            }
        }

        let _ = self.event_tx.send(WsEvent::Disconnected);
        Ok(())
    }
}

/// Build a rustls client config that trusts exactly the engine's self-signed
/// certificate (cert pinning), so the TUI can connect to the WSS-only engine.
fn pinned_tls_config(cert_pem_path: &str) -> Result<rustls::ClientConfig, String> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cert_bytes =
        std::fs::read(cert_pem_path).map_err(|e| format!("read TLS cert {}: {}", cert_pem_path, e))?;
    let mut reader = std::io::BufReader::new(cert_bytes.as_slice());
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("parse TLS cert: {}", e))?;
    if certs.is_empty() {
        return Err(format!("no certificate found in {}", cert_pem_path));
    }
    let mut roots = rustls::RootCertStore::empty();
    for c in certs {
        roots.add(c).map_err(|e| format!("pinning TLS cert failed: {}", e))?;
    }
    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}
