//! QQ 渠道(feature `qq`):**OneBot 11 协议 + 反向 WebSocket**(NapCat 是
//! WS **客户端**,gateway 是服务端 —— gateway 无需公网 IP)。
//!
//! 已核实语义(§6 坑 3 / §2.6):
//! - `Authorization: Bearer <accessToken>` 校验(**不走 query 参数** —— query
//!   会进访问日志);token 比较用常数时间实现;
//! - 事件按 `post_type` 分发;action 请求带 `echo` 字段 + oneshot 表关联响应;
//!   `meta_event.heartbeat` 判活;多 NapCat 实例按 `self_id` 路由(MVP 取最新连接);
//! - **NapCat 会上报机器人自己的发言(user_id == self_id),归一化先丢自消息,
//!   否则私聊恒 to_me 直接死循环**(§2.3 防环硬规则);
//! - 收:`message.private` / `message.group`(text/at/face/reply/image 段);
//!   发:`send_private_msg` / `send_group_msg`;
//! - 资源上限与控制面统一:单帧 1 MiB 断连、单端点并发 ≤ 8、auth 失败
//!   5 次进入 60s 冷却(按来源)。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, watch, Mutex, RwLock as AsyncRwLock, Semaphore};

use crate::error::ChannelError;
use crate::plugin::{spawn_guarded, ChannelCommand, ChannelHandle, ChannelPlugin, ChannelSender};
use crate::types::{
    ChatRef, ChannelEvent, ChannelStatus, InboundMessage, OutboundMessage, Sender, Segment,
};

/// 单帧上限(与控制面统一)。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// 单端点并发连接上限。
pub const MAX_CONNECTIONS: usize = 8;
/// 连续 auth 失败进入冷却的阈值。
pub const MAX_AUTH_FAILURES: u32 = 5;
/// auth 失败冷却时长。
pub const AUTH_COOLDOWN: Duration = Duration::from_secs(60);
/// auth 失败表容量上限(公网 bind 下防慢性泄漏;超限先清过期冷却再挤掉一条)。
const MAX_AUTH_FAILURE_ENTRIES: usize = 4096;
/// action 应答等待上限。
const ACTION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct QqConfig {
    /// 反向 WS 服务端监听地址(默认且强烈建议 loopback)
    pub reverse_ws_host: String,
    pub reverse_ws_port: u16,
    /// 必填;缺失/为空 → 该渠道拒绝启动
    pub access_token: String,
    /// WS 路径(NapCat 可配置;默认 /ws;必须以 / 开头且非 /,非法值拒绝启动)
    pub path: String,
}

impl Default for QqConfig {
    fn default() -> Self {
        QqConfig {
            reverse_ws_host: "127.0.0.1".into(),
            reverse_ws_port: 3001,
            access_token: String::new(),
            path: "/ws".into(),
        }
    }
}

impl QqConfig {
    pub fn from_json(raw: &serde_json::Value) -> Result<Self, ChannelError> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        struct Raw {
            reverse_ws_host: String,
            reverse_ws_port: u16,
            access_token: String,
            path: String,
        }
        impl Default for Raw {
            fn default() -> Self {
                Raw {
                    reverse_ws_host: "127.0.0.1".into(),
                    reverse_ws_port: 3001,
                    access_token: String::new(),
                    path: "/ws".into(),
                }
            }
        }
        let raw: Raw = serde_json::from_value(raw.clone())
            .map_err(|error| ChannelError::Config(format!("qq 配置解析失败: {error}")))?;
        Ok(QqConfig {
            reverse_ws_host: raw.reverse_ws_host,
            reverse_ws_port: raw.reverse_ws_port,
            access_token: raw.access_token,
            path: raw.path,
        })
    }
}

