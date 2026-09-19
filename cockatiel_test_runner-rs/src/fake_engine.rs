use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use cockatiel_client::proto::{container::Payload, *};

use crate::metrics::Metrics;

/// A fake engine: accepts any connection regardless of auth, then spams the
/// connected module with test payloads to measure compliance, throughput,
/// round-trip latency, and crash/error behavior.
pub struct FakeEngine {
    #[allow(dead_code)]
    pub port: u16,
    sessions: Arc<Mutex<Vec<u64>>>, // counter of accepted sessions
    pub metrics: Arc<Mutex<Metrics>>,
    pub done: Arc<tokio::sync::Notify>,
}

impl FakeEngine {
    pub async fn bind() -> Result<(Self, TcpListener), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind: {}", e))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        Ok((
            Self {
                port,
                sessions: Arc::new(Mutex::new(Vec::new())),
                metrics: Arc::new(Mutex::new(Metrics::new("fake-engine"))),
                done: Arc::new(tokio::sync::Notify::new()),
            },
            listener,
        ))
    }

    /// Accept connections and, for each, run the payload spam.
    /// `module_name` is the expected module name for responses.
    pub async fn run(
        self: Arc<Self>,
        listener: TcpListener,
        iterations: u64,
        module_name: String,
    ) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let this = self.clone();
            let module_name = module_name.clone();
            tokio::spawn(async move {
                let _ = this.handle_session(stream, iterations, &module_name).await;
            });
        }
    }

    async fn handle_session(
        &self,
        stream: tokio::net::TcpStream,
        iterations: u64,
        module_name: &str,
    ) -> Result<(), String> {
        let mut ws = accept_async(stream)
            .await
            .map_err(|e| format!("accept: {}", e))?;
        {
            let mut sessions = self.sessions.lock().await;
            sessions.push(1);
        }

        // The module sends its ConnectionRequest (any auth) — respond with a
        // fake ConnectionRequestReturn so the module proceeds to its loop,
        // then we begin the spam.
        if let Ok(Some(Ok(WsMessage::Binary(data)))) =
            tokio::time::timeout(Duration::from_secs(3), ws.next()).await
        {
            if let Ok(req) = Container::decode(data.as_ref()) {
                let resp = Container {
                    version: 1,
                    auth_token: "fake-token".into(),
                    module_name: "fake-engine".into(),
                    module_instance_uuid7: req.module_instance_uuid7.clone(),
                    payload: Some(Payload::ConnectionRequestReturn(
                        cockatiel_client::proto::ConnectionRequestReturn {
                            new_port: 0,
                            module_instance_uuid7: req.module_instance_uuid7,
                        },
                    )),
                };
                let mut buf = Vec::new();
                let _ = resp.encode(&mut buf);
                let _ = ws.send(WsMessage::Binary(buf.into())).await;
            }
        }

        let mut metrics = self.metrics.lock().await;
        metrics.total_msgs += iterations;
        drop(metrics);

        let mut latencies = Vec::new();
        let burst_start = Instant::now();

        for i in 0..iterations {
            let payload = Payload::MessageInProcess(MessageInProcess {
                message_uuid7: uuid::Uuid::now_v7().to_string(),
                raw_message: Some(ChatMessage {
                    platform: "test".into(),
                    raw_data: vec![],
                    raw_message: format!("fake message {}", i),
                    user_uuid7: String::new(),
                    command: None,
                    user_data: None,
                }),
                processed_message: String::new(),
                abandon_message: false,
            });
            let container = Container {
                version: 1,
                auth_token: "fake-token".into(),
                module_name: "fake-engine".into(),
                module_instance_uuid7: String::new(),
                payload: Some(payload),
            };
            let mut buf = Vec::new();
            container.encode(&mut buf).map_err(|e| e.to_string())?;

            let item_start = Instant::now();
            if ws.send(WsMessage::Binary(buf.into())).await.is_err() {
                metrics = self.metrics.lock().await;
                metrics.notes.push(format!(
                    "module '{}' closed connection at msg {}",
                    module_name, i
                ));
                metrics.failed += 1;
                break;
            }

            // Read the module's response (or timeout). This measures the WS
            // round-trip — a core part of the test.
            let resp = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
            match resp {
                Ok(Some(Ok(WsMessage::Binary(data)))) => {
                    if let Ok(c) = Container::decode(data.as_ref()) {
                        let _ = c;
                        // Any response is a success: compliance means the
                        // module didn't crash or error on the payload.
                        let mut m = self.metrics.lock().await;
                        m.passed += 1;
                    } else {
                        let mut m = self.metrics.lock().await;
                        m.notes.push(format!("undecodable response at msg {}", i));
                        m.failed += 1;
                    }
                }
                Ok(Some(Ok(WsMessage::Close(_)))) => {
                    let mut m = self.metrics.lock().await;
                    m.notes.push(format!("module '{}' closed ws at msg {}", module_name, i));
                    m.failed += 1;
                    break;
                }
                Ok(Some(Ok(_))) => {
                    let mut m = self.metrics.lock().await;
                    m.notes.push(format!("non-binary response at msg {}", i));
                    m.failed += 1;
                }
                Ok(Some(Err(e))) => {
                    let mut m = self.metrics.lock().await;
                    m.notes.push(format!("ws error at msg {}: {}", i, e));
                    m.failed += 1;
                    break;
                }
                Ok(None) | Err(_) => {
                    // Timeout — module may be slow or unresponsive.
                    let mut m = self.metrics.lock().await;
                    m.notes.push(format!("module '{}' unresponsive at msg {}", module_name, i));
                    m.failed += 1;
                }
            }
            latencies.push(item_start.elapsed());
        }

        {
            let mut m = self.metrics.lock().await;
            m.duration_ms = burst_start.elapsed().as_millis();
            m.record(&latencies);
            m.finalize();
        }
        self.done.notify_waiters();

        let _ = ws.close(None).await;
        Ok(())
    }
}