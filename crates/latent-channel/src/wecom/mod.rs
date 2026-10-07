//! 企业微信智能机器人渠道(feature `wecom`):**WS 长连接**(官方文档
//! `developer.work.weixin.qq.com/document/path/101463`,已核对):
//!
//! - 连接 `wss://openws.work.weixin.qq.com` → 发送 `aibot_subscribe`
//!   (bot_id + secret)→ 校验通过开始接收回调;
//! - 消息回调 `cmd=aibot_msg_callback`:body 含 `msgid`/`aibotid`/`chatid`/
//!   `chattype`(single|group)/`from.userid`/`msgtype`;
//! - 回复:`aibot_respond_msg`(透传回调 req_id)或主动推送
//!   `aibot_send_msg`(指定 chatid/chat_type,须用户先给机器人发过消息)——
//!   gateway 出站统一走 `aibot_send_msg`;
//! - 心跳:每 30s 一次 `ping`,超时服务端断开;
//! - 单机器人同时仅一条长连接,新连接踢旧连接;断线自动重连。
//!
//! 频率限制:单会话 30 条/分钟(限速由平台侧 4xx 表达,通道层透传分类)。

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::MaybeTlsStream;

use crate::error::ChannelError;
use crate::plugin::{
    ChannelCommand, ChannelHandle, ChannelPlugin, ChannelSender, spawn_guarded,
};
use crate::types::{
    ChatRef, ChannelEvent, ChannelStatus, InboundMessage, OutboundMessage, Sender, Segment,
};

/// 官方长连接端点(测试经 `wsEndpoint` 覆盖指向本地假服务器)。
pub const DEFAULT_WS_ENDPOINT: &str = "wss://openws.work.weixin.qq.com";
/// 心跳间隔(官方建议 30s)。
pub const HEARTBEAT_INTERVAL_SECS: u64 = 30;
/// 断线重连退避。
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct WecomConfig {
    pub bot_id: String,
    pub secret: String,
    pub ws_endpoint: String,
    pub heartbeat_interval_secs: u64,
}

impl Default for WecomConfig {
    fn default() -> Self {
        WecomConfig {
            bot_id: String::new(),
            secret: String::new(),
            ws_endpoint: DEFAULT_WS_ENDPOINT.into(),
            heartbeat_interval_secs: HEARTBEAT_INTERVAL_SECS,
        }
    }
}

impl WecomConfig {
    pub fn from_json(raw: &serde_json::Value) -> Result<Self, ChannelError> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        struct Raw {
            bot_id: String,
            secret: String,
            ws_endpoint: String,
            heartbeat_interval_secs: u64,
        }
        impl Default for Raw {
            fn default() -> Self {
                Raw {
                    bot_id: String::new(),
                    secret: String::new(),
                    ws_endpoint: DEFAULT_WS_ENDPOINT.into(),
                    heartbeat_interval_secs: HEARTBEAT_INTERVAL_SECS,
                }
            }
        }
        let raw: Raw = serde_json::from_value(raw.clone())
            .map_err(|error| ChannelError::Config(format!("wecom 配置解析失败: {error}")))?;
        Ok(WecomConfig {
            bot_id: raw.bot_id,
            secret: raw.secret,
            ws_endpoint: raw.ws_endpoint,
            heartbeat_interval_secs: raw.heartbeat_interval_secs,
        })
    }
}

pub struct WecomChannel {
    config: RwLock<Option<WecomConfig>>,
}

impl WecomChannel {
    pub fn new() -> Self {
        WecomChannel {
            config: RwLock::new(None),
        }
    }

    fn take_config(&self) -> Result<WecomConfig, ChannelError> {
        self.config
            .write()
            .unwrap()
            .take()
            .ok_or_else(|| ChannelError::Config("wecom 渠道未注入配置".into()))
    }
}

impl Default for WecomChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// 共享运行态(连接出站口;断线重连后替换)。
struct WecomState {
    config: WecomConfig,
    connection: tokio::sync::RwLock<Option<mpsc::Sender<WsMessage>>>,
    status_tx: watch::Sender<ChannelStatus>,
    event_tx: mpsc::Sender<ChannelEvent>,
}

impl WecomState {
    /// 主动推送 `aibot_send_msg`(gateway 出站统一入口)。
    async fn send_message(
        &self,
        chat: &ChatRef,
        message: &OutboundMessage,
    ) -> Result<(), ChannelError> {
        let connection = self.connection.read().await.clone();
        let Some(sender) = connection else {
            return Err(ChannelError::DeliveryFailed("wecom 长连接未建立".into()));
        };
        let text: String = message
            .segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let chat_type = match chat.chat_type {
            crate::types::ChatType::Private => "single",
            crate::types::ChatType::Group => "group",
        };
        let frame = serde_json::json!({
            "cmd": "aibot_send_msg",
            "headers": { "req_id": uuid_like_req_id() },
            "body": {
                "chatid": chat.conversation_id,
                "chattype": chat_type,
                "msgtype": "text",
                "text": { "content": text },
            },
        });
        sender
            .send(WsMessage::text(frame.to_string()))
            .await
            .map_err(|_| ChannelError::DeliveryFailed("wecom 连接已断开".into()))
    }
}