/// 运行期共享状态(连接注册表 + echo 表 + 自消息防环)。
pub struct QqState {
    pub config: QqConfig,
    /// 当前活跃 NapCat 连接的出站口(最新连接优先)
    pub connection: AsyncRwLock<Option<mpsc::Sender<axum::extract::ws::Message>>>,
    /// action echo → 应答 oneshot
    pub echo_table: Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>,
    pub next_echo: AtomicU64,
    /// 机器人 self_id(连接建立后得知)
    pub self_id: AsyncRwLock<Option<String>>,
    /// 已发送消息 id(reply_to_me 判定)
    pub sent_message_ids: Mutex<HashSet<String>>,
    pub event_tx: mpsc::Sender<ChannelEvent>,
    /// 端点资源上限
    pub connections: Arc<Semaphore>,
    /// 按来源的 auth 失败退避(计数 + 冷却截止;键 = 对端 IP,P1-1:
    /// SocketAddr 含临时端口,重连即换键,节流失效)
    pub auth_failures: Mutex<HashMap<IpAddr, (u32, Option<Instant>)>>,
    /// 绑定地址(测试/status 用)
    pub bound_addr: RwLock<Option<SocketAddr>>,
    /// 状态 watch(handle.status() 读口)
    pub status_tx: watch::Sender<ChannelStatus>,
}

impl QqState {
    /// action 出站:发 JSON + 等 echo 应答。
    pub async fn call_action(
        &self,
        action: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ChannelError> {
        let connection = self.connection.read().await.clone();
        let Some(sender) = connection else {
            return Err(ChannelError::DeliveryFailed("NapCat 未连接".into()));
        };
        let echo = self.next_echo.fetch_add(1, Ordering::Relaxed).to_string();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.echo_table.lock().await.insert(echo.clone(), reply_tx);
        let frame = serde_json::json!({
            "action": action,
            "params": params,
            "echo": echo,
        });
        if sender
            .send(axum::extract::ws::Message::text(frame.to_string()))
            .await
            .is_err()
        {
            self.echo_table.lock().await.remove(&echo);
            return Err(ChannelError::DeliveryFailed("NapCat 连接已断开".into()));
        }
        let response = match tokio::time::timeout(ACTION_TIMEOUT, reply_rx).await {
            Ok(Ok(response)) => response,
            _ => {
                self.echo_table.lock().await.remove(&echo);
                return Err(ChannelError::DeliveryFailed("action 应答超时".into()));
            }
        };
        // retcode 0 = ok;其余按 wording 分类
        let retcode = response
            .get("retcode")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        if retcode == 0 {
            return Ok(response);
        }
        let wording = response
            .get("wording")
            .and_then(serde_json::Value::as_str)
            .or_else(|| response.get("status").and_then(serde_json::Value::as_str))
            .unwrap_or("unknown");
        Err(match wording {
            w if w.contains("群") && w.contains("不存在") => ChannelError::ChatNotFound,
            w if w.contains("不在群") || w.contains("not in group") => ChannelError::NotInGroup,
            _ => ChannelError::DeliveryFailed(format!(
                "OneBot action 失败(retcode {retcode}): {wording}"
            )),
        })
    }

    /// 记录已发送消息 id(reply_to_me 判定;上限防泄漏)。
    pub async fn track_sent_message(&self, message_id: String) {
        let mut ids = self.sent_message_ids.lock().await;
        if ids.len() >= 4096 {
            ids.clear();
        }
        ids.insert(message_id);
    }
}

pub struct QqChannel {
    config: RwLock<Option<QqConfig>>,
    /// 绑定地址(测试断言用)
    bound_addr: RwLock<Option<SocketAddr>>,
}

impl QqChannel {
    pub fn new() -> Self {
        QqChannel {
            config: RwLock::new(None),
            bound_addr: RwLock::new(None),
        }
    }

    /// 实际绑定的地址(端口 0 = 内核分配;测试连接目标)。
    pub fn bound_addr(&self) -> Option<SocketAddr> {
        *self.bound_addr.read().unwrap()
    }

    fn take_config(&self) -> Result<QqConfig, ChannelError> {
        self.config
            .write()
            .unwrap()
            .take()
            .ok_or_else(|| ChannelError::Config("qq 渠道未注入配置".into()))
    }
}

impl Default for QqChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelPlugin for QqChannel {
    fn id(&self) -> &'static str {
        "qq"
    }

