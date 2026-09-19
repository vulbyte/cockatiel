pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/cockatiel_userdb.v1.rs"));
}

mod db;

use db::UserDatabase;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use proto::{user_db_request, User, UserDbRequest, UserDbResponse};
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = env::var("USER_DB_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9736);
    let token = env::var("USER_DB_TOKEN").unwrap_or_else(|_| "userdb-default-token".to_string());
    let db_path: PathBuf = env::var("USER_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("user_data.db"));

    let db = Arc::new(UserDatabase::new());
    db.initialize(&db_path).await?;

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    println!("[UserDB] Listening on port {} (engine-only access)", port);

    loop {
        let (stream, addr) = listener.accept().await?;
        let db = Arc::clone(&db);
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, addr, db, token).await {
                eprintln!("[UserDB] Connection error from {}: {}", addr, e);
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    db: Arc<UserDatabase>,
    expected_token: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let ws = accept_async(stream).await?;
    let (mut write, mut read) = ws.split();

    while let Some(msg) = read.next().await {
        let Ok(WsMessage::Binary(data)) = msg else {
            continue;
        };

        eprintln!("[UserDB] received {} bytes from {}", data.len(), addr);

        let request = match UserDbRequest::decode(data.as_ref()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[UserDB] malformed request: {}", e);
                send_response(&mut write, fail("Malformed request", &e.to_string())).await?;
                continue;
            }
        };

        // Auth check on every request.
        if request.auth_token != expected_token {
            send_response(&mut write, fail("Unauthorized", "Invalid auth token")).await?;
            continue;
        }

        let response = dispatch(&db, &request).await;
        send_response(&mut write, response).await?;
    }

    let _ = addr;
    Ok(())
}

async fn dispatch(db: &Arc<UserDatabase>, request: &UserDbRequest) -> UserDbResponse {
    let Some(op) = &request.op else {
        return fail("Bad request", "No operation provided");
    };

    match op {
        user_db_request::Op::AddUser(add) => {
            let channel = add.channel.as_ref();
            // Handle "user exists already": find by channel first.
            if let Some(ch) = channel {
                if let Some(existing) = db
                    .find_user_by_channel(&ch.platform, &ch.channel_id, &ch.handle)
                    .await
                    .ok()
                    .flatten()
                {
                    return ok(Some(existing), "User already exists; returning existing".to_string());
                }
            }
            match db.add_user(&add.username, channel).await {
                Ok(user) => ok(Some(user), "User added".to_string()),
                Err(e) => fail("Add user failed", &e.to_string()),
            }
        }
        user_db_request::Op::DeleteUser(del) => {
            // Permission: only the user themselves, or owner/admin.
            let target = db.get_user_by_uuid(&del.uuid7).await.ok().flatten();
            let Some(target) = target else {
                return fail("Delete failed", "User not found");
            };
            let role = del.actor_role.to_lowercase();
            let is_self = del.actor_uuid7 == del.uuid7;
            let privileged = role == "owner" || role == "admin" || target.is_owner || target.is_admin;
            if !is_self && !privileged {
                return fail(
                    "Permission denied",
                    "Only the user themselves or an owner/admin can delete a user",
                );
            }
            match db.delete_user(&del.uuid7).await {
                Ok(true) => ok(None, "User deleted".to_string()),
                Ok(false) => fail("Delete failed", "User not found"),
                Err(e) => fail("Delete failed", &e.to_string()),
            }
        }
        user_db_request::Op::AddScore(s) => {
            match db.adjust_score(&s.uuid7, s.delta, true).await {
                Ok(Some(user)) => ok(Some(user), "Score added".to_string()),
                Ok(None) => fail("Score add failed", "User not found"),
                Err(e) => fail("Score add failed", &e.to_string()),
            }
        }
        user_db_request::Op::RemoveScore(s) => {
            match db.adjust_score(&s.uuid7, -s.delta, false).await {
                Ok(Some(user)) => ok(Some(user), "Score removed".to_string()),
                Ok(None) => fail("Score remove failed", "User not found"),
                Err(e) => fail("Score remove failed", &e.to_string()),
            }
        }
        user_db_request::Op::AddChannel(ac) => {
            match db.add_channel(&ac.uuid7, ac.channel.as_ref()).await {
                Ok(Some(user)) => ok(Some(user), "Channel added".to_string()),
                Ok(None) => fail("Add channel failed", "User not found"),
                Err(e) => fail("Add channel failed", &e.to_string()),
            }
        }
        user_db_request::Op::RemoveChannel(rc) => {
            match db.remove_channel(&rc.uuid7, &rc.platform, &rc.channel_id).await {
                Ok(Some(user)) => ok(Some(user), "Channel removed".to_string()),
                Ok(None) => fail("Remove channel failed", "User not found"),
                Err(e) => fail("Remove channel failed", &e.to_string()),
            }
        }
        user_db_request::Op::GetUser(g) => {
            let user = if !g.uuid7.is_empty() {
                db.get_user_by_uuid(&g.uuid7).await.ok().flatten()
            } else {
                db.find_user_by_channel(&g.platform, &g.channel_id, &g.handle)
                    .await
                    .ok()
                    .flatten()
            };
            match user {
                Some(user) => ok(Some(user), "User found".to_string()),
                None => fail("Get user failed", "User not found"),
            }
        }
        user_db_request::Op::ListUsers(l) => {
            match db.list_users(&l.platform, l.limit, l.offset).await {
                Ok(users) => ok_many(users, "Users listed".to_string()),
                Err(e) => fail("List users failed", &e.to_string()),
            }
        }
        user_db_request::Op::UpdateFlags(uf) => {
            match db.update_flags(&uf.uuid7, &uf.flags).await {
                Ok(Some(user)) => ok(Some(user), "Flags updated".to_string()),
                Ok(None) => fail("Update flags failed", "User not found"),
                Err(e) => fail("Update flags failed", &e.to_string()),
            }
        }
        user_db_request::Op::SetRoles(sr) => {
            // Only owner/admin actors may change roles.
            let role = sr.actor_role.to_lowercase();
            if role != "owner" && role != "admin" {
                return fail("Permission denied", "Only owner/admin may change roles");
            }
            match db
                .set_roles(&sr.uuid7, sr.is_sponsor, sr.is_moderator, sr.is_admin, sr.is_owner)
                .await
            {
                Ok(Some(user)) => ok(Some(user), "Roles updated".to_string()),
                Ok(None) => fail("Set roles failed", "User not found"),
                Err(e) => fail("Set roles failed", &e.to_string()),
            }
        }
        user_db_request::Op::ReadUserValue(rv) => {
            match db.read_user_value(&rv.uuid7, &rv.key).await {
                Ok(Some(value)) => ok_value(proto::UserValueResult {
                    key: rv.key.clone(),
                    value,
                }, "Value read".to_string()),
                Ok(None) => fail("Read value failed", "Value or user not found"),
                Err(e) => fail("Read value failed", &e.to_string()),
            }
        }
        user_db_request::Op::WriteUserValue(wv) => {
            match db.write_user_value(&wv.uuid7, &wv.key, &wv.value).await {
                Ok(Some(value)) => ok_value(proto::UserValueResult {
                    key: wv.key.clone(),
                    value,
                }, "Value written".to_string()),
                Ok(None) => fail("Write value failed", "User not found"),
                Err(e) => fail("Write value failed", &e.to_string()),
            }
        }
        user_db_request::Op::DeleteUserValue(dv) => {
            match db.delete_user_value(&dv.uuid7, &dv.key).await {
                Ok(true) => ok(None, "Value deleted".to_string()),
                Ok(false) => fail("Delete value failed", "Value or user not found"),
                Err(e) => fail("Delete value failed", &e.to_string()),
            }
        }
        user_db_request::Op::ListUserValues(lv) => {
            match db.list_user_values(&lv.uuid7).await {
                Ok(entries) => {
                    let results = entries.into_iter().map(|(k, v)| proto::UserValueResult {
                        key: k,
                        value: v,
                    }).collect();
                    ok_values(results, "Values listed".to_string())
                }
                Err(e) => fail("List values failed", &e.to_string()),
            }
        }
    }
}

fn ok(user: Option<User>, message: impl Into<String>) -> UserDbResponse {
    UserDbResponse {
        success: true,
        error: String::new(),
        user,
        users: Vec::new(),
        message: message.into(),
        value: None,
        values: Vec::new(),
    }
}

fn ok_many(users: Vec<User>, message: impl Into<String>) -> UserDbResponse {
    UserDbResponse {
        success: true,
        error: String::new(),
        user: None,
        users,
        message: message.into(),
        value: None,
        values: Vec::new(),
    }
}

fn ok_value(value: proto::UserValueResult, message: impl Into<String>) -> UserDbResponse {
    UserDbResponse {
        success: true,
        error: String::new(),
        user: None,
        users: Vec::new(),
        message: message.into(),
        value: Some(value),
        values: Vec::new(),
    }
}

fn ok_values(values: Vec<proto::UserValueResult>, message: impl Into<String>) -> UserDbResponse {
    UserDbResponse {
        success: true,
        error: String::new(),
        user: None,
        users: Vec::new(),
        message: message.into(),
        value: None,
        values,
    }
}

fn fail(error: impl Into<String>, detail: impl Into<String>) -> UserDbResponse {
    UserDbResponse {
        success: false,
        error: format!("{}: {}", error.into(), detail.into()),
        user: None,
        users: Vec::new(),
        message: String::new(),
        value: None,
        values: Vec::new(),
    }
}

async fn send_response(
    write: &mut (impl SinkExt<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    response: UserDbResponse,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    response.encode(&mut buf)?;
    write.send(WsMessage::Binary(buf.into())).await?;
    Ok(())
}