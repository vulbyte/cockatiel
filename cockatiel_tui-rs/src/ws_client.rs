use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    Log { source: String, message: String, event_type: i32 },
    QueryResult { query_id: String, result: DatabaseQueryResult },
    StatsUpdate(db::GlobalStats),
    ConnectionInfo { ip: String, port: u16, pin: u32 },
    Prompt(Prompt),
}

#[derive(Debug, Clone)]
pub enum WsCommand {
    SendQuery { query_id: String, sql: String },
    SendPromptResponse { prompt_id: String, accepted: bool, reason: String },
    SendLog { source: String, message: String },
    /// Close the engine socket NOW and never reconnect. Sent when the TUI
    /// removes the engine (`Action::RemoveEngine`): the engine is not coming
    /// back on its own, so a client that kept retrying would be an orphan — the
    /// same failure the detached-window give-up below exists to prevent, one
    /// level up.
    Disconnect,
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
    /// Set by the TUI when the engine has been deliberately removed. Read
    /// between connection attempts, so a detached client stops for good instead
    /// of retrying an engine that is not coming back.
    ///
    /// An `Arc<AtomicBool>` because the client is moved into its own task while
    /// the switch is raised from the main loop — the same shape as
    /// `AppState::engine_detached`, which holds the other end.
    pub stopped: Arc<AtomicBool>,
}

/// How many consecutive failures a DETACHED (pop-out) window tolerates before
/// concluding its parent TUI is gone for good and exiting. With the backoff
/// below that is roughly 75 seconds of trying.
pub const DETACHED_GIVE_UP_AFTER: u32 = 5;

/// Delay before reconnecting after `attempt` consecutive failures: 3s, 6s, 12s,
/// 24s, then capped at 30s. Pure so the curve is testable.
pub fn reconnect_backoff_secs(attempt: u32) -> u64 {
    if attempt == 0 {
        return 3;
    }
    // Shift is bounded so the doubling cannot overflow, and the result is capped
    // at 30s: past that, waiting longer buys nothing.
    (3u64 << attempt.min(4)).min(30)
}