    fn apply_config(&self, raw: &serde_json::Value) -> Result<(), ChannelError> {
        let config = QqConfig::from_json(raw)?;
        if config.access_token.trim().is_empty() {
            return Err(ChannelError::Config(
                "qq accessToken 必填(缺失/为空 → 该渠道拒绝启动)".into(),
            ));
        }
        // P1-7:path 不以 / 开头会让 axum route panic;/ 与兜底路由重复注册
        // 同样 panic —— 配置非法一律 Config 错误拒绝启动,绝不 panic
        if !config.path.starts_with('/') || config.path == "/" {
            return Err(ChannelError::Config(format!(
                "qq path 非法: {:?}(必须以 / 开头且非 /)",
                config.path
            )));
        }
        *self.config.write().unwrap() = Some(config);
        Ok(())
    }

    async fn start(
        &self,
        tx: mpsc::Sender<ChannelEvent>,
    ) -> Result<ChannelHandle, ChannelError> {
        let config = self.take_config()?;

        // watch 状态通道(初始等待 NapCat;连接建立 → Connected,断开 → Disconnected)
        let (status_tx, status_rx) = watch::channel(ChannelStatus::Disconnected {
            reason: "waiting for NapCat".into(),
        });
        let state = Arc::new(QqState {
            config: config.clone(),
            connection: AsyncRwLock::new(None),
            echo_table: Mutex::new(HashMap::new()),
            next_echo: AtomicU64::new(1),
            self_id: AsyncRwLock::new(None),
            sent_message_ids: Mutex::new(HashSet::new()),
            event_tx: tx.clone(),
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            auth_failures: Mutex::new(HashMap::new()),
            bound_addr: RwLock::new(None),
            status_tx,
        });

        // axum 反向 WS 服务端(自持;panic → Failed)
        let app_state = state.clone();
        let bind_addr: SocketAddr = format!("{}:{}", config.reverse_ws_host, config.reverse_ws_port)
            .parse()
            .map_err(|error| ChannelError::Config(format!("reverseWs 地址非法: {error}")))?;
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|error| ChannelError::Startup(format!("反向 WS 监听失败: {error}")))?;
        let bound = listener
            .local_addr()
            .map_err(|error| ChannelError::Startup(error.to_string()))?;
        *self.bound_addr.write().unwrap() = Some(bound);
        *state.bound_addr.write().unwrap() = Some(bound);

        let path = config.path.clone();
        let app = axum::Router::new()
            .route(&path, axum::routing::get(ws_handler))
            .route("/", axum::routing::get(ws_handler))
            .with_state(app_state);
        let server_status_tx = state.status_tx.clone();
        spawn_guarded("qq:server", tx.clone(), async move {
            if let Err(error) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                eprintln!("[latent-channel:qq] WS 服务端退出: {error}");
                let _ = server_status_tx.send(ChannelStatus::Failed {
                    reason: format!("WS 服务端退出: {error}"),
                });
            }
        });

        // 出站命令任务
        let command_state = state.clone();
        let (command_tx, command_rx) = mpsc::channel::<ChannelCommand>(64);
        spawn_guarded("qq:commands", tx, async move {
            run_qq_command_loop(command_rx, command_state).await;
        });

        Ok(ChannelHandle::new(ChannelSender::new(command_tx), status_rx))
    }
}

/// QQ 出站命令循环(Send → OneBot action;Typing → no-op)。
async fn run_qq_command_loop(mut rx: mpsc::Receiver<ChannelCommand>, state: Arc<QqState>) {
    while let Some(command) = rx.recv().await {
        match command {
            ChannelCommand::Send {
                chat,
                message,
                reply,
            } => {
                let result = send_via_onebot(&state, &chat, &message).await;
                let _ = reply.send(result);
            }
            ChannelCommand::Typing { .. } => {
                // OneBot 11 无 typing 动作:静默抑制(对齐 OpenClaw 不支持渠道)
            }
            ChannelCommand::Shutdown => break,
        }
    }
}

