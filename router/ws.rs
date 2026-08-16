use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::jwt::Claims;
use crate::router::middleware::RateLimiter;
use crate::server::Server;
use crate::state::DaemonState;

pub fn router() -> Router<DaemonState> {
    Router::new().route("/api/servers/:server/ws", get(ws_route))
}

/// JSON envelope used in both directions.
#[derive(Debug, Serialize)]
struct WsMessage {
    event: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct InboundMessage {
    event: String,
    #[serde(default)]
    args: Vec<String>,
}

/// GET /api/servers/:id/ws — authenticated in-band via the `auth` event.
async fn ws_route(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<DaemonState>,
    Path(server_uuid): Path<Uuid>,
) -> AppResult<Response> {
    let config = state.config.read().await.clone();

    // Origin must match the panel location or allowed_origins ("*" ok).
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let panel = config.panel_url();
        let allowed = origin == panel
            || config.allowed_origins.iter().any(|a| a == "*" || a == origin)
            || (config.allow_cors_private_network && origin.starts_with("http://"));
        if !allowed {
            tracing::warn!(origin, "websocket origin not allowed");
            return Err(AppError::Forbidden("origin is not allowed".into()));
        }
    }

    let server = state.manager.get(server_uuid).await?;

    if server.ws_connections.load(Ordering::SeqCst) >= crate::server::MAX_WEBSOCKETS_PER_SERVER {
        return Err(AppError::BadRequest("Too many open websocket connections.".into()));
    }

    Ok(ws.on_upgrade(move |socket| handle_socket(socket, state, server)))
}

/// Send one protocol message.
macro_rules! ws_send {
    ($socket:expr, $event:expr, $args:expr) => {{
        let msg = WsMessage {
            event: $event.to_string(),
            args: $args,
        };
        if let Ok(text) = serde_json::to_string(&msg) {
            let _ = $socket.send(Message::Text(text.into())).await;
        }
    }};
}