impl WsClient {
    pub fn new(ip: String, port: u16, pin: u32, event_tx: mpsc::UnboundedSender<WsEvent>, command_rx: mpsc::UnboundedReceiver<WsCommand>) -> Self {
        // Engine mode: replay the identity this TUI registered in a previous
        // session (pinned in the engine's modules.json). A warm engine's
        // auto-approve for control-surface names requires the registered
        // instance uuid, so a fresh process must present it or it hangs.
        let (instance_uuid7, auth_token) = crate::supervisor::registered_engine_identity("cockatiel-tui")
            .unwrap_or_else(|| (String::new(), String::new()));
        Self {
            ip,
            port,
            pin,
            auth_token,
            instance_uuid7,
            event_tx,
            command_rx,
            stats: db::GlobalStats::default(),
            parent_mode: false,
            stopped: Arc::new(AtomicBool::new(false)),
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
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// True once the TUI has told this client to stop for good.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub async fn run(&mut self) {
        let mut consecutive_failures: u32 = 0;
        loop {
            // Checked BEFORE the first attempt, not just between them: a client
            // that was stopped while disconnected (the engine was removed) must
            // never dial it again. This is the difference between "forgot the
            // engine" and "the engine is down", and only the former is final.
            if self.is_stopped() {
                return;
            }
            // The `Result` is consumed inside the match rather than bound
            // across the sleep below: `Box<dyn Error>` is not `Send`, so
            // holding it over an await point would make this future unspawnable.
            match self.connect_and_run().await {
                Ok(_) => {
                    let _ = self.event_tx.send(WsEvent::Disconnected);
                    consecutive_failures = 0;
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    // A detached (pop-out) window talks to its PARENT TUI, not to
                    // the engine. If the parent is gone its port is closed for
                    // good, so retrying forever just leaves an orphan spamming
                    // the log (and, before the stdio fix, the tty) every few
                    // seconds. Give up and exit instead.
                    if self.parent_mode && consecutive_failures >= DETACHED_GIVE_UP_AFTER {
                        crate::app::supervisor_log_global(format!(
                            "[ws_client] detached window: parent TUI is unreachable ({}), giving up",
                            e
                        ));
                        std::process::exit(0);
                    }
                    if consecutive_failures == 1 {
                        crate::app::supervisor_log_global(format!(
                            "[ws_client] connect_and_run ended with error: {}",
                            e
                        ));
                    }
                    let _ = self.event_tx.send(WsEvent::Disconnected);
                }
            }
            // Stopped mid-attempt (the removal raised the switch while the
            // socket was up): return without reporting anything — a deliberate
            // detach is not a failure — and without sitting out a backoff for a
            // reconnect that must not happen.
            if self.is_stopped() {
                return;
            }
            // Back off instead of hammering: a real outage should not turn into
            // a log line every 3 seconds for as long as it lasts.
            tokio::time::sleep(Duration::from_secs(reconnect_backoff_secs(
                consecutive_failures,
            )))
            .await;
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

        // Drain frames until the ConnectionRequestReturn arrives. The engine
        // floods connected UIs with Log broadcasts / prompts the moment a
        // module connects, so under load the FIRST frame is often NOT the
        // handshake response — treating it as such dropped the TUI into a
        // reconnect loop. Handle those frames and keep reading.
        let mut auth_token = String::new();
        let mut assigned_uuid = String::new();
        let mut got_return = false;
        for _ in 0..64 {
            let response_msg = match tokio::time::timeout(
                Duration::from_secs(15),
                read.next(),
            )
            .await
            {
                Ok(Some(msg)) => msg.map_err(|e| format!("engine read error: {}", e))?,
                Ok(None) => return Err("engine closed during handshake".into()),
                Err(_) => return Err("engine handshake timed out".into()),
            };
            let WsMessage::Binary(data) = response_msg else {
                continue;
            };
            let response = Container::decode(data.as_ref())?;
            match response.payload {
                Some(Payload::ConnectionRequestReturn(ret)) => {
                    if ret.module_instance_uuid7.is_empty() {
                        // Engine rejected the connection. Downgrade the identity so
                        // the run() retry loop tries the next fallback instead of
                        // retrying the same doomed identity forever:
                        //   token + registered uuid → PIN + registered uuid → PIN + fresh (bootstrap).
                        if !self.auth_token.is_empty() {
                            self.auth_token.clear();
                        } else if !self.instance_uuid7.is_empty() {
                            self.instance_uuid7.clear();
                        }
                        return Err("Engine rejected connection (empty UUID)".into());
                    }
                    auth_token = response.auth_token;
                    assigned_uuid = ret.module_instance_uuid7.clone();
                    got_return = true;
                    break;
                }
                Some(Payload::Log(log)) => {
                    let _ = self.event_tx.send(WsEvent::Log {
                        source: response.module_name,
                        message: log.log,
                        event_type: 1,
                    });
                }
                Some(Payload::Prompt(prompt)) => {
                    let _ = self.event_tx.send(WsEvent::Prompt(prompt));
                }
                Some(Payload::AuthVerify(_)) => {
                    // Answer the liveness probe like any module.
                    let reply = Container {
                        version: 1,
                        auth_token: if self.auth_token.is_empty() { String::new() } else { self.auth_token.clone() },
                        module_name: if self.parent_mode { "cockatiel-tui-child" } else { "cockatiel-tui" }.into(),
                        module_instance_uuid7: if self.instance_uuid7.is_empty() { String::new() } else { self.instance_uuid7.clone() },
                        payload: Some(Payload::AuthVerify(AuthVerify {
                            cur_auth: self.auth_token.clone(),
                        })),
                    };
                    let mut rb = Vec::new();
                    if reply.encode(&mut rb).is_ok() {
                        let _ = write.send(WsMessage::Binary(rb.into())).await;
                    }
                }
                _ => {}
            }
        }
        if !got_return {
            return Err("Engine never returned a ConnectionRequestReturn".into());
        }
        self.auth_token = auth_token;
        self.instance_uuid7 = assigned_uuid;

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
                            // Only the TUI removes the engine, and the TUI is
                            // never in parent mode, so a child never receives
                            // this — handled here so the variant cannot fall
                            // through as a silent no-op if that ever changes.
                            WsCommand::Disconnect => break,
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
                                    Some(Payload::AuthVerify(_)) => {
                                        // Answer the liveness probe like any
                                        // module, so a quiet TUI is never
                                        // flagged unresponsive and killed.
                                        let reply = Container {
                                            version: 1,
                                            auth_token: auth_token.clone(),
                                            module_name: "cockatiel-tui".into(),
                                            module_instance_uuid7: instance_uuid7.clone(),
                                            payload: Some(Payload::AuthVerify(AuthVerify {
                                                cur_auth: auth_token.clone(),
                                            })),
                                        };
                                        let mut rb = Vec::new();
                                        if reply.encode(&mut rb).is_ok() {
                                            let _ = write.send(WsMessage::Binary(rb.into())).await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                crate::app::supervisor_log_global(format!(
                                    "[ws_client] engine read error: {:?}",
                                    e
                                ));
                                break;
                            }
                            None => {
                                crate::app::supervisor_log_global(
                                    "[ws_client] engine closed the connection".to_string(),
                                );
                                break;
                            }
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
                            // The engine was removed from the TUI: close the
                            // socket now rather than after the reconnect
                            // backoff. `run()` reads the stop switch next and
                            // returns without dialing again.
                            WsCommand::Disconnect => break,
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

#[cfg(test)]
mod reconnect_tests {
    use super::{reconnect_backoff_secs, DETACHED_GIVE_UP_AFTER};

    /// A fixed 3s retry turned a real outage into a log line every 3 seconds
    /// indefinitely. The curve has to grow and then cap.
    #[test]
    fn the_reconnect_backoff_grows_then_caps() {
        assert_eq!(reconnect_backoff_secs(0), 3);
        assert_eq!(reconnect_backoff_secs(1), 6);
        assert_eq!(reconnect_backoff_secs(2), 12);
        assert_eq!(reconnect_backoff_secs(3), 24);
        assert_eq!(reconnect_backoff_secs(4), 30, "capped");
        assert_eq!(reconnect_backoff_secs(50), 30);
        assert_eq!(reconnect_backoff_secs(u32::MAX), 30);
    }

    #[test]
    fn the_backoff_is_monotonic_and_never_zero() {
        let mut last = 0;
        for a in 0..40u32 {
            let d = reconnect_backoff_secs(a);
            assert!(d >= last, "backoff went backwards at attempt {a}");
            assert!(d > 0);
            last = d;
        }
    }

    /// The orphan that motivated this: a detached window whose parent TUI had
    /// died retried every 3 seconds for over an hour. It has to give up.
    #[test]
    fn a_detached_window_gives_up_eventually() {
        // One failure is not enough evidence the parent is gone (a restart race
        // is normal), so the budget has to allow several attempts — but the total
        // time spent must stay short enough that a real orphan does not linger.
        let total: u64 = (1..=DETACHED_GIVE_UP_AFTER)
            .map(reconnect_backoff_secs)
            .sum();
        assert!(total <= 120, "gave up after {total}s, too slow");
        assert!(total >= 10, "gave up after only {total}s, too eager");
    }

    /// Only a DETACHED window may give up. The main TUI talks to the ENGINE,
    /// which the TUI itself supervises and restarts, so it must keep retrying
    /// forever no matter how long the engine is down.
    #[test]
    fn only_a_detached_window_is_allowed_to_give_up() {
        let src = include_str!("ws_client.rs");
        let start = src.find("pub async fn run(&mut self)").expect("run() not found");
        let body = &src[start..start + 2000];
        assert!(
            body.contains("self.parent_mode && consecutive_failures >= DETACHED_GIVE_UP_AFTER"),
            "the give-up must be gated on parent_mode"
        );
        assert!(
            body.contains("std::process::exit(0)"),
            "the detached window must actually exit"
        );
    }

    #[test]
    fn a_repeated_failure_is_logged_once_not_every_retry() {
        // The flood the operator saw was the same line every 3 seconds.
        let src = include_str!("ws_client.rs");
        let start = src.find("pub async fn run(&mut self)").expect("run() not found");
        let body = &src[start..start + 2000];
        assert!(
            body.contains("if consecutive_failures == 1"),
            "only the first failure in a streak should be logged"
        );
    }

    /// Removing the engine from the TUI means the client must never speak to it
    /// again. Checked BEHAVIOURALLY, not by reading the source: the stop switch
    /// is set and `run()` is pointed at a port nothing is listening on, so if
    /// the switch were not honoured the client would still be retrying (or
    /// blocked in a connect) when the timeout fires. A stopped client is a
    /// client that has left the building.
    #[tokio::test]
    async fn a_stopped_client_dials_nothing_and_returns() {
        use super::{WsClient, WsCommand, WsEvent};
        use tokio::sync::mpsc;
        let (tx, mut rx) = mpsc::unbounded_channel::<WsEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<WsCommand>();
        // Port 1: nothing listens there, so a client that ignored the switch
        // would either be mid-reconnect or already backing off for 3s+.
        let mut client = WsClient::new("127.0.0.1".to_string(), 1, 0, tx, cmd_rx);
        client.stopped.store(true, std::sync::atomic::Ordering::SeqCst);

        let ran = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            client.run(),
        )
        .await;
        assert!(ran.is_ok(), "a stopped client must return, not keep retrying");
        // A deliberate detach is not a disconnection to report: the TUI has
        // already forgotten the engine and would drop the event anyway.
        assert!(
            rx.try_recv().is_err(),
            "stopping must not report anything to the TUI"
        );
    }

    /// The stop switch is only half the mechanism: the socket has to CLOSE now,
    /// not after the reconnect backoff, and `run()` has to re-check the switch
    /// on the way out. Both are asserted on the source because neither is
    /// reachable without a live engine socket: dropping the `Disconnect` arm
    /// would leave the removal working in tests and hanging in the app.
    #[test]
    fn the_disconnect_command_and_the_stop_switch_both_end_the_client() {
        let src = include_str!("ws_client.rs");
        // Sliced to the CLIENT (everything before the test module) because this
        // file is its own source here: searching the whole file would count the
        // literal in the assertion below as a third implementation.
        let start = src.find("pub async fn run(&mut self)").expect("run() not found");
        let end = src.find("#[cfg(test)]").expect("test module not found");
        let body = &src[start..end];
        // The command breaks the read loop in BOTH modes (engine and parent), so
        // it can never be a silently ignored variant.
        assert_eq!(
            body.matches("WsCommand::Disconnect => break,").count(),
            2,
            "the Disconnect command must break the read loop in both modes"
        );
        // The switch is read at the TOP of the retry loop (so a client stopped
        // while disconnected never dials again) and again after each attempt
        // (so a detach mid-attempt neither reports nor backs off).
        let run = &body[..body.find("\n    ///").unwrap_or(body.len())];
        assert!(
            run.find("if self.is_stopped()").expect("run() must check the switch")
                < run.find("self.connect_and_run()").expect("run() must connect"),
            "the switch must be checked BEFORE the first dial, not only between retries"
        );
        assert!(
            run.matches("if self.is_stopped()").count() >= 2,
            "the switch must also be re-checked after an attempt, or a detach mid-connect backs off before it notices"
        );
    }
}