#[async_trait]
impl ChannelPlugin for WecomChannel {
    fn id(&self) -> &'static str {
        "wecom"
    }

    fn apply_config(&self, raw: &serde_json::Value) -> Result<(), ChannelError> {
        let config = WecomConfig::from_json(raw)?;
        if config.bot_id.trim().is_empty() || config.secret.trim().is_empty() {
            return Err(ChannelError::Config(
                "wecom botId/secret 必填(缺失 → 该渠道拒绝启动)".into(),
            ));
        }
        *self.config.write().unwrap() = Some(config);
        Ok(())
    }

    async fn start(
        &self,
        tx: mpsc::Sender<ChannelEvent>,
    ) -> Result<ChannelHandle, ChannelError> {
        let config = self.take_config()?;
        let (status_tx, status_rx) = watch::channel(ChannelStatus::Disconnected {
            reason: "connecting".into(),
        });
        let state = Arc::new(WecomState {
            config: config.clone(),
            connection: tokio::sync::RwLock::new(None),
            status_tx: status_tx.clone(),
            event_tx: tx.clone(),
        });

        // 连接管理任务:订阅 → 接收循环;断线退避重连;订阅被拒 → Failed
        let connect_state = state.clone();
        spawn_guarded("wecom:connect", tx.clone(), async move {
            connect_loop(connect_state).await;
        });

        // 出站命令任务
        let command_state = state.clone();
        let (command_tx, mut command_rx) = mpsc::channel::<ChannelCommand>(64);
        spawn_guarded("wecom:commands", tx, async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    ChannelCommand::Send {
                        chat,
                        message,
                        reply,
                    } => {
                        let result = command_state.send_message(&chat, &message).await;
                        let _ = reply.send(result);
                    }
                    ChannelCommand::Typing { .. } => {
                        // 企微智能机器人无 typing 动作:静默抑制
                    }
                    ChannelCommand::Shutdown => break,
                }
            }
        });

        Ok(ChannelHandle::new(ChannelSender::new(command_tx), status_rx))
    }
}