/// 发送:send_private_msg / send_group_msg(段 → OneBot message 数组)。
async fn send_via_onebot(
    state: &QqState,
    chat: &ChatRef,
    message: &OutboundMessage,
) -> Result<(), ChannelError> {
    let mut onebot_segments: Vec<serde_json::Value> = Vec::new();
    if let Some(reply_to) = &message.reply_to {
        onebot_segments.push(serde_json::json!({
            "type": "reply",
            "data": { "id": reply_to },
        }));
    }
    for segment in &message.segments {
        match segment {
            Segment::Text(text) => onebot_segments.push(serde_json::json!({
                "type": "text",
                "data": { "text": text },
            })),
            Segment::At { user_id } => {
                let qq = if user_id == "all" {
                    serde_json::json!("all")
                } else {
                    serde_json::json!(user_id.parse::<i64>().unwrap_or(-1))
                };
                onebot_segments.push(serde_json::json!({
                    "type": "at",
                    "data": { "qq": qq },
                }));
            }
            Segment::Image { url, file_id } => {
                onebot_segments.push(serde_json::json!({
                    "type": "image",
                    "data": { "url": url, "file": file_id },
                }));
            }
            Segment::File { file_id, name, .. } => {
                onebot_segments.push(serde_json::json!({
                    "type": "file",
                    "data": { "file": file_id, "name": name },
                }));
            }
            Segment::Reply { .. } => {}
        }
    }
    let (action, id_field) = match chat.chat_type {
        crate::types::ChatType::Private => ("send_private_msg", "user_id"),
        crate::types::ChatType::Group => ("send_group_msg", "group_id"),
    };
    let params = serde_json::json!({
        id_field: chat.conversation_id.parse::<i64>().unwrap_or(0),
        "message": onebot_segments,
    });
    let response = state.call_action(action, params).await?;
    // 记录已发送消息 id(reply_to_me 判定)
    if let Some(message_id) = response
        .pointer("/data/message_id")
        .and_then(serde_json::Value::as_i64)
    {
        state.track_sent_message(message_id.to_string()).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// WS 服务端(axum)
// ---------------------------------------------------------------------------

async fn ws_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<QqState>>,
    headers: axum::http::HeaderMap,
    addr: axum::extract::ConnectInfo<SocketAddr>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let peer = addr.0;
    let peer_ip = peer.ip();
    // 冷却检查(连续 5 次失败 → 60s;期间一律拒绝;键 = 对端 IP,P1-1)
    {
        let failures = state.auth_failures.lock().await;
        if let Some((_, Some(until))) = failures.get(&peer_ip) {
            if *until > Instant::now() {
                return (StatusCode::FORBIDDEN, "auth cooldown").into_response();
            }
        }
    }
    // Bearer token 校验(常数时间;Header 不走 query)
    let expected = format!("Bearer {}", state.config.access_token);
    let provided = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(provided, &expected) {
        let mut failures = state.auth_failures.lock().await;
        // 失败表有界(P1-1):公网 bind 下防慢性泄漏
        if failures.len() >= MAX_AUTH_FAILURE_ENTRIES && !failures.contains_key(&peer_ip) {
            failures.retain(|_, (_, until)| until.is_some_and(|until| until > Instant::now()));
            if failures.len() >= MAX_AUTH_FAILURE_ENTRIES {
                if let Some(oldest) = failures.keys().next().cloned() {
                    failures.remove(&oldest);
                }
            }
        }
        let entry = failures.entry(peer_ip).or_insert((0, None));
        entry.0 += 1;
        if entry.0 >= MAX_AUTH_FAILURES {
            entry.1 = Some(Instant::now() + AUTH_COOLDOWN);
            entry.0 = 0;
        }
        eprintln!("[latent-channel:qq] {peer} 认证失败({MAX_AUTH_FAILURES} 次内进入冷却)");
        return (StatusCode::FORBIDDEN, "invalid token").into_response();
    }
    state.auth_failures.lock().await.remove(&peer_ip);
    // 并发上限
    if state.connections.available_permits() == 0 {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many connections").into_response();
    }
    // P1-2:帧/消息上限前置到 WS 升级(应用层检查保留作双保险)——
    // 否则整帧先被 axum 缓冲(默认上限 ~64MiB)才轮到应用层断连
    ws.max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| handle_connection(socket, state, peer))
}

