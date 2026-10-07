//! telegram 渠道集成测试:本地假 Bot API 服务器(getUpdates 轮询 /
//! sendMessage / sendChatAction / getMe),全离线。

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use latent_channel::plugin::ChannelPlugin;
use latent_channel::types::{ChatRef, ChannelEvent, OutboundMessage};

#[derive(Default)]
struct FakeBotState {
    /// getUpdates 队列(每调用取走一批)
    updates: Mutex<Vec<serde_json::Value>>,
    /// sendMessage 请求体记录
    sent: Mutex<Vec<serde_json::Value>>,
    /// sendChatAction 请求体记录
    chat_actions: Mutex<Vec<serde_json::Value>>,
}

async fn get_me() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "result": { "id": 42, "username": "latent_bot" }
    }))
}

async fn get_updates(
    State(state): State<Arc<FakeBotState>>,
    Path(_token): Path<String>,
) -> Json<serde_json::Value> {
    let updates = state.updates.lock().unwrap().drain(..).collect::<Vec<_>>();
    Json(serde_json::json!({ "ok": true, "result": updates }))
}

async fn send_message(
    State(state): State<Arc<FakeBotState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    state.sent.lock().unwrap().push(body);
    Json(serde_json::json!({
        "ok": true,
        "result": { "message_id": 777 }
    }))
}

async fn send_chat_action(
    State(state): State<Arc<FakeBotState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    state.chat_actions.lock().unwrap().push(body);
    Json(serde_json::json!({ "ok": true, "result": true }))
}

