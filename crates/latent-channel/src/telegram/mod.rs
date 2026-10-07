//! Telegram 渠道(feature `telegram`):Bot API **getUpdates 长轮询**
//! (不引 SDK,reqwest 直连;OpenClaw 的 telegram 同样默认长轮询)。
//!
//! 已核实语义(§6 坑 4):`getUpdates` 带 `timeout` 长轮询;`offset =
//! last_update_id + 1`;两个轮询进程会 409 Conflict(重启竞速时注意);
//! botToken 从 `$ENV` 读(凭据解析在 gateway 层)。发送 `sendMessage`,
//! `reply_to_message_id` 实现 Reply 段;typing 用 `sendChatAction`。
//!
//! `apiBase` 配置默认官方端点,测试指向本地假 Bot API 服务器。

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use crate::error::ChannelError;
use crate::plugin::{
    initial_status, run_command_loop, spawn_guarded, ChannelCommand, ChannelHandle, ChannelPlugin,
    ChannelSender,
};
use crate::types::{
    ChatRef, ChannelEvent, ChannelStatus, InboundMessage, OutboundMessage, Sender, Segment,
};

/// 官方 Bot API 端点(测试经 `apiBase` 覆盖指向本地假服务器)。
pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";
/// getUpdates 长轮询秒数(上游同款默认)。
pub const POLL_TIMEOUT_SECS: u64 = 25;
/// 轮询网络错误后的重试间隔。
const POLL_RETRY_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub api_base: String,
    pub poll_timeout_secs: u64,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        TelegramConfig {
            bot_token: String::new(),
            api_base: DEFAULT_API_BASE.into(),
            poll_timeout_secs: POLL_TIMEOUT_SECS,
        }
    }
}

impl TelegramConfig {
    /// apply_config 的反序列化入口(gateway 注入原始 JSON)。
    pub fn from_json(raw: &serde_json::Value) -> Result<Self, ChannelError> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        struct Raw {
            bot_token: String,
            api_base: String,
            poll_timeout_secs: u64,
        }
        impl Default for Raw {
            fn default() -> Self {
                Raw {
                    bot_token: String::new(),
                    api_base: DEFAULT_API_BASE.into(),
                    poll_timeout_secs: POLL_TIMEOUT_SECS,
                }
            }
        }
        let raw: Raw = serde_json::from_value(raw.clone()).map_err(|error| {
            ChannelError::Config(format!("telegram 配置解析失败: {error}"))
        })?;
        Ok(TelegramConfig {
            bot_token: raw.bot_token,
            api_base: raw.api_base,
            poll_timeout_secs: raw.poll_timeout_secs,
        })
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.bot_token, method)
    }
}

/// 运行期共享状态(bot 身份 + 配置快照)。
struct TelegramState {
    config: TelegramConfig,
    bot_id: String,
    bot_username: Option<String>,
    client: reqwest::Client,
}

pub struct TelegramChannel {
    config: RwLock<Option<TelegramConfig>>,
}

impl TelegramChannel {
    pub fn new() -> Self {
        TelegramChannel {
            config: RwLock::new(None),
        }
    }

    fn take_config(&self) -> Result<TelegramConfig, ChannelError> {
        self.config
            .write()
            .unwrap()
            .take()
            .ok_or_else(|| ChannelError::Config("telegram 渠道未注入配置".into()))
    }
}

