use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::accept_async;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use uuid::Uuid;
use cockatiel_client::proto::*;
use cockatiel_client::proto::container::Payload;

use crate::ws_client::{WsCommand, WsEvent};

pub struct WsServer {
    pub addr: SocketAddr,
    pub auth_token: String,
}

impl WsServer {
    pub async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let auth_token = Uuid::now_v7().to_string();
        Self { addr, auth_token }
    }

    /// `ws_command_tx` relays queries that detached children issue (e.g. the
    /// users window's `Action::UserQuery`) to the parent's engine connection.
    pub fn start(self, rx: broadcast::Receiver<WsEvent>, ws_command_tx: mpsc::UnboundedSender<WsCommand>) {
        let addr = self.addr;
        let auth_token = self.auth_token.clone();

        tokio::spawn(async move {
            let listener = match TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("WS server failed to bind: {}", e);
                    return;
                }
            };

            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        let auth_token = auth_token.clone();
                        let rx = rx.resubscribe();
                        let ws_command_tx = ws_command_tx.clone();
                        tokio::spawn(handle_child(stream, peer_addr, auth_token, rx, ws_command_tx));
                    }
                    Err(e) => eprintln!("WS server: accept error: {}", e),
                }
            }
        });
    }
}

async fn handle_child(
    stream: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    auth_token: String,
    mut rx: broadcast::Receiver<WsEvent>,
    ws_command_tx: mpsc::UnboundedSender<WsCommand>,
) {
    let mut ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("WS server: accept error: {}", e);
            return;
        }
    };

    // Phase 1: receive ConnectionRequest
    let my_uuid = loop {
        let msg = match ws_stream.next().await {
            Some(Ok(msg)) => msg,
            _ => return,
        };

        let data = match msg {
            tokio_tungstenite::tungstenite::Message::Binary(d) => d,
            _ => continue,
        };

        let container = match Container::decode(data.as_slice()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("WS server: decode error: {}", e);
                return;
            }
        };

        match container.payload {
            Some(Payload::ConnectionRequest(_req)) => {
                // For parent mode, pin is 0 and we use auth_token for validation
                // The child sends auth_token in the Container.auth_token field
                if container.auth_token != auth_token {
                    let _ = ws_stream.close(None).await;
                    return;
                }

                let my_uuid = Uuid::now_v7().to_string();
                let return_msg = Container {
                    version: 1,
                    auth_token: String::new(),
                    module_name: "cockatiel-tui-child".into(),
                    module_instance_uuid7: String::new(),
                    payload: Some(Payload::ConnectionRequestReturn(ConnectionRequestReturn {
                        new_port: 0,
                        module_instance_uuid7: my_uuid.clone(),
                    })),
                };
                let _ = ws_stream
                    .send(tokio_tungstenite::tungstenite::Message::Binary(
                        return_msg.encode_to_vec().into(),
                    ))
                    .await;

                break my_uuid;
            }
            _ => continue,
        }
    };

    // Phase 2: wait for auth reconnect
    loop {
        let msg = match ws_stream.next().await {
            Some(Ok(msg)) => msg,
            _ => return,
        };

        let data = match msg {
            tokio_tungstenite::tungstenite::Message::Binary(d) => d,
            _ => continue,
        };

        if let Ok(container) = Container::decode(data.as_slice()) {
            if container.auth_token == my_uuid {
                break;
            }
        }
    }

    eprintln!("WS server: child {} authenticated", peer_addr);

    let (mut sink, mut stream) = ws_stream.split();
    let child_uuid = my_uuid.clone();

    // Forward broadcast events to child
    let forward_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(WsEvent::Log { source, message, event_type: _ }) => {
                    let msg = Container {
                        version: 1,
                        auth_token: my_uuid.clone(),
                        module_name: "cockatiel-tui".into(),
                        module_instance_uuid7: my_uuid.clone(),
                        payload: Some(Payload::Log(Log {
                            log: format!("[{}] {}", source, message),
                            blob: Vec::new(),
                        })),
                    };
                    if sink
                        .send(tokio_tungstenite::tungstenite::Message::Binary(
                            msg.encode_to_vec().into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // Query results (polled stats + the child's own one-shot
                // userdb queries) are forwarded so the child runs the same
                // `update_stats_from_query` merge into its GlobalStats.
                Ok(WsEvent::QueryResult { result, .. }) => {
                    let msg = Container {
                        version: 1,
                        auth_token: my_uuid.clone(),
                        module_name: "cockatiel-tui".into(),
                        module_instance_uuid7: my_uuid.clone(),
                        payload: Some(Payload::DatabaseQueryResult(result)),
                    };
                    if sink
                        .send(tokio_tungstenite::tungstenite::Message::Binary(
                            msg.encode_to_vec().into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(WsEvent::Connected) => {
                    let msg = Container {
                        version: 1,
                        auth_token: my_uuid.clone(),
                        module_name: "cockatiel-tui".into(),
                        module_instance_uuid7: my_uuid.clone(),
                        payload: Some(Payload::Log(Log {
                            log: "parent connected".into(),
                            blob: Vec::new(),
                        })),
                    };
                    if sink
                        .send(tokio_tungstenite::tungstenite::Message::Binary(
                            msg.encode_to_vec().into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                _ => {}
            }
        }
    });

    // Forward child→parent messages: one-shot queries (e.g. the users window's
    // `Action::UserQuery`) are relayed to the engine via the parent's command
    // channel; the result comes back through the QueryResult broadcast.
    let incoming_task = tokio::spawn(async move {
        while let Some(msg) = stream.next().await {
            let Ok(tokio_tungstenite::tungstenite::Message::Binary(data)) = msg else {
                continue;
            };
            let Ok(container) = Container::decode(data.as_slice()) else {
                continue;
            };
            if container.auth_token != child_uuid {
                continue;
            }
            if let Some(Payload::DatabaseQuery(query)) = container.payload {
                let _ = ws_command_tx.send(WsCommand::SendQuery {
                    query_id: query.query_id,
                    sql: query.sql,
                });
            }
        }
    });

    let _ = incoming_task.await;
    forward_task.abort();
}