/// NapCat 连接任务:事件归一化 + echo 路由 + 自消息防环。
async fn handle_connection(
    socket: axum::extract::ws::WebSocket,
    state: Arc<QqState>,
    peer: SocketAddr,
) {
    use axum::extract::ws::Message as AxumMessage;
    use futures::{SinkExt, StreamExt};

    let _permit = match state.connections.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return,
    };
    let (mut sender, mut receiver) = socket.split();
    // 注册为当前连接(最新优先)
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<AxumMessage>(64);
    *state.connection.write().await = Some(outbound_tx.clone());
    let writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });
    eprintln!("[latent-channel:qq] NapCat 已连接:{peer}");

    while let Some(message) = receiver.next().await {
        let Ok(message) = message else { break };
        let text = match message {
            AxumMessage::Text(text) => text,
            AxumMessage::Close(_) => break,
            _ => continue,
        };
        // 单帧上限:超限断连
        if text.len() > MAX_FRAME_BYTES {
            eprintln!("[latent-channel:qq] {peer} 单帧超限(>1MiB),断连");
            break;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        handle_onebot_event(&state, value).await;
    }

    // 连接清理(仅当自己仍是当前连接)
    {
        let mut connection = state.connection.write().await;
        if connection
            .as_ref()
            .map(|c| c.same_channel(&outbound_tx))
            .unwrap_or(false)
        {
            *connection = None;
            let disconnected = ChannelStatus::Disconnected {
                reason: "NapCat 断开".into(),
            };
            let _ = state.status_tx.send(disconnected.clone());
            let _ = state.event_tx.send(ChannelEvent::Status(disconnected)).await;
        }
    }
    writer.abort();
    eprintln!("[latent-channel:qq] NapCat 断开:{peer}");
}

/// OneBot 事件分发:meta_event(lifecycle/heartbeat)/ action 应答(echo)/ 消息。
async fn handle_onebot_event(state: &QqState, value: serde_json::Value) {
    let post_type = value.get("post_type").and_then(serde_json::Value::as_str);
    match post_type {
        Some("meta_event") => {
            // lifecycle connect → 记 self_id + Connected;heartbeat → 判活(MVP 忽略)
            if let Some(self_id) = value.get("self_id").and_then(serde_json::Value::as_i64) {
                let self_id = self_id.to_string();
                let mut guard = state.self_id.write().await;
                if guard.as_deref() != Some(self_id.as_str()) {
                    *guard = Some(self_id.clone());
                    drop(guard);
                    let connected = ChannelStatus::Connected {
                        account_id: self_id,
                    };
                    let _ = state.status_tx.send(connected.clone());
                    let _ = state.event_tx.send(ChannelEvent::Status(connected)).await;
                }
            }
        }
        Some("message") => {
            // 自消息防环(硬规则):user_id == self_id 直接丢弃
            let self_id = state.self_id.read().await.clone();
            let user_id = value
                .get("user_id")
                .and_then(serde_json::Value::as_i64)
                .map(|id| id.to_string());
            if let (Some(self_id), Some(user_id)) = (&self_id, &user_id) {
                if self_id == user_id {
                    return;
                }
            }
            if let Some(message) = normalize_message(state, &value, self_id.as_deref()).await {
                let _ = state.event_tx.send(ChannelEvent::Inbound(message)).await;
            }
        }
        _ => {
            // action 应答:echo 字段路由回 oneshot
            if let Some(echo) = value.get("echo").and_then(serde_json::Value::as_str) {
                if let Some(reply_tx) = state.echo_table.lock().await.remove(echo) {
                    let _ = reply_tx.send(value);
                }
            }
        }
    }
}