struct Authed {
    token: String,
    claims: Claims,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn handle_socket(mut socket: WebSocket, state: DaemonState, server: Arc<Server>) {
    server.ws_connections.fetch_add(1, Ordering::SeqCst);

    let mut authenticated: Option<Authed> = None;
    let mut first_auth_done = false;
    let mut events_rx: Option<tokio::sync::broadcast::Receiver<crate::server::events::ServerEvent>> = None;
    let mut rate = RateLimiter::new(10, Duration::from_millis(200));
    let mut throttled_sent = false;
    let mut expired_sent = false;

    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await; // skip the immediate first tick

    loop {
        let subscribed = events_rx.is_some();
        let event_fut = async {
            let mut rx = events_rx.as_ref().unwrap().resubscribe();
            rx.recv().await
        };

        tokio::select! {
            _ = interval.tick() => {
                if let Some(a) = &authenticated {
                    let exp = a.claims.exp as i64;
                    let now = now_unix();
                    if exp - now <= 0 {
                        if !expired_sent {
                            ws_send!(&mut socket, "token expired", Vec::<String>::new());
                            expired_sent = true;
                        }
                    } else if exp - now <= 60 {
                        ws_send!(&mut socket, "token expiring", Vec::<String>::new());
                    }
                }
            }
            event = event_fut, if subscribed => {
                match event {
                    Ok(event) => {
                        // permission-gated events
                        if let Some(perm) = event.required_permission() {
                            let allowed = authenticated
                                .as_ref()
                                .map(|a| a.claims.has_permission(perm))
                                .unwrap_or(false);
                            if !allowed {
                                continue;
                            }
                        }
                        ws_send!(&mut socket, event.event_name(), event.args());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if text.len() > 32768 {
                            continue; // oversized -> dropped
                        }
                        let Ok(inbound) = serde_json::from_str::<InboundMessage>(&text) else {
                            continue;
                        };

                        if !rate.allow() {
                            if !throttled_sent {
                                ws_send!(&mut socket, "throttled", vec!["global".to_string()]);
                                throttled_sent = true;
                            }
                            continue;
                        }
                        throttled_sent = false;

                        handle_inbound(
                            &mut socket,
                            &state,
                            &server,
                            &inbound,
                            &mut authenticated,
                            &mut first_auth_done,
                            &mut events_rx,
                        )
                        .await;
                    }
                    Some(Ok(Message::Ping(_))) => {
                        let _ = socket.send(Message::Pong(Vec::new())).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "websocket recv error");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    server.ws_connections.fetch_sub(1, Ordering::SeqCst);
    tracing::debug!(uuid = %server.uuid, "websocket closed");
}

#[allow(clippy::too_many_arguments)]
async fn handle_inbound(
    socket: &mut WebSocket,
    state: &DaemonState,
    server: &Arc<Server>,
    inbound: &InboundMessage,
    authenticated: &mut Option<Authed>,
    first_auth_done: &mut bool,
    events_rx: &mut Option<tokio::sync::broadcast::Receiver<crate::server::events::ServerEvent>>,
) {
    macro_rules! jwt_error {
        ($msg:expr) => {{
            ws_send!(socket, "jwt error", vec![$msg.to_string()]);
        }};
    }

    if inbound.event == "auth" {
        let token = inbound.args.join("");
        if token.is_empty() {
            jwt_error!("jwt: no jwt present");
            return;
        }

        let secret = state.config.read().await.token.clone();
        match crate::jwt::parse_token(&token, secret.as_bytes(), &state.tokens, state.boot_time).await {
            Err(e) => jwt_error!(e.to_string()),
            Ok(claims) => {
                if !claims.has_scope("websocket") {
                    jwt_error!("jwt: invalid scope");
                    return;
                }
                if !claims.has_permission("websocket.connect") {
                    jwt_error!("jwt: missing connect permission");
                    return;
                }
                let claim_server = claims.server_uuid().and_then(|s| Uuid::parse_str(s).ok());
                if claim_server != Some(server.uuid) {
                    jwt_error!("jwt: server uuid mismatch");
                    return;
                }
                if server.suspended.load(Ordering::SeqCst) {
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame {
                            code: 4409,
                            reason: "server is suspended".into(),
                        })))
                        .await;
                    return;
                }

                *authenticated = Some(Authed { token, claims });
                ws_send!(socket, "auth success", Vec::<String>::new());

                if !*first_auth_done {
                    *first_auth_done = true;
                    *events_rx = Some(server.subscribe());
                    // Current state + stats right away.
                    let current_state = server.query_state().await;
                    ws_send!(socket, "status", vec![current_state.as_str().to_string()]);
                    if current_state == crate::server::ServerState::Offline {
                        if let Ok(json_str) = serde_json::to_string(&server.usage().await) {
                            ws_send!(socket, "stats", vec![json_str]);
                        }
                    }
                }
                // re-auth: just swap the token (no duplicate status)
            }
        }
        return;
    }

    // Everything else needs a valid token first.
    let Some(authed) = authenticated.as_ref() else {
        jwt_error!("jwt: no jwt present");
        return;
    };

    // Re-validate the token on every message, like wings.
    let secret = state.config.read().await.token.clone();
    let claims = match crate::jwt::parse_token(&authed.token, secret.as_bytes(), &state.tokens, state.boot_time).await {
        Ok(c) => c,
        Err(e) => {
            jwt_error!(e.to_string());
            return;
        }
    };

    match inbound.event.as_str() {
        "set state" => {
            let action = inbound.args.first().map(|s| s.as_str()).unwrap_or("");
            let user = claims.user_uuid.clone();
            match action {
                "start" if claims.has_permission("control.start") => {
                    state.activity.push(
                        crate::models::Activity::new(&server.uuid.to_string(), "server:power.start")
                            .with_user(user),
                    );
                    let srv = server.clone();
                    tokio::spawn(async move { let _ = srv.power_start().await; });
                }
                "stop" if claims.has_permission("control.stop") => {
                    state.activity.push(
                        crate::models::Activity::new(&server.uuid.to_string(), "server:power.stop")
                            .with_user(user),
                    );
                    let srv = server.clone();
                    tokio::spawn(async move { let _ = srv.power_stop(30).await; });
                }
                "restart" if claims.has_permission("control.restart") => {
                    state.activity.push(
                        crate::models::Activity::new(&server.uuid.to_string(), "server:power.restart")
                            .with_user(user),
                    );
                    let srv = server.clone();
                    tokio::spawn(async move { let _ = srv.power_restart(30).await; });
                }
                "kill" if claims.has_permission("control.stop") => {
                    state.activity.push(
                        crate::models::Activity::new(&server.uuid.to_string(), "server:power.kill")
                            .with_user(user),
                    );
                    let srv = server.clone();
                    tokio::spawn(async move { let _ = srv.power_kill().await; });
                }
                other => {
                    tracing::warn!(action = ?other, "invalid or unauthorized set state");
                }
            }
        }
        "send command" if claims.has_permission("control.console") => {
            let command = inbound.args.join("");
            if !command.is_empty() {
                state.activity.push(
                    crate::models::Activity::new(&server.uuid.to_string(), "server:console.command")
                        .with_user(claims.user_uuid.clone())
                        .with_metadata(serde_json::json!({ "command": command })),
                );
                let _ = server.send_command(&command).await;
            }
        }
        "send logs" => {
            for line in server.recent_logs().await {
                ws_send!(socket, "console output", vec![line]);
            }
        }
        "send stats" => {
            if let Ok(json_str) = serde_json::to_string(&server.usage().await) {
                ws_send!(socket, "stats", vec![json_str]);
            }
        }
        other => {
            tracing::debug!(event = %other, "unhandled websocket event");
        }
    }
}