async fn spawn_fake_bot(state: Arc<FakeBotState>) -> String {
    let app = Router::new()
        .fallback(|uri: axum::http::Uri| async move {
            eprintln!("[fake-bot] 404 未匹配: {uri}");
            axum::http::StatusCode::NOT_FOUND
        })
        .route("/bot{token}/getMe", get(get_me))
        .route("/bot{token}/getUpdates", get(get_updates))
        .route("/bot{token}/sendMessage", post(send_message))
        .route("/bot{token}/sendChatAction", post(send_chat_action))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn group_update(update_id: i64, text: &str, mention: bool) -> serde_json::Value {
    let mut message = serde_json::json!({
        "message_id": update_id,
        "from": { "id": 1001, "first_name": "张三" },
        "chat": { "id": 12345, "type": "group" },
        "text": text,
    });
    if mention {
        message["entities"] = serde_json::json!([
            { "type": "mention", "offset": 0, "length": 11 }
        ]);
    }
    serde_json::json!({ "update_id": update_id, "message": message })
}

async fn next_inbound(
    rx: &mut tokio::sync::mpsc::Receiver<ChannelEvent>,
) -> latent_channel::types::InboundMessage {
    for _ in 0..500 {
        if let Ok(ChannelEvent::Inbound(message)) = rx.try_recv() {
            return message;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("等待 Inbound 事件超时");
}

#[tokio::test]
async fn normalizes_updates_and_sends_messages() {
    let state = Arc::new(FakeBotState::default());
    let api_base = spawn_fake_bot(state.clone()).await;

    let channel = latent_channel::telegram::TelegramChannel::new();
    channel
        .apply_config(&serde_json::json!({
            "botToken": "test-token",
            "apiBase": api_base,
            "pollTimeoutSecs": 0,
        }))
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let handle = ChannelPlugin::start(&channel, tx).await.unwrap();

    // 状态:Connected,account_id = bot id(getMe 自检)
    for _ in 0..100 {
        if matches!(
            handle.status().await,
            latent_channel::types::ChannelStatus::Connected { .. }
        ) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        handle.status().await,
        latent_channel::types::ChannelStatus::Connected {
            account_id: "42".into()
        }
    );

    // 注入 group 更新:@latent_bot 开头 → At 段 + to_me
    state
        .updates
        .lock()
        .unwrap()
        .push(group_update(101, "@latent_bot 帮我看看", true));
    let message = next_inbound(&mut rx).await;
    assert_eq!(message.message_id, "tg-101");
    assert!(message.to_me);
    assert!(message.has_at_segment());
    assert_eq!(message.chat.conversation_id, "12345");
    assert_eq!(message.sender.display_name, "张三");

    // 注入普通 group 消息(无 @)→ to_me=false
    state
        .updates
        .lock()
        .unwrap()
        .push(group_update(102, "普通消息", false));
    let message = next_inbound(&mut rx).await;
    assert!(!message.to_me);

    // 注入 reply 机器人消息 → reply_to_me
    let reply_update = serde_json::json!({
        "update_id": 103,
        "message": {
            "message_id": 103,
            "from": { "id": 1002, "first_name": "李四" },
            "chat": { "id": 12345, "type": "group" },
            "text": "收到",
            "reply_to_message": {
                "message_id": 777,
                "from": { "id": 42 }
            }
        }
    });
    state.updates.lock().unwrap().push(reply_update);
    let message = next_inbound(&mut rx).await;
    assert!(message.reply_to_me);
    assert!(message.to_me);

    // 私聊 → 恒 to_me
    state
        .updates
        .lock()
        .unwrap()
        .push(serde_json::json!({
            "update_id": 104,
            "message": {
                "message_id": 104,
                "from": { "id": 1003, "first_name": "王五" },
                "chat": { "id": 1003, "type": "private" },
                "text": "你好",
            }
        }));
    let message = next_inbound(&mut rx).await;
    assert!(message.to_me);

    // 出站:sendMessage 记录 chat_id + text
    let chat = ChatRef::group("telegram", "12345");
    handle
        .send(&chat, OutboundMessage::text("回复内容"))
        .await
        .unwrap();
    for _ in 0..100 {
        if !state.sent.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let sent = state.sent.lock().unwrap();
    assert_eq!(sent[0]["chat_id"], "12345");
    assert_eq!(sent[0]["text"], "回复内容");
}

#[tokio::test]
async fn typing_uses_send_chat_action() {
    let state = Arc::new(FakeBotState::default());
    let api_base = spawn_fake_bot(state.clone()).await;
    let channel = latent_channel::telegram::TelegramChannel::new();
    channel
        .apply_config(&serde_json::json!({
            "botToken": "test-token",
            "apiBase": api_base,
            "pollTimeoutSecs": 0,
        }))
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let handle = ChannelPlugin::start(&channel, tx).await.unwrap();

    // typing 指示器(sendChatAction 被记录)
    let chat = ChatRef::private("telegram", "1003");
    handle.typing(&chat, true).await;
    for _ in 0..100 {
        if !state.chat_actions.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let actions = state.chat_actions.lock().unwrap();
    assert_eq!(actions[0]["chat_id"], "1003");
    assert_eq!(actions[0]["action"], "typing");
}

#[tokio::test]
async fn missing_token_refuses_to_start() {
    let channel = latent_channel::telegram::TelegramChannel::new();
    let error = channel
        .apply_config(&serde_json::json!({ "botToken": "" }))
        .unwrap_err();
    assert!(error.to_string().contains("botToken 必填"), "{error}");
}

/// P1-5:Bot API 实体 offset/length 是 UTF-16 code units —— mention 前
/// 有中文/emoji 时按字节切片会错位,群里中文用户 @ 机器人必须仍判 to_me。
#[tokio::test]
async fn cjk_prefixed_mention_still_detects_to_me() {
    let state = Arc::new(FakeBotState::default());
    let api_base = spawn_fake_bot(state.clone()).await;
    let channel = latent_channel::telegram::TelegramChannel::new();
    channel
        .apply_config(&serde_json::json!({
            "botToken": "test-token", "apiBase": api_base, "pollTimeoutSecs": 0,
        }))
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    ChannelPlugin::start(&channel, tx).await.unwrap();

    // "你好 @latent_bot 帮忙":mention 的 UTF-16 offset = 3(你好=2 + 空格=1)
    state
        .updates
        .lock()
        .unwrap()
        .push(serde_json::json!({
            "update_id": 201,
            "message": {
                "message_id": 201,
                "from": { "id": 1001, "first_name": "张三" },
                "chat": { "id": 12345, "type": "group" },
                "text": "你好 @latent_bot 帮忙",
                "entities": [ { "type": "mention", "offset": 3, "length": 11 } ]
            }
        }));
    let message = next_inbound(&mut rx).await;
    assert!(message.to_me, "CJK 前缀的 @ 必须按 UTF-16 偏移命中");
    assert!(message.has_at_segment());
}

/// P1-5:单个畸形实体(缺 offset/length)不得让整条消息归一化失败被丢弃。
#[tokio::test]
async fn malformed_entity_does_not_drop_message() {
    let state = Arc::new(FakeBotState::default());
    let api_base = spawn_fake_bot(state.clone()).await;
    let channel = latent_channel::telegram::TelegramChannel::new();
    channel
        .apply_config(&serde_json::json!({
            "botToken": "test-token", "apiBase": api_base, "pollTimeoutSecs": 0,
        }))
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    ChannelPlugin::start(&channel, tx).await.unwrap();

    state
        .updates
        .lock()
        .unwrap()
        .push(serde_json::json!({
            "update_id": 202,
            "message": {
                "message_id": 202,
                "from": { "id": 1002, "first_name": "李四" },
                "chat": { "id": 12345, "type": "group" },
                "text": "普通文本",
                "entities": [ { "type": "mention" } ]
            }
        }));
    let message = next_inbound(&mut rx).await;
    assert_eq!(message.text, "普通文本", "畸形实体只跳过实体,不丢消息");
    assert!(!message.to_me);
}
