//! qq 渠道集成测试:假 NapCat(OneBot 11 WS 客户端)连本渠道的反向 WS
//! 服务端 —— 自消息防环、At 判定、echo 关联、Bearer 认证与冷却,全离线。

use futures::{SinkExt, StreamExt};
use latent_channel::plugin::ChannelPlugin;
use latent_channel::qq::QqChannel;
use latent_channel::types::{ChatRef, ChannelEvent, OutboundMessage};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 起一个 qq 渠道(端口 0 = 内核分配),返回 (channel, handle, rx)。
async fn spawn_channel() -> (
    QqChannel,
    latent_channel::plugin::ChannelHandle,
    tokio::sync::mpsc::Receiver<ChannelEvent>,
) {
    let channel = QqChannel::new();
    channel
        .apply_config(&serde_json::json!({
            "reverseWsHost": "127.0.0.1",
            "reverseWsPort": 0,
            "accessToken": "napcat-token",
        }))
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let handle = ChannelPlugin::start(&channel, tx).await.unwrap();
    (channel, handle, rx)
}

/// 假 NapCat 连接(带 Bearer 认证;返回已握手的 ws)。
async fn connect_napcat(
    addr: std::net::SocketAddr,
    token: Option<&str>,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
    if let Some(token) = token {
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().unwrap(),
        );
    }
    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws
}

async fn recv_json(ws: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin)) -> serde_json::Value {
    for _ in 0..500 {
        match tokio::time::timeout(std::time::Duration::from_millis(20), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                return serde_json::from_str(&text).unwrap();
            }
            Ok(Some(_)) => continue,
            _ => {}
        }
    }
    panic!("等待 WS 帧超时");
}

