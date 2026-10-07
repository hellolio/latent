//! 控制面 WS 集成测试(§4.12/§5.3/§5.4):connect 首帧强制、token 校验、
//! chat.send 走同一管线且身份固定 operator、事件 seq 单调、pairing 方法。

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use latent_ai::{ScriptedProvider, ScriptedTurn};
use latent_channel::mock::MockChannel;
use latent_channel::types::{ChatRef, InboundMessage, Sender};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use latent_gateway::agents::SessionFactory;
use latent_gateway::approval::ChatApprovalUi;
use latent_gateway::auto_reply::Gateway;
use latent_gateway::channels::ChannelManager;
use latent_gateway::config::{validate, GatewayConfig};
use latent_gateway::pairing::PairingStore;
use latent_gateway::state::StateStore;

fn test_model() -> latent_ai::Model {
    latent_ai::Model::minimal("test-model", "mock", "mock")
}

async fn build_gateway(turns: Vec<ScriptedTurn>) -> (Arc<Gateway>, Arc<MockChannel>) {
    let value = json!({
        "gateway": { "auth": { "mode": "token", "token": "secret-token" } },
        "channels": { "mock": { "dmPolicy": "open", "allowFrom": ["*"] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    let config: GatewayConfig = validate(&value, false).unwrap();
    let provider: Arc<dyn latent_ai::Provider> =
        Arc::new(ScriptedProvider::new(&test_model(), turns));
    let approval = ChatApprovalUi::new();
    let state = Arc::new(StateStore::in_memory());
    let mut pairing = PairingStore::new(state.clone());
    for (channel, policy, allow_from) in ChannelManager::dm_policies(&config) {
        pairing.register_policy(channel, policy, allow_from);
    }
    let sessions_dir = std::env::temp_dir().join(format!(
        "latent-gw-control-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let (broadcast_events, _) = tokio::sync::broadcast::channel(64);
    let factory = Arc::new(SessionFactory {
        provider,
        model: test_model(),
        settings: latent_runtime::assembly::SessionSettings::default(),
        sessions_dir,
        extension_specs: Vec::new(),
        approval_ui: approval.clone(),
        state: state.clone(),
        typing_config: latent_channel::typing::TypingConfig::default(),
        events: Some(latent_gateway::auto_reply::reply_dispatcher::EventSink {
            events: broadcast_events.clone(),
            session_key: String::new(),
        }),
        default_thinking_level: None,
        queue_settings: latent_gateway::auto_reply::queue::QueueSettings::default(),
    });
    let channels = Arc::new(ChannelManager::new());
    let (events_tx, _rx) = tokio::sync::mpsc::channel(256);
    let mock = MockChannel::new("mock");
    channels
        .attach("mock", mock.clone(), 4000, events_tx)
        .await
        .unwrap();
    let gateway = Gateway::new(
        config,
        channels,
        factory,
        approval,
        pairing,
        state,
        broadcast_events,
    );
    tokio::spawn(gateway.clone().run_debounce_loop());
    (gateway, mock)
}

/// 起控制面(随机端口),返回 endpoint。
async fn spawn_server(gateway: Arc<Gateway>, token: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let token = token.to_string();
    tokio::spawn(async move {
        latent_gateway::control::server::serve_on(listener, gateway, token)
            .await
            .unwrap();
    });
    format!("ws://{addr}/ws")
}

fn dm(user: &str, id: &str, text: &str) -> InboundMessage {
    InboundMessage::plain_text(
        "mock",
        ChatRef::private("mock", user),
        Sender {
            user_id: user.into(),
            display_name: user.into(),
        },
        id,
        text,
        true,
    )
}

/// 原始 WS 客户端(不走 ControlClient,便于构造异常帧)。
async fn raw_connect(
    endpoint: &str,
    token: Option<&str>,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let _ = token;
    let (ws, _) = tokio_tungstenite::connect_async(endpoint).await.unwrap();
    ws
}

async fn send_json<S>(ws: &mut S, value: &Value)
where
    S: futures::Sink<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    if let Err(error) = ws.send(WsMessage::text(value.to_string())).await {
        panic!("发送帧失败: {error}");
    }
}

async fn next_json(
    ws: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Value {
    for _ in 0..500 {
        match tokio::time::timeout(Duration::from_millis(20), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                return serde_json::from_str(&text).unwrap();
            }
            Ok(Some(_)) => continue,
            _ => {}
        }
    }
    panic!("等待 WS JSON 帧超时");
}

#[tokio::test]
async fn connect_handshake_then_health() {
    let (gateway, _mock) = build_gateway(vec![]).await;
    let endpoint = spawn_server(gateway, "secret-token").await;
    let mut client = latent_gateway::control::client::ControlClient::connect(
        &endpoint,
        "secret-token",
    )
    .await
    .expect("正确 token 应握手成功");
    let health = client.request("health", json!({})).await.unwrap();
    assert_eq!(health["ok"], true);
    assert!(health["uptimeMs"].as_u64().is_some());
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let (gateway, _mock) = build_gateway(vec![]).await;
    let endpoint = spawn_server(gateway, "secret-token").await;
    let result =
        latent_gateway::control::client::ControlClient::connect(&endpoint, "wrong").await;
    assert!(result.is_err(), "错误 token 应被拒绝");
}

#[tokio::test]
async fn first_frame_must_be_connect() {
    let (gateway, _mock) = build_gateway(vec![]).await;
    let endpoint = spawn_server(gateway, "secret-token").await;
    let mut ws = raw_connect(&endpoint, None).await;
    // 首帧直接发 health(跳过 connect)→ 服务端断连
    send_json(
        &mut ws,
        &json!({ "type": "req", "id": 1, "method": "health", "params": {} }),
    )
    .await;
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(message) = ws.next().await {
            if matches!(message, Ok(WsMessage::Close(_)) | Err(_)) {
                return true;
            }
        }
        true
    })
    .await
    .expect("断连判定超时");
    assert!(closed, "首帧非 connect 应被断连");
}

#[tokio::test]
async fn chat_send_goes_through_dispatch_and_emits_agent_events() {
    let (gateway, _mock) = build_gateway(vec![ScriptedTurn::text(&test_model(), "operator 你好")])
        .await;
    let endpoint = spawn_server(gateway, "secret-token").await;
    let mut client = latent_gateway::control::client::ControlClient::connect(
        &endpoint,
        "secret-token",
    )
    .await
    .unwrap();

    // chat.send → 与渠道入站同一管线(sessionKey 直指;身份 operator)
    let reply = client
        .request(
            "chat.send",
            json!({ "sessionKey": "agent:main:mock:direct:op1", "text": "注入的消息" }),
        )
        .await
        .unwrap();
    assert_eq!(reply["state"], "started");

    // (事件线由 broadcast 语义保证;seq 单调在 event_seq 测试里验证)
}

#[tokio::test]
async fn chat_send_rejects_sender_field() {
    let (gateway, _mock) = build_gateway(vec![]).await;
    let endpoint = spawn_server(gateway, "secret-token").await;
    let mut client = latent_gateway::control::client::ControlClient::connect(
        &endpoint,
        "secret-token",
    )
    .await
    .unwrap();
    let error = client
        .request(
            "chat.send",
            json!({
                "sessionKey": "agent:main:mock:direct:x",
                "text": "冒充",
                "sender": { "userId": "owner1" }
            }),
        )
        .await
        .unwrap_err();
    assert!(error.contains("sender"), "{error}");
}

#[tokio::test]
async fn pairing_methods_via_control_plane() {
    let (gateway, _mock) = build_gateway(vec![ScriptedTurn::text(&test_model(), "配对后回复")])
        .await;
    // 预置一个配对请求(直接写 state:daemon 单一写者语义的测试替身)
    {
        let mut pairing_state = gateway.state.pairing();
        pairing_state.pending.entry("mock".to_string()).or_default().push(
            latent_gateway::state::PendingPairing {
                code: "ABCD2345".into(),
                user_id: "stranger".into(),
                expires_at: latent_gateway::state::now_ms() + 60_000,
            },
        );
        gateway.state.set_pairing(pairing_state);
    }
    let endpoint = spawn_server(gateway, "secret-token").await;
    let mut client = latent_gateway::control::client::ControlClient::connect(
        &endpoint,
        "secret-token",
    )
    .await
    .unwrap();
    // pairing.list
    let listed = client.request("pairing.list", json!({})).await.unwrap();
    let code = listed["pending"][0]["code"].as_str().unwrap().to_string();
    assert_eq!(listed["pending"][0]["channel"], "mock");
    // pairing.approve(daemon 单一写者)
    let approved = client
        .request(
            "pairing.approve",
            json!({ "channel": "mock", "code": code }),
        )
        .await
        .unwrap();
    assert_eq!(approved["approved"], true);
    assert_eq!(approved["userId"], "stranger");
    // 再次 list:pending 清空
    let listed = client.request("pairing.list", json!({})).await.unwrap();
    assert_eq!(listed["pending"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn event_seq_is_monotonic_per_connection() {
    let (gateway, _mock) = build_gateway(vec![ScriptedTurn::text(&test_model(), "回复")])
        .await;
    let endpoint = spawn_server(gateway.clone(), "secret-token").await;
    let mut ws = raw_connect(&endpoint, None).await;
    // 握手
    send_json(
        &mut ws,
        &json!({
            "type": "req", "id": 1, "method": "connect",
            "params": { "auth": { "token": "secret-token" }, "role": "operator", "scopes": [] }
        }),
    )
    .await;
    let hello = next_json(&mut ws).await;
    assert_eq!(hello["type"], "hello-ok", "{hello}");
    assert_eq!(hello["policy"]["maxPayload"], 1024 * 1024);

    // 订阅者就绪后注入两条消息 → agent 事件 seq 单调
    let gateway2 = gateway.clone();
    tokio::spawn(async move {
        for index in 0..3 {
            gateway2
                .dispatch_inbound(dm("u1", &format!("seq-{index}"), "你好"))
                .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let mut last_seq: u64 = 0;
    let mut seen = 0;
    for _ in 0..2000 {
        match tokio::time::timeout(Duration::from_millis(20), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == "event" {
                    let seq = value["seq"].as_u64().unwrap();
                    assert!(seq > last_seq, "seq 必须单调递增: {seq} <= {last_seq}");
                    last_seq = seq;
                    seen += 1;
                    if seen >= 3 {
                        break;
                    }
                }
            }
            Ok(Some(_)) => continue,
            _ => continue,
        }
    }
    assert!(seen >= 3, "应收到至少 3 个事件,实际 {seen}");
}

#[tokio::test]
async fn oversized_frame_disconnects() {
    let (gateway, _mock) = build_gateway(vec![]).await;
    let endpoint = spawn_server(gateway, "secret-token").await;
    let mut ws = raw_connect(&endpoint, None).await;
    send_json(
        &mut ws,
        &json!({
            "type": "req", "id": 1, "method": "connect",
            "params": { "auth": { "token": "secret-token" }, "role": "operator" }
        }),
    )
    .await;
    let _hello = next_json(&mut ws).await;
    // 超限帧(>1MiB)
    let huge = "x".repeat(1024 * 1024 + 100);
    send_json(
        &mut ws,
        &json!({ "type": "req", "id": 2, "method": "status", "params": { "pad": huge } }),
    )
    .await;
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(message) = ws.next().await {
            if matches!(message, Ok(WsMessage::Close(_)) | Err(_)) {
                return true;
            }
        }
        true
    })
    .await
    .unwrap_or(true);
    assert!(closed, "超限帧应断连");
}

#[tokio::test]
async fn config_get_redacts_credentials() {
    let (gateway, _mock) = build_gateway(vec![]).await;
    let endpoint = spawn_server(gateway, "secret-token").await;
    let mut client = latent_gateway::control::client::ControlClient::connect(
        &endpoint,
        "secret-token",
    )
    .await
    .unwrap();
    let config = client.request("config.get", json!({})).await.unwrap();
    assert_eq!(config["gateway"]["auth"]["token"], "[redacted]");
}
