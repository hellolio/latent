//! WS 控制面服务端(axum):首帧 connect 强制、token 常数时间比较、单帧
//! 1MiB 上限、单端点并发 ≤ 8、auth 失败指数退避、事件广播(per-connection
//! seq 单调递增)。

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Semaphore};

use super::auth::{constant_time_eq, AuthThrottle};
use super::events::GatewayEvent;
use super::methods;
use crate::auto_reply::Gateway;

/// 单帧/单消息上限(超限断连)。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// 单端点并发连接上限。
pub const MAX_CONNECTIONS: usize = 8;
/// 首帧 connect 超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ControlState {
    pub gateway: Arc<Gateway>,
    pub token: String,
    pub throttle: AuthThrottle,
    pub connections: Arc<Semaphore>,
}

/// 启动控制面 HTTP/WS 服务(阻塞至 server 退出;错误向上传播)。
pub async fn serve(
    gateway: Arc<Gateway>,
    bind: &str,
    port: u16,
    token: String,
) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind((bind, port))
        .await
        .map_err(|error| format!("控制面监听失败 {bind}:{port}: {error}"))?;
    eprintln!("[latent-gateway] 控制面已监听 ws://{bind}:{port}/ws");
    serve_on(listener, gateway, token).await
}

/// 在既有 listener 上启动(测试注入 port 0 用)。
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    gateway: Arc<Gateway>,
    token: String,
) -> Result<(), String> {
    let state = Arc::new(ControlState {
        gateway,
        token,
        throttle: AuthThrottle::new(),
        connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
    });
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health_handler))
        .with_state(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|error| format!("控制面服务退出: {error}"))
}

async fn health_handler(State(state): State<Arc<ControlState>>) -> &'static str {
    let _ = state.gateway.started_at;
    "ok"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ControlState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, peer.ip()))
}

/// 连接处理:握手(首帧 connect 强制)→ 请求循环 + 事件转发。
async fn handle_socket(socket: WebSocket, state: Arc<ControlState>, peer: IpAddr) {
    // 并发上限(满 = 直接断开)
    let Ok(_permit) = state.connections.clone().try_acquire_owned() else {
        eprintln!("[latent-gateway][control] 并发连接已满,拒绝 {peer}");
        return;
    };
    // 出站通道:writer 单任务消费(request 应答 + 事件帧都进这里)
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(64);
    let (sender, mut receiver) = socket.split();
    let writer = tokio::spawn(async move {
        let mut sender = sender;
        while let Some(message) = outbound_rx.recv().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });

    // 首帧必须是 connect,否则立即断连
    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, receiver.next()).await;
    let connect_id = match handshake {
        Ok(Some(Ok(Message::Text(text)))) => match parse_frame(&text) {
            Some((id, method, params)) if method == "connect" => {
                if check_auth(&state, &params, peer) {
                    id
                } else {
                    writer.abort();
                    return;
                }
            }
            _ => {
                eprintln!("[latent-gateway][control] {peer} 首帧不是 connect,断连");
                writer.abort();
                return;
            }
        },
        _ => {
            writer.abort();
            return;
        }
    };
    // hello-ok(含 server 版本、策略上限、当前状态快照)
    let hello = json!({
        "type": "hello-ok",
        "id": connect_id,
        "server": { "version": env!("CARGO_PKG_VERSION") },
        "protocol": { "min": 1, "max": 1 },
        "policy": {
            "maxPayload": MAX_FRAME_BYTES,
            "maxBufferedBytes": MAX_FRAME_BYTES * 8,
            "tickIntervalMs": 1000,
        },
        "state": {
            "uptimeMs": state.gateway.started_at.elapsed().as_millis() as u64,
            "sessions": state.gateway.registry.len().await,
        },
    });
    let _ = outbound_tx
        .send(Message::text(hello.to_string()))
        .await;

    // 事件转发(broadcast → per-connection seq 单调递增)
    let mut event_rx = state.gateway.events.subscribe();
    let event_tx = outbound_tx.clone();
    let event_task = tokio::spawn(async move {
        let mut seq: u64 = 0;
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    seq += 1;
                    let payload = serde_json::to_value(&event).unwrap_or(Value::Null);
                    let frame = json!({
                        "type": "event",
                        "event": event_name(&event),
                        "payload": payload,
                        "seq": seq,
                    });
                    if event_tx.send(Message::text(frame.to_string())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    eprintln!("[latent-gateway][control] 事件积压丢弃 {missed} 条(客户端须刷新)");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 请求循环(每请求 spawn,保持连接可响应)
    while let Some(message) = receiver.next().await {
        let Ok(text) = message else { break };
        let text = match text {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };
        // 单帧上限:超限断连
        if text.len() > MAX_FRAME_BYTES {
            eprintln!("[latent-gateway][control] {peer} 单帧超限(>1MiB),断连");
            break;
        }
        let Some((id, method, params)) = parse_frame(&text) else {
            let _ = outbound_tx
                .send(Message::text(
                    json!({"type": "res", "id": 0, "ok": false, "error": "帧解析失败"}).to_string(),
                ))
                .await;
            continue;
        };
        let gateway = state.gateway.clone();
        let reply_tx = outbound_tx.clone();
        tokio::spawn(async move {
            let result = methods::dispatch(&gateway, &method, &params).await;
            let frame = match result {
                Ok(payload) => json!({"type": "res", "id": id, "ok": true, "payload": payload}),
                Err(error) => json!({"type": "res", "id": id, "ok": false, "error": error}),
            };
            let _ = reply_tx.send(Message::text(frame.to_string())).await;
        });
    }

    event_task.abort();
    writer.abort();
}

/// 帧 → (id, method, params)。
fn parse_frame(text: &str) -> Option<(u64, String, Value)> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.get("type")?.as_str()? != "req" {
        return None;
    }
    let id = value.get("id")?.as_u64()?;
    let method = value.get("method")?.as_str()?.to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    Some((id, method, params))
}

/// 握手认证:冷却检查 + token 常数时间比较。
fn check_auth(state: &ControlState, params: &Value, peer: IpAddr) -> bool {
    if state.throttle.is_cooled_down(peer) {
        eprintln!("[latent-gateway][control] {peer} 处于认证冷却期,拒绝");
        return false;
    }
    let token = params
        .pointer("/auth/token")
        .and_then(Value::as_str)
        .or_else(|| params.get("token").and_then(Value::as_str))
        .unwrap_or_default();
    if constant_time_eq(token, &state.token) {
        state.throttle.record_success(peer);
        true
    } else {
        state.throttle.record_failure(peer);
        eprintln!("[latent-gateway][control] {peer} 认证失败");
        false
    }
}

/// 事件名(GatewayEvent 的 serde tag)。
fn event_name(event: &GatewayEvent) -> &'static str {
    match event {
        GatewayEvent::Chat { .. } => "chat",
        GatewayEvent::Agent { .. } => "agent",
        GatewayEvent::Channels { .. } => "channels",
        GatewayEvent::Health { .. } => "health",
        GatewayEvent::Shutdown => "shutdown",
    }
}
