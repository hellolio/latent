//! daemon 事件循环回归测试:**所有渠道启动失败时保持存活**(此前该场景会
//! 因事件通道关闭静默秒退,日志只有"已就绪→已退出"无任何原因)。

use std::sync::Arc;
use std::time::Duration;

use latent_ai::ScriptedProvider;
use latent_channel::mock::MockChannel;
use serde_json::json;
use tokio::sync::mpsc;

use latent_gateway::agents::SessionFactory;
use latent_gateway::approval::ChatApprovalUi;
use latent_gateway::auto_reply::Gateway;
use latent_gateway::channels::{ChannelManager, TaggedChannelEvent};
use latent_gateway::config::{validate, GatewayConfig};
use latent_gateway::pairing::PairingStore;
use latent_gateway::state::StateStore;

fn test_model() -> latent_ai::Model {
    latent_ai::Model::minimal("test-model", "mock", "mock")
}

/// 构造一个**未 attach 任何渠道**的 gateway(模拟"所有渠道启动失败"后
/// 的运行时形态:ChannelManager 空表)。
async fn build_gateway_without_channels() -> (Arc<Gateway>, mpsc::Receiver<TaggedChannelEvent>) {
    let value = json!({
        "gateway": { "auth": { "mode": "token", "token": "secret-token" } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    let config: GatewayConfig = validate(&value, false).unwrap();
    let provider: Arc<dyn latent_ai::Provider> =
        Arc::new(ScriptedProvider::new(&test_model(), vec![]));
    let approval = ChatApprovalUi::new();
    let state = Arc::new(StateStore::in_memory());
    let mut pairing = PairingStore::new(state.clone());
    for (channel, policy, allow_from) in ChannelManager::dm_policies(&config) {
        pairing.register_policy(channel, policy, allow_from);
    }
    let sessions_dir = std::env::temp_dir().join(format!(
        "latent-gw-daemon-test-{}-{}",
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
    // 注意:不 attach 任何渠道 —— 等价于"全部渠道启动失败"
    let gateway = Gateway::new(
        config,
        channels,
        factory,
        approval,
        pairing,
        state,
        broadcast_events,
    );
    let (_events_tx, events_rx) = mpsc::channel::<TaggedChannelEvent>(8);
    (gateway, events_rx)
}

#[tokio::test]
async fn event_loop_stays_alive_when_no_channels() {
    // 回归:此前所有渠道启动失败 → 事件通道关闭 → recv() 返回 None →
    // 主循环静默 break,daemon 在"已就绪"后瞬间退出且无任何原因
    let (gateway, events_rx) = build_gateway_without_channels().await;
    // 空的 ChannelManager:没有句柄、没有状态 → refresh_approval_transport
    // 会打"无可用审批通道"警告(fail-closed 预期路径)
    let future = latent_gateway::daemon::run_event_loop(gateway, events_rx);
    let result = tokio::time::timeout(Duration::from_secs(1), future).await;
    assert!(
        result.is_err(),
        "无渠道时事件循环必须保持存活(仅控制面可用),不应静默退出"
    );
}

#[tokio::test]
async fn event_loop_survives_channel_failure_event() {
    // 渠道 Failed 状态事件(模拟运行中渠道挂掉):manager 记录失败态 +
    // 未决审批落 Deny + 审批通道刷新,循环继续存活
    let value = json!({
        "gateway": { "auth": { "mode": "token", "token": "secret-token" } },
        "channels": { "mock": { "dmPolicy": "open", "allowFrom": ["*"] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    let config: GatewayConfig = validate(&value, false).unwrap();
    let provider: Arc<dyn latent_ai::Provider> =
        Arc::new(ScriptedProvider::new(&test_model(), vec![]));
    let approval = ChatApprovalUi::new();
    let state = Arc::new(StateStore::in_memory());
    let mut pairing = PairingStore::new(state.clone());
    for (channel, policy, allow_from) in ChannelManager::dm_policies(&config) {
        pairing.register_policy(channel, policy, allow_from);
    }
    let sessions_dir = std::env::temp_dir().join(format!(
        "latent-gw-daemon-fail-{}-{}",
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
    let (events_tx, events_rx) = mpsc::channel::<TaggedChannelEvent>(16);
    let mock = MockChannel::new("mock");
    channels
        .attach("mock", mock.clone(), 4000, events_tx.clone())
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

    let run = tokio::spawn(latent_gateway::daemon::run_event_loop(
        gateway.clone(),
        events_rx,
    ));
    // 注入渠道 Failed 状态事件
    events_tx
        .send(TaggedChannelEvent::Status {
            channel: "mock".into(),
            status: latent_channel::types::ChannelStatus::Failed {
                reason: "test-failure".into(),
            },
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!run.is_finished(), "渠道失败事件处理后事件循环应继续存活");
    // manager 记录了失败态;未决审批被清空(fail-closed)
    let snapshot = gateway.channels.status_snapshot().await;
    let (_, status, _) = snapshot.iter().find(|(id, _, _)| id == "mock").unwrap();
    assert!(status.contains("failed"), "{status}");
    assert!(gateway.approval.pending_ids().await.is_empty());
    run.abort();
}