/// OneBot 11 消息 → InboundMessage。
async fn normalize_message(
    state: &QqState,
    value: &serde_json::Value,
    self_id: Option<&str>,
) -> Option<InboundMessage> {
    let message_type = value.get("message_type").and_then(serde_json::Value::as_str)?;
    let user_id = value
        .get("user_id")
        .and_then(serde_json::Value::as_i64)?
        .to_string();
    let chat_type = match message_type {
        "private" => crate::types::ChatType::Private,
        "group" => crate::types::ChatType::Group,
        _ => return None,
    };
    let conversation_id = match chat_type {
        crate::types::ChatType::Private => user_id.clone(),
        crate::types::ChatType::Group => value
            .get("group_id")
            .and_then(serde_json::Value::as_i64)?
            .to_string(),
    };
    let sender = value.get("sender")?;
    let display_name = sender
        .get("card")
        .and_then(serde_json::Value::as_str)
        .filter(|card| !card.is_empty())
        .or_else(|| sender.get("nickname").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown")
        .to_string();

    // 段归一化(text/at/reply/image/file/face)
    let raw_segments = value
        .get("message")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut segments: Vec<Segment> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let mut at_me = false;
    for raw in &raw_segments {
        let seg_type = raw
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let data = raw.get("data").cloned().unwrap_or(serde_json::json!({}));
        match seg_type {
            "text" => {
                let text = data
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                text_parts.push(text.to_string());
                segments.push(Segment::text(text));
            }
            "at" => {
                let qq = data.get("qq").map(|q| match q {
                    serde_json::Value::Number(number) => number.to_string(),
                    serde_json::Value::String(text) => text.clone(),
                    _ => String::new(),
                });
                if let Some(qq) = qq {
                    if !qq.is_empty() {
                        if let Some(self_id) = self_id {
                            if qq == self_id {
                                at_me = true;
                            }
                        }
                        text_parts.push(format!("@{qq}"));
                        segments.push(Segment::at(qq));
                    }
                }
            }
            "reply" => {
                let id = data
                    .get("id")
                    .map(|id| match id {
                        serde_json::Value::Number(number) => number.to_string(),
                        serde_json::Value::String(text) => text.clone(),
                        _ => String::new(),
                    })
                    .unwrap_or_default();
                if !id.is_empty() {
                    text_parts.push("[回复]".into());
                    segments.push(Segment::Reply { message_id: id });
                }
            }
            "image" => {
                segments.push(Segment::Image {
                    url: data
                        .get("url")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    file_id: data
                        .get("file")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                });
            }
            "file" => {
                segments.push(Segment::File {
                    url: data
                        .get("url")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    file_id: data
                        .get("file")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    name: data
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                });
            }
            "face" => {
                text_parts.push("[表情]".into());
            }
            _ => {}
        }
    }

    // reply_to_me:引用的 id 是否为本 bot 已发送消息
    let reply_to_me = match segments.iter().find_map(|segment| match segment {
        Segment::Reply { message_id } => Some(message_id.clone()),
        _ => None,
    }) {
        Some(reply_id) => state.sent_message_ids.lock().await.contains(&reply_id),
        None => false,
    };
    let to_me = match chat_type {
        crate::types::ChatType::Private => true,
        crate::types::ChatType::Group => at_me || reply_to_me,
    };

    Some(InboundMessage {
        platform: "qq",
        chat: ChatRef {
            platform: "qq",
            chat_type,
            conversation_id,
        },
        sender: Sender {
            user_id,
            display_name,
        },
        message_id: value
            .get("message_id")
            .map(|id| match id {
                serde_json::Value::Number(number) => number.to_string(),
                serde_json::Value::String(text) => text.clone(),
                _ => String::new(),
            })
            .unwrap_or_default(),
        segments,
        text: text_parts.join(""),
        to_me,
        reply_to_me,
        raw: value.clone(),
    })
}

/// 常数时间字符串比较(L1 自包含;与控制面 auth 同语义)。
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = (a.len() ^ b.len()) as u8;
    for index in 0..max {
        diff |= a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_plain_equality() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
        assert!(!constant_time_eq("secret", "secret1"));
        assert!(!constant_time_eq("Bearer x", "Bearer y"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn config_requires_token() {
        let channel = QqChannel::new();
        let error = channel
            .apply_config(&serde_json::json!({ "reverseWsPort": 3001 }))
            .unwrap_err();
        assert!(error.to_string().contains("accessToken 必填"), "{error}");
    }
}