impl Default for TelegramChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelPlugin for TelegramChannel {
    fn id(&self) -> &'static str {
        "telegram"
    }

    fn apply_config(&self, raw: &serde_json::Value) -> Result<(), ChannelError> {
        let config = TelegramConfig::from_json(raw)?;
        if config.bot_token.trim().is_empty() {
            return Err(ChannelError::Config(
                "telegram botToken 必填(缺失/为空 → 渠道拒绝启动)".into(),
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
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(
                config.poll_timeout_secs.max(5) + 10,
            ))
            .build()
            .map_err(|error| ChannelError::Startup(error.to_string()))?;
        // getMe:登录态自检 + bot 身份(At 判定/账号 id)
        let me: serde_json::Value = api_get(&client, &config.api_url("getMe")).await?;
        let bot_id = me
            .pointer("/result/id")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| ChannelError::Startup("getMe 响应缺 result.id".into()))?
            .to_string();
        let bot_username = me
            .pointer("/result/username")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let state = Arc::new(TelegramState {
            config: config.clone(),
            bot_id: bot_id.clone(),
            bot_username,
            client,
        });

        let (status_tx, status_rx) = initial_status();
        let _ = status_tx.send(ChannelStatus::Connected {
            account_id: bot_id.clone(),
        });
        // 注意:Connected 先行上报(长轮询循环内错误转 Disconnected)

        // 长轮询任务
        let poll_state = state.clone();
        let poll_tx = tx.clone();
        spawn_guarded("telegram:poll", tx.clone(), async move {
            poll_loop(poll_state, poll_tx, status_tx).await;
        });

        // 出站命令任务
        let (command_tx, command_rx) = mpsc::channel::<ChannelCommand>(64);
        let command_state = state.clone();
        spawn_guarded("telegram:commands", tx, async move {
            let send_state = command_state.clone();
            run_command_loop(
                command_rx,
                move |chat, message, reply| {
                    let state = send_state.clone();
                    async move {
                        let result = send_message(&state, &chat, &message).await;
                        let _ = reply.send(result);
                    }
                },
                move |chat, on| {
                    // typing:sendChatAction;关闭信号 no-op(动作 5s 自过期)
                    if on {
                        let state = command_state.clone();
                        let chat = chat.clone();
                        tokio::spawn(async move {
                            let _ = state
                                .client
                                .post(state.config.api_url("sendChatAction"))
                                .json(&serde_json::json!({
                                    "chat_id": chat.conversation_id,
                                    "action": "typing",
                                }))
                                .send()
                                .await;
                        });
                    }
                },
            )
            .await;
        });

        Ok(ChannelHandle::new(ChannelSender::new(command_tx), status_rx))
    }
}

// ---------------------------------------------------------------------------
// Bot API 交互
// ---------------------------------------------------------------------------

/// GET 调用并解包 `{ok, result}`;ok=false → 分类错误。
async fn api_get(client: &reqwest::Client, url: &str) -> Result<serde_json::Value, ChannelError> {
    let response = client.get(url).send().await.map_err(|error| {
        ChannelError::Startup(format!("telegram 请求失败: {error}"))
    })?;
    parse_api_response(response).await
}

/// Telegram API 响应解包:`{ok: bool, result, description, parameters}`。
async fn parse_api_response(
    response: reqwest::Response,
) -> Result<serde_json::Value, ChannelError> {
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    let ok = body.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false);
    if ok && status.is_success() {
        return Ok(body);
    }
    let description = body
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    // 分类(对齐上游 ReplyMediaFailure 的 code 面)
    if status.as_u16() == 429 {
        let retry_after = body
            .pointer("/parameters/retry_after")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);
        return Err(ChannelError::RateLimited {
            retry_after_ms: retry_after * 1000,
        });
    }
    if status.as_u16() == 401 {
        return Err(ChannelError::Startup(format!(
            "telegram 认证失败(401): {description}"
        )));
    }
    if description.contains("chat not found") {
        return Err(ChannelError::ChatNotFound);
    }
    if description.contains("not a member") || description.contains("kicked") {
        return Err(ChannelError::NotInGroup);
    }
    Err(ChannelError::DeliveryFailed(format!(
        "telegram API 错误({status}): {description}"
    )))
}