async fn next_inbound(rx: &mut tokio::sync::mpsc::Receiver<ChannelEvent>) -> latent_channel::types::InboundMessage {
    for _ in 0..500 {
        if let Ok(ChannelEvent::Inbound(message)) = rx.try_recv() {
            return message;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("等待 Inbound 事件超时");
}

#[tokio::test]
async fn napcat_roundtrip_with_anti_echo() {
    let (channel, handle, mut rx) = spawn_channel().await;
    let addr = channel.bound_addr().expect("反向 WS 已绑定");
    let mut napcat = connect_napcat(addr, Some("napcat-token")).await;

    // lifecycle connect → Connected { account_id: self_id }
    napcat
        .send(WsMessage::text(
            serde_json::json!({
                "post_type": "meta_event",
                "meta_event_type": "lifecycle",
                "sub_type": "connect",
                "self_id": 10000,
            })
            .to_string(),
        ))
        .await
        .unwrap();
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
            account_id: "10000".into()
        }
    );

    // 自消息(机器人自己发言 user_id == self_id)→ 渠道源头丢弃(防环)
    napcat
        .send(WsMessage::text(
            serde_json::json!({
                "post_type": "message",
                "message_type": "private",
                "self_id": 10000,
                "user_id": 10000,
                "message_id": 1,
                "sender": { "user_id": 10000, "nickname": "bot" },
                "message": [{ "type": "text", "data": { "text": "我说的话" } }],
            })
            .to_string(),
        ))
        .await
        .unwrap();

    // 群消息 @机器人 → to_me
    napcat
        .send(WsMessage::text(
            serde_json::json!({
                "post_type": "message",
                "message_type": "group",
                "self_id": 10000,
                "user_id": 2001,
                "group_id": 12345,
                "message_id": 2,
                "sender": { "user_id": 2001, "card": "张三" },
                "message": [
                    { "type": "text", "data": { "text": "帮我看看" } },
                    { "type": "at", "data": { "qq": 10000 } },
                ],
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let message = next_inbound(&mut rx).await;
    assert_eq!(message.message_id, "2", "自消息应被丢弃,这是第一条入站");
    assert_eq!(message.chat.conversation_id, "12345");
    assert_eq!(message.sender.display_name, "张三");
    assert!(message.to_me, "At 机器人 → to_me");
    assert!(message.text.contains("帮我看看"));
    assert!(message.has_at_segment());

    // 私聊(非自消息)→ 恒 to_me
    napcat
        .send(WsMessage::text(
            serde_json::json!({
                "post_type": "message",
                "message_type": "private",
                "self_id": 10000,
                "user_id": 3001,
                "message_id": 3,
                "sender": { "user_id": 3001, "nickname": "李四" },
                "message": [{ "type": "text", "data": { "text": "你好" } }],
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let message = next_inbound(&mut rx).await;
    assert_eq!(message.message_id, "3");
    assert!(message.to_me);

    // 出站:send_group_msg action + echo 关联
    let chat = ChatRef::group("qq", "12345");
    let send_handle = handle.clone();
    let send_task = tokio::spawn(async move {
        send_handle
            .send(&chat, OutboundMessage::text("回复内容"))
            .await
    });
    let action = recv_json(&mut napcat).await;
    assert_eq!(action["action"], "send_group_msg");
    assert_eq!(action["params"]["group_id"], 12345);
    assert_eq!(
        action["params"]["message"][0]["type"], "text",
        "段应转 OneBot message 数组"
    );
    let echo = action["echo"].as_str().unwrap().to_string();
    napcat
        .send(WsMessage::text(
            serde_json::json!({
                "status": "ok",
                "retcode": 0,
                "data": { "message_id": 999 },
                "echo": echo,
            })
            .to_string(),
        ))
        .await
        .unwrap();
    send_task.await.unwrap().unwrap();

    // reply 机器人已发送消息(999)→ reply_to_me
    napcat
        .send(WsMessage::text(
            serde_json::json!({
                "post_type": "message",
                "message_type": "group",
                "self_id": 10000,
                "user_id": 2001,
                "group_id": 12345,
                "message_id": 4,
                "sender": { "user_id": 2001, "card": "张三" },
                "message": [
                    { "type": "reply", "data": { "id": 999 } },
                    { "type": "text", "data": { "text": "收到" } },
                ],
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let message = next_inbound(&mut rx).await;
    assert!(message.reply_to_me, "引用 bot 已发消息 → reply_to_me");
    assert!(message.to_me);
}

#[tokio::test]
async fn bad_token_is_rejected() {
    let (channel, _handle, _rx) = spawn_channel().await;
    let addr = channel.bound_addr().expect("反向 WS 已绑定");
    let result = tokio_tungstenite::connect_async(with_token(addr, "wrong-token")).await;
    assert!(
        result.is_err(),
        "错误 token 的 NapCat 连接应被 HTTP 层拒绝"
    );
    // 正确 token 仍可连接(未进入冷却)
    let ws = connect_napcat(addr, Some("napcat-token")).await;
    drop(ws);
}

fn with_token(addr: std::net::SocketAddr, token: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    request
}

/// P1-7:非法 path(不以 / 开头、或与兜底路由 / 重复注册)必须返回
/// Config 错误拒绝启动,而不是 axum route panic。
#[tokio::test]
async fn invalid_path_is_rejected_not_panic() {
    let channel = QqChannel::new();
    let error = channel
        .apply_config(&serde_json::json!({
            "reverseWsHost": "127.0.0.1", "reverseWsPort": 0,
            "accessToken": "t", "path": "ws",
        }))
        .unwrap_err();
    assert!(
        matches!(error, latent_channel::error::ChannelError::Config(_)),
        "{error}"
    );
    // path == "/" 与兜底 .route("/") 重复注册(曾 "Overlapping method route" panic)
    let channel = QqChannel::new();
    let error = channel
        .apply_config(&serde_json::json!({
            "reverseWsHost": "127.0.0.1", "reverseWsPort": 0,
            "accessToken": "t", "path": "/",
        }))
        .unwrap_err();
    assert!(
        matches!(error, latent_channel::error::ChannelError::Config(_)),
        "{error}"
    );
}

/// P1-1:auth 失败冷却按对端 IP 计键 —— 每次重连源端口都变,按
/// SocketAddr 计键时"连续 5 次失败 → 60s 冷却"永不生效。
#[tokio::test]
async fn auth_cooldown_is_keyed_by_ip_not_ephemeral_port() {
    let (channel, _handle, _rx) = spawn_channel().await;
    let addr = channel.bound_addr().expect("反向 WS 已绑定");
    // 同 IP(源端口每次 TCP 重连都不同)连续 5 次错 token
    for _ in 0..5 {
        let result = tokio_tungstenite::connect_async(with_token(addr, "wrong-token")).await;
        assert!(result.is_err());
    }
    // 第 6 次:换源端口后仍应被冷却拒绝
    let result = tokio_tungstenite::connect_async(with_token(addr, "wrong-token")).await;
    assert!(result.is_err(), "第 6 次连接应被冷却拒绝");
    // 冷却期内即使 token 正确也拒绝(按 IP 生效)
    let result = tokio_tungstenite::connect_async(with_token(addr, "napcat-token")).await;
    assert!(result.is_err(), "冷却期内正确 token 也应被拒绝(P1-1)");
}