/// 连接管理:subscribe → 事件循环 → 断线重连(订阅被拒 = Failed,终止)。
async fn connect_loop(state: Arc<WecomState>) {
    loop {
        match establish(&state).await {
            Ok(()) => {
                // 正常退出(Shutdown/宿主停)—— connect_loop 由接收循环返回控制
                let _ = state.status_tx.send(ChannelStatus::Disconnected {
                    reason: "连接关闭".into(),
                });
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            Err(ChannelError::Startup(reason)) => {
                // 订阅被拒(凭据失效):需宿主介入,不再重试
                let _ = state.status_tx.send(ChannelStatus::Failed { reason });
                return;
            }
            Err(reason) => {
                let _ = state.status_tx.send(ChannelStatus::Disconnected {
                    reason: reason.to_string(),
                });
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

/// 单次连接生命周期:握手 + 订阅 + 心跳 + 接收循环。
async fn establish(state: &Arc<WecomState>) -> Result<(), ChannelError> {
    let (ws, _) = tokio_tungstenite::connect_async(&state.config.ws_endpoint)
        .await
        .map_err(|error| ChannelError::Startup(format!("wemos 连接失败: {error}")))?;
    let (mut sender, mut receiver) = ws.split();

    // aibot_subscribe
    let req_id = uuid_like_req_id();
    let subscribe = serde_json::json!({
        "cmd": "aibot_subscribe",
        "headers": { "req_id": req_id },
        "body": {
            "bot_id": state.config.bot_id,
            "secret": state.config.secret,
        },
    });
    sender
        .send(WsMessage::text(subscribe.to_string()))
        .await
        .map_err(|error| ChannelError::Startup(format!("订阅发送失败: {error}")))?;

    // 等订阅应答(带超时;errcode 非 0 = 凭据失效 → Startup 错误)
    let ack = tokio::time::timeout(Duration::from_secs(15), receiver.next())
        .await
        .map_err(|_| ChannelError::Startup("订阅应答超时".into()))?
        .ok_or_else(|| ChannelError::Startup("连接在订阅应答前关闭".into()))?
        .map_err(|error| ChannelError::Startup(format!("订阅应答读取失败: {error}")))?;
    let ack_text = match ack {
        WsMessage::Text(text) => text.to_string(),
        _ => return Err(ChannelError::Startup("订阅应答帧异常".into())),
    };
    let ack_value: serde_json::Value = serde_json::from_str(&ack_text)
        .map_err(|error| ChannelError::Startup(format!("订阅应答解析失败: {error}")))?;
    let errcode = ack_value
        .pointer("/body/errcode")
        .or_else(|| ack_value.get("errcode"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    if errcode != 0 {
        return Err(ChannelError::Startup(format!(
            "wecom 订阅被拒(errcode {errcode}):botId/secret 无效?"
        )));
    }

    // 连接出站口注册(心跳 + 应用层发送都走它);writer 任务把出站帧转发到
    // WS —— 缺了它,出站帧只会堵在 mpsc 里(曾致出站静默丢失)
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<WsMessage>(64);
    *state.connection.write().await = Some(outbound_tx.clone());
    let writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });
    let _ = state.status_tx.send(ChannelStatus::Connected {
        account_id: state.config.bot_id.clone(),
    });
    let _ = state
        .event_tx
        .send(ChannelEvent::Status(ChannelStatus::Connected {
            account_id: state.config.bot_id.clone(),
        }))
        .await;

    // 心跳任务(30s ping)
    let heartbeat_interval =
        Duration::from_secs(state.config.heartbeat_interval_secs.max(1));
    let heartbeat = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(heartbeat_interval);
        loop {
            ticker.tick().await;
            let ping = serde_json::json!({ "cmd": "ping" });
            if outbound_tx
                .send(WsMessage::text(ping.to_string()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 接收循环
    let mut result: Result<(), ChannelError> = Ok(());
    while let Some(message) = receiver.next().await {
        let Ok(message) = message else {
            result = Err(ChannelError::DeliveryFailed("wecom 连接读取失败".into()));
            break;
        };
        let text = match message {
            WsMessage::Text(text) => text.to_string(),
            WsMessage::Close(_) => break,
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        match value.get("cmd").and_then(serde_json::Value::as_str) {
            Some("aibot_msg_callback") => {
                if let Some(message) = normalize_callback(&state.config, &value) {
                    if state
                        .event_tx
                        .send(ChannelEvent::Inbound(message))
                        .await
                        .is_err()
                    {
                        break; // 宿主已停
                    }
                }
            }
            Some("aibot_event_callback") => {
                // enter_chat / disconnected_event 等:MVP 仅诊断
                eprintln!(
                    "[latent-channel:wecom] 事件回调: {}",
                    value.pointer("/body/eventtype").and_then(serde_json::Value::as_str).unwrap_or("unknown")
                );
            }
            _ => {}
        }
    }
    heartbeat.abort();
    writer.abort();
    // 清理连接注册
    *state.connection.write().await = None;
    result
}

/// 回调 → InboundMessage(single 恒 to_me;group 需 @bot 文本命中)。
fn normalize_callback(config: &WecomConfig, value: &serde_json::Value) -> Option<InboundMessage> {
    let body = value.get("body")?;
    let chat_type = match body.get("chattype").and_then(serde_json::Value::as_str)? {
        "single" => crate::types::ChatType::Private,
        "group" => crate::types::ChatType::Group,
        _ => return None,
    };
    let conversation_id = body
        .get("chatid")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let user_id = body
        .pointer("/from/userid")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let display_name = body
        .pointer("/from/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&user_id)
        .to_string();
    let msgtype = body.get("msgtype").and_then(serde_json::Value::as_str).unwrap_or("");
    let text = match msgtype {
        "text" => body
            .pointer("/text/content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        "markdown" => body
            .pointer("/markdown/content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        "image" | "file" | "video" | "voice" => String::new(),
        _ => String::new(),
    };

    let mut segments: Vec<Segment> = Vec::new();
    if !text.is_empty() {
        segments.push(Segment::text(text.clone()));
    }
    match msgtype {
        "image" => segments.push(Segment::Image {
            url: body.pointer("/image/url").and_then(serde_json::Value::as_str).map(str::to_string),
            file_id: body.pointer("/image/aeskey").and_then(serde_json::Value::as_str).map(str::to_string),
        }),
        "file" => segments.push(Segment::File {
            url: body.pointer("/file/url").and_then(serde_json::Value::as_str).map(str::to_string),
            file_id: body.pointer("/file/aeskey").and_then(serde_json::Value::as_str).map(str::to_string),
            name: body.pointer("/file/filename").and_then(serde_json::Value::as_str).map(str::to_string),
        }),
        _ => {}
    }

    // to_me:single 恒真;group 需文本命中 @botid(企微无显式 At 段结构)
    let to_me = match chat_type {
        crate::types::ChatType::Private => true,
        crate::types::ChatType::Group => {
            text.contains(&format!("@{}", config.bot_id))
        }
    };

    Some(InboundMessage {
        platform: "wecom",
        chat: ChatRef {
            platform: "wecom",
            chat_type,
            conversation_id,
        },
        sender: Sender {
            user_id,
            display_name,
        },
        message_id: body
            .get("msgid")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        segments,
        text,
        to_me,
        reply_to_me: false,
        raw: value.clone(),
    })
}

/// 轻量 req_id(时间戳 + 计数;协议只要求唯一性)。
fn uuid_like_req_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("latent-{now}-{count}")
}

/// 测试与诊断用:类型别名收敛(tungstenite 流形态)。
pub type WecomStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<TcpStream>>;