/// 长轮询主循环:offset = last_update_id + 1;网络错误退避重试;401 → Failed。
async fn poll_loop(
    state: Arc<TelegramState>,
    tx: mpsc::Sender<ChannelEvent>,
    status_tx: watch::Sender<ChannelStatus>,
) {
    let mut offset: Option<i64> = None;
    let allowed_updates = serde_json::json!(["message"]);
    loop {
        let mut url = format!(
            "{}?timeout={}&allowed_updates={}",
            state.config.api_url("getUpdates"),
            state.config.poll_timeout_secs,
            allowed_updates
        );
        if let Some(offset) = offset {
            url.push_str(&format!("&offset={offset}"));
        }
        match state.client.get(&url).send().await {
            Ok(response) => match parse_api_response(response).await {
                Ok(body) => {
                    let updates = body
                        .get("result")
                        .and_then(serde_json::Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for update in updates {
                        let update_id = update.get("update_id").and_then(serde_json::Value::as_i64);
                        if let Some(update_id) = update_id {
                            offset = Some(update_id + 1);
                        }
                        if let Some(message) = normalize_update(&state, &update) {
                            if tx.send(ChannelEvent::Inbound(message)).await.is_err() {
                                return; // 宿主已停
                            }
                        }
                    }
                }
                // 401 = 凭据失效,需宿主介入
                Err(ChannelError::Startup(reason)) => {
                    let _ = status_tx.send(ChannelStatus::Failed { reason });
                    return;
                }
                Err(_) => {
                    let _ = status_tx.send(ChannelStatus::Disconnected {
                        reason: "API 错误,退避重试".into(),
                    });
                    tokio::time::sleep(POLL_RETRY_DELAY).await;
                }
            },
            Err(_) => {
                let _ = status_tx.send(ChannelStatus::Disconnected {
                    reason: "网络错误,退避重试".into(),
                });
                tokio::time::sleep(POLL_RETRY_DELAY).await;
            }
        }
    }
}

/// update → InboundMessage(无法归一化的 update 返回 None)。
fn normalize_update(state: &TelegramState, update: &serde_json::Value) -> Option<InboundMessage> {
    let message = update.get("message")?;
    let from = message.get("from")?;
    let user_id = from.get("id").and_then(serde_json::Value::as_i64)?.to_string();
    let display_name = from
        .get("first_name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| from.get("username").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown")
        .to_string();
    let chat = message.get("chat")?;
    let chat_id = chat.get("id").and_then(serde_json::Value::as_i64)?.to_string();
    let chat_type = match chat.get("type").and_then(serde_json::Value::as_str) {
        Some("private") => crate::types::ChatType::Private,
        _ => crate::types::ChatType::Group,
    };
    let text = message
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut segments = vec![Segment::text(text.clone())];
    // At 判定:entities mention 命中 bot username
    let mut to_me = chat_type == crate::types::ChatType::Private;
    if let Some(entities) = message.get("entities").and_then(serde_json::Value::as_array) {
        for entity in entities {
            let is_mention = entity.get("type").and_then(serde_json::Value::as_str) == Some("mention");
            if !is_mention {
                continue;
            }
            let offset = entity.get("offset").and_then(serde_json::Value::as_u64)? as usize;
            let length = entity.get("length").and_then(serde_json::Value::as_u64)? as usize;
            let mention = text.get(offset..offset + length).unwrap_or("");
            if let Some(username) = &state.bot_username {
                if mention.eq_ignore_ascii_case(&format!("@{username}")) {
                    segments.push(Segment::at(&state.bot_id));
                    to_me = true;
                }
            }
        }
    }
    // Reply 判定:引用的消息来自 bot
    let reply_to_me = message
        .pointer("/reply_to_message/from/id")
        .and_then(serde_json::Value::as_i64)
        .map(|id| id.to_string() == state.bot_id)
        .unwrap_or(false);
    if reply_to_me {
        to_me = true;
        segments.insert(0, Segment::Reply {
            message_id: message
                .pointer("/reply_to_message/message_id")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
                .to_string(),
        });
    }

    Some(InboundMessage {
        platform: "telegram",
        chat: ChatRef {
            platform: "telegram",
            chat_type,
            conversation_id: chat_id,
        },
        sender: Sender {
            user_id,
            display_name,
        },
        // 去重键用 update_id 推导(§2.3)
        message_id: format!(
            "tg-{}",
            update.get("update_id").and_then(serde_json::Value::as_i64).unwrap_or(0)
        ),
        segments,
        text,
        to_me,
        reply_to_me,
        raw: update.clone(),
    })
}

/// sendMessage(reply 段 → reply_to_message_id)。
async fn send_message(
    state: &TelegramState,
    chat: &ChatRef,
    message: &OutboundMessage,
) -> Result<(), ChannelError> {
    let text: String = message
        .segments
        .iter()
        .filter_map(|segment| match segment {
            Segment::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let mut params = serde_json::json!({
        "chat_id": chat.conversation_id,
        "text": text,
    });
    if let Some(reply_to) = &message.reply_to {
        params["reply_to_message_id"] = serde_json::json!(reply_to);
    }
    let response = state
        .client
        .post(state.config.api_url("sendMessage"))
        .json(&params)
        .send()
        .await
        .map_err(|error| ChannelError::DeliveryFailed(format!("sendMessage 网络错误: {error}")))?;
    parse_api_response(response).await.map(|_| ())
}
