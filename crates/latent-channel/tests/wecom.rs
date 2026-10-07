//! wecom 渠道集成测试:假企业微信智能机器人长连接服务端
//! (aibot_subscribe 握手 / aibot_msg_callback 回调 / aibot_send_msg 推送),
//! 协议帧对齐官方文档(`developer.work.weixin.qq.com/document/path/101463`)。

use futures::{SinkExt, StreamExt};
use latent_channel::plugin::ChannelPlugin;
use latent_channel::types::{ChatRef, ChannelEvent, ChannelStatus, OutboundMessage};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 起监听(不 accept —— accept 必须在渠道客户端连接之后,否则握手死锁)。
async fn spawn_wecom_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, format!("ws://{addr}"))
}

/// accept + WS 握手(渠道 start 之后调用)。
async fn accept_wecom(
    listener: TcpListener,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let (stream, _) = listener.accept().await.unwrap();
    tokio_tungstenite::accept_async(stream)
        .await
        .expect("WS 握手失败")
}

#[tokio::test]
async fn subscribe_then_receive_callback_and_send() {
    let (listener, endpoint) = spawn_wecom_listener().await;

    let channel = latent_channel::wecom::WecomChannel::new();
    channel
        .apply_config(&serde_json::json!({
            "botId": "AIBOT-1",
            "secret": "test-secret",
            "wsEndpoint": endpoint,
            "heartbeatIntervalSecs": 1,
        }))
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let handle = ChannelPlugin::start(&channel, tx).await.unwrap();
    let mut server = accept_wecom(listener).await;

    // 渠道发订阅帧
    let subscribe = loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(3), server.next())
            .await
            .expect("等待订阅帧超时")
            .expect("连接关闭")
            .unwrap();
        if let WsMessage::Text(text) = message {
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value["cmd"] == "aibot_subscribe" {
                break value;
            }
        }
    };
    assert_eq!(subscribe["body"]["bot_id"], "AIBOT-1");
    assert_eq!(subscribe["body"]["secret"], "test-secret");

    // 应答订阅(errcode 0)→ Connected
    server
        .send(WsMessage::text(
            serde_json::json!({
                "cmd": "aibot_subscribe",
                "headers": { "req_id": subscribe["headers"]["req_id"].clone() },
                "body": { "errcode": 0 },
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
            account_id: "AIBOT-1".into()
        }
    );

    // 单聊回调 → Inbound(恒 to_me)
    server
        .send(WsMessage::text(
            serde_json::json!({
                "cmd": "aibot_msg_callback",
                "headers": { "req_id": "req-cb-1" },
                "body": {
                    "msgid": "MSG-1",
                    "aibotid": "AIBOT-1",
                    "chatid": "chat-1001",
                    "chattype": "single",
                    "from": { "userid": "u1001", "name": "张三" },
                    "msgtype": "text",
                    "text": { "content": "你好机器人" },
                },
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let message = {
        let mut found = None;
        for _ in 0..500 {
            match rx.try_recv() {
                Ok(ChannelEvent::Inbound(message)) => {
                    found = Some(message);
                    break;
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        found.expect("等待 Inbound 事件超时")
    };
    assert_eq!(message.message_id, "MSG-1");
    assert_eq!(message.chat.conversation_id, "chat-1001");
    assert_eq!(message.sender.user_id, "u1001");
    assert!(message.to_me);
    assert_eq!(message.text, "你好机器人");

    // 出站:aibot_send_msg(chatid + text)
    handle
        .send(
            &ChatRef::private("wecom", "chat-1001"),
            OutboundMessage::text("回复内容"),
        )
        .await
        .unwrap();
    // 出站帧到达(有界:心跳 ping 帧会被循环忽略,10 轮 × 3s 封顶)
    let mut delivered = false;
    for _ in 0..10 {
        let frame = match tokio::time::timeout(std::time::Duration::from_secs(3), server.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => text.to_string(),
            Ok(Some(other)) => {
                eprintln!("[wecom-test] 非文本帧: {other:?}");
                continue;
            }
            Ok(None) => {
                eprintln!("[wecom-test] 服务端流已关闭");
                break;
            }
            Err(_) => {
                eprintln!("[wecom-test] 3s 无帧");
                break;
            }
        };
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        if value["cmd"] == "aibot_send_msg" {
            assert_eq!(value["body"]["chatid"], "chat-1001");
            assert_eq!(value["body"]["chattype"], "single");
            assert_eq!(value["body"]["text"]["content"], "回复内容");
            delivered = true;
            break;
        }
    }
    assert!(delivered, "aibot_send_msg 应到达假服务端");
}

#[tokio::test]
async fn subscribe_rejection_fails_channel() {
    let (listener, endpoint) = spawn_wecom_listener().await;
    let channel = latent_channel::wecom::WecomChannel::new();
    channel
        .apply_config(&serde_json::json!({
            "botId": "AIBOT-1",
            "secret": "bad-secret",
            "wsEndpoint": endpoint,
        }))
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    // spawn 驱动 start(订阅帧在其轮询中发出)
    let start_task = tokio::spawn(async move { ChannelPlugin::start(&channel, tx).await });
    let mut server = accept_wecom(listener).await;
    let subscribe = loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(3), server.next())
            .await
            .unwrap()
            .expect("连接关闭")
            .unwrap();
        if let WsMessage::Text(text) = message {
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value["cmd"] == "aibot_subscribe" {
                break value;
            }
        }
    };
    // 拒绝订阅(errcode 非 0)→ start 以 Startup 错误失败
    server
        .send(WsMessage::text(
            serde_json::json!({
                "cmd": "aibot_subscribe",
                "headers": { "req_id": subscribe["headers"]["req_id"].clone() },
                "body": { "errcode": 10004 },
            })
            .to_string(),
        ))
        .await
        .unwrap();
    // 订阅被拒不改变 start 的返回(connect_loop 内部自持);失败经
    // Status::Failed 上报(凭据失效需宿主介入,不再重试)
    let handle = tokio::time::timeout(std::time::Duration::from_secs(5), start_task)
        .await
        .expect("start 应返回")
        .unwrap()
        .expect("start 本身应成功(失败走状态通道)");
    let mut status = ChannelStatus::Disconnected {
        reason: "initial".into(),
    };
    for _ in 0..100 {
        status = handle.status().await;
        if matches!(status, ChannelStatus::Failed { .. }) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    match &status {
        ChannelStatus::Failed { reason } => {
            assert!(reason.contains("订阅被拒"), "{reason}")
        }
        other => panic!("应上报 Failed 状态: {other:?}"),
    }
}

#[tokio::test]
async fn missing_credentials_refuse_to_start() {
    let channel = latent_channel::wecom::WecomChannel::new();
    let error = channel
        .apply_config(&serde_json::json!({ "botId": "AIBOT-1" }))
        .unwrap_err();
    assert!(error.to_string().contains("botId/secret 必填"), "{error}");
}
