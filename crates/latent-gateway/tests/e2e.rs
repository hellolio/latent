//! gateway 引擎端到端集成测试(mock 渠道 + ScriptedProvider,全离线):
//! dispatch 管线、四模式队列、去重、群策略、命令、配对、审批、重启恢复。

use std::sync::Arc;
use std::time::Duration;

use latent_ai::{ScriptedProvider, ScriptedTurn};
use latent_channel::mock::MockChannel;
use latent_channel::types::{ChatRef, InboundMessage, Sender};

use latent_gateway::agents::SessionFactory;
use latent_gateway::approval::{ApprovalTransport, ChatApprovalUi};
use latent_gateway::auto_reply::Gateway;
use latent_gateway::channels::ChannelManager;
use latent_gateway::config::GatewayConfig;
use latent_gateway::pairing::PairingStore;
use latent_gateway::state::StateStore;

// ---------------------------------------------------------------------------
// 测试装配
// ---------------------------------------------------------------------------

struct TestHarness {
    gateway: Arc<Gateway>,
    mock: Arc<MockChannel>,
    mock_handle: latent_channel::plugin::ChannelHandle,
    #[allow(dead_code)]
    sessions_dir: std::path::PathBuf,
    state: Arc<StateStore>,
}

fn test_model() -> latent_ai::Model {
    latent_ai::Model::minimal("test-model", "mock", "mock")
}

async fn build_harness(
    turns: Vec<ScriptedTurn>,
    config_json: serde_json::Value,
) -> TestHarness {
    build_harness_full(turns, config_json, Arc::new(StateStore::in_memory()), None).await
}

#[allow(dead_code)]
async fn build_harness_with_state(
    turns: Vec<ScriptedTurn>,
    config_json: serde_json::Value,
    state: Arc<StateStore>,
) -> TestHarness {
    build_harness_full(turns, config_json, state, None).await
}

async fn build_harness_full(
    turns: Vec<ScriptedTurn>,
    config_json: serde_json::Value,
    state: Arc<StateStore>,
    settings: Option<latent_runtime::assembly::SessionSettings>,
) -> TestHarness {
    // 类型化校验面(补默认 token,除非测试显式给)
    let mut value = config_json;
    if value.get("gateway").is_none() {
        value["gateway"] = serde_json::json!({});
    }
    if value["gateway"]["auth"]["token"].is_null()
        || value["gateway"]["auth"]["token"] == ""
    {
        value["gateway"]["auth"]["token"] = "test-token".into();
    }
    let config: GatewayConfig =
        latent_gateway::config::validate(&value, false).expect("测试配置非法");

    let provider: Arc<dyn latent_ai::Provider> =
        Arc::new(ScriptedProvider::new(&test_model(), turns));
    let approval = ChatApprovalUi::new();
    let mut pairing = PairingStore::new(state.clone());
    for (channel, policy, allow_from) in ChannelManager::dm_policies(&config) {
        pairing.register_policy(channel, policy, allow_from);
    }

    let sessions_dir = std::env::temp_dir().join(format!(
        "latent-gw-test-sessions-{}-{}",
        std::process::id(),
        unique()
    ));
    std::fs::create_dir_all(&sessions_dir).unwrap();

    // config 的 messages.queue → factory(新会话队列设置)
    let queue_json = &value["messages"]["queue"];
    let queue_settings = latent_gateway::auto_reply::queue::QueueSettings {
        mode: queue_json["mode"]
            .as_str()
            .and_then(|m| latent_gateway::config::QueueMode::parse(m).ok())
            .unwrap_or_default(),
        debounce_ms: queue_json["debounceMs"].as_u64().unwrap_or(500),
        cap: queue_json["cap"].as_u64().unwrap_or(20) as usize,
        drop_policy: match queue_json["drop"].as_str() {
            Some("old") => latent_gateway::config::DropPolicy::Old,
            Some("new") => latent_gateway::config::DropPolicy::New,
            _ => latent_gateway::config::DropPolicy::Summarize,
        },
    };
    let factory = Arc::new(SessionFactory {
        provider,
        model: test_model(),
        settings: settings.unwrap_or_default(),
        sessions_dir: sessions_dir.clone(),
        extension_specs: Vec::new(),
        approval_ui: approval.clone(),
        state: state.clone(),
        typing_config: latent_channel::typing::TypingConfig::default(),
        events: Some(latent_gateway::auto_reply::reply_dispatcher::EventSink {
            events: tokio::sync::broadcast::channel(64).0,
            session_key: String::new(),
        }),
        default_thinking_level: None,
        queue_settings,
    });

    let channels = Arc::new(ChannelManager::new());
    let (events_tx, _events_rx) = tokio::sync::mpsc::channel(256);
    let mock = MockChannel::new("mock");
    let mock_handle = channels
        .attach("mock", mock.clone(), 4000, events_tx.clone())
        .await
        .expect("mock 渠道启动");
    // 记录 Connected(mention gating 的 self_ids = 机器人账号)
    channels
        .record_status(
            "mock",
            latent_channel::types::ChannelStatus::Connected {
                account_id: "bot-1".into(),
            },
        )
        .await;

    let gateway = Gateway::new(
        config,
        channels,
        factory,
        approval,
        pairing,
        state.clone(),
        tokio::sync::broadcast::channel(64).0,
    );
    // daemon 同款:防抖冲刷循环(harness 内常驻)
    tokio::spawn(gateway.clone().run_debounce_loop());
    TestHarness {
        gateway,
        mock,
        mock_handle,
        sessions_dir,
        state,
    }
}

fn unique() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
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

fn group(group_id: &str, user: &str, id: &str, text: &str, at_bot: bool) -> InboundMessage {
    let mut message = InboundMessage::plain_text(
        "mock",
        ChatRef::group("mock", group_id),
        Sender {
            user_id: user.into(),
            display_name: user.into(),
        },
        id,
        text,
        at_bot,
    );
    if at_bot {
        message.segments.push(latent_channel::types::Segment::at("bot-1"));
        message.reply_to_me = false;
    }
    message
}

/// 等待 mock 渠道发给指定会话的第 n 条文本出站。
async fn wait_for_text(mock: &MockChannel, min: usize) -> Vec<String> {
    for _ in 0..1000 {
        let sent = mock.sent();
        if sent.len() >= min {
            return sent
                .iter()
                .map(|(_, outbound)| match &outbound.segments[0] {
                    latent_channel::types::Segment::Text(text) => text.clone(),
                    other => format!("{other:?}"),
                })
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待出站消息超时:{}", mock.sent().len());
}

async fn wait_for_reply_containing(harness: &TestHarness, needle: &str) -> bool {
    for _ in 0..1000 {
        for (_, outbound) in harness.mock.sent() {
            if let latent_channel::types::Segment::Text(text) = &outbound.segments[0] {
                if text.contains(needle) {
                    return true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

fn default_config() -> serde_json::Value {
    serde_json::json!({
        "channels": { "mock": { "dmPolicy": "open", "allowFrom": ["*"] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": {
            "queue": { "mode": "steer", "debounceMs": 0, "cap": 20, "drop": "summarize" },
            "groupChat": {
                "requireMention": true,
                "groupPolicy": "allowlist",
                "groupAllowFrom": ["mock:group1"]
            }
        }
    })
}

// ---------------------------------------------------------------------------
// 端到端:消息 → 回复
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dm_message_gets_reply() {
    let harness = build_harness(
        vec![ScriptedTurn::text(&test_model(), "你好,我是回复")],
        default_config(),
    )
    .await;
    harness
        .gateway
        .dispatch_inbound(dm("u1", "m-1", "你好"))
        .await;
    let texts = wait_for_text(&harness.mock, 1).await;
    assert_eq!(texts[0], "你好,我是回复");
}

#[tokio::test]
async fn dedupe_drops_repeated_message_id() {
    // 同一 message_id 只跑一次(claim 先于 ACK)
    let harness = build_harness(
        vec![ScriptedTurn::text(&test_model(), "只跑一次")],
        default_config(),
    )
    .await;
    let gateway = harness.gateway.clone();
    let first = gateway.dispatch_inbound(dm("u1", "m-dup", "第一条"));
    // 重复消息并发投递
    let gateway2 = gateway.clone();
    let second = tokio::spawn(async move {
        gateway2.dispatch_inbound(dm("u1", "m-dup", "第一条")).await;
    });
    first.await;
    second.await.unwrap();
    let texts = wait_for_text(&harness.mock, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let total = harness.mock.sent().len();
    assert_eq!(total, 1, "重复消息应被去重,实际出站 {texts:?}");
}

// ---------------------------------------------------------------------------
// 群策略与 @ 门
// ---------------------------------------------------------------------------

#[tokio::test]
async fn group_policy_and_mention_gate() {
    let harness = build_harness(
        vec![
            ScriptedTurn::text(&test_model(), "群回复"),
        ],
        default_config(),
    )
    .await;
    // 非白名单群 → 丢弃
    harness
        .gateway
        .dispatch_inbound(group("group-unknown", "u1", "m-1", "@bot-1 你好", true))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        harness.mock.sent().is_empty(),
        "非白名单群消息应被丢弃"
    );
    // 白名单群但未 @ → 丢弃(requireMention 默认 true)
    harness
        .gateway
        .dispatch_inbound(group("group1", "u1", "m-2", "普通消息", false))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(harness.mock.sent().is_empty(), "未 @ 消息应被丢弃");
    // 白名单群 + @ → 回复
    harness
        .gateway
        .dispatch_inbound(group("group1", "u1", "m-3", "@bot-1 帮忙", true))
        .await;
    let texts = wait_for_text(&harness.mock, 1).await;
    assert_eq!(texts[0], "群回复");
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_commands_work() {
    let harness = build_harness(
        vec![ScriptedTurn::text(&test_model(), "先跑一轮")],
        default_config(),
    )
    .await;
    // /help
    harness.gateway.dispatch_inbound(dm("u1", "c-1", "/help")).await;
    assert!(
        wait_for_reply_containing(&harness, "/approve").await,
        "/help 应列出命令"
    );
    // 未识别命令 → 本地警告
    harness.gateway.dispatch_inbound(dm("u1", "c-2", "/nope")).await;
    assert!(wait_for_reply_containing(&harness, "未知命令").await);
    // /status(会话未创建)
    harness.gateway.dispatch_inbound(dm("u1", "c-3", "/status")).await;
    assert!(wait_for_reply_containing(&harness, "会话尚未创建").await);
    // 正常消息建会话
    harness.gateway.dispatch_inbound(dm("u1", "c-4", "你好")).await;
    assert!(wait_for_reply_containing(&harness, "先跑一轮").await);
    // /status(会话已建)
    harness.gateway.dispatch_inbound(dm("u1", "c-5", "/status")).await;
    assert!(wait_for_reply_containing(&harness, "session:").await);
    assert!(wait_for_reply_containing(&harness, "model:").await);
    // /new
    harness.gateway.dispatch_inbound(dm("u1", "c-6", "/new")).await;
    assert!(wait_for_reply_containing(&harness, "已新建会话").await);
    // /stop(无活跃 run,仍应回执)
    harness.gateway.dispatch_inbound(dm("u1", "c-7", "/stop")).await;
    assert!(wait_for_reply_containing(&harness, "已中止").await);
    // /model(无参 = 查看当前)
    harness.gateway.dispatch_inbound(dm("u1", "c-8", "/model")).await;
    assert!(wait_for_reply_containing(&harness, "test-model").await);
}

#[tokio::test]
async fn group_session_commands_are_owner_only() {
    let harness = build_harness(
        vec![ScriptedTurn::text(&test_model(), "群回复")],
        default_config(),
    )
    .await;
    // 先正常消息建群会话(@ 机器人)
    harness
        .gateway
        .dispatch_inbound(group("group1", "member", "g-0", "@bot-1 打个招呼", true))
        .await;
    assert!(wait_for_reply_containing(&harness, "群回复").await);
    // 非 owner 在群里发 /new → 拒绝(§8 偏离 11:群会话全群共享)
    harness
        .gateway
        .dispatch_inbound(group("group1", "member", "g-1", "/new", true))
        .await;
    assert!(wait_for_reply_containing(&harness, "该命令仅 owner 可用").await);
    // 非 owner /mode(plan 也收紧)与 /model
    harness
        .gateway
        .dispatch_inbound(group("group1", "member", "g-1b", "/mode plan", true))
        .await;
    assert!(wait_for_reply_containing(&harness, "该命令仅 owner 可用").await);
    harness
        .gateway
        .dispatch_inbound(group("group1", "member", "g-1c", "/model mock/test-model", true))
        .await;
    assert!(wait_for_reply_containing(&harness, "该命令仅 owner 可用").await);
    // owner 在群里发 /new → 通过
    harness
        .gateway
        .dispatch_inbound(group("group1", "owner1", "g-2", "/new", true))
        .await;
    assert!(wait_for_reply_containing(&harness, "已新建会话").await);
    // /status 所有人可用
    harness
        .gateway
        .dispatch_inbound(group("group1", "member", "g-3", "/status", true))
        .await;
    assert!(
        wait_for_reply_containing(&harness, "session:").await,
        "/status 不应受 owner 限制"
    );
}

// ---------------------------------------------------------------------------
// 四模式队列
// ---------------------------------------------------------------------------

async fn build_busy_session(
    config_json: serde_json::Value,
) -> (TestHarness, Arc<Gateway>, ChatRef) {
    let harness = build_harness(
        vec![
            ScriptedTurn::text(&test_model(), "slow").with_delay(600),
            ScriptedTurn::text(&test_model(), "steered reply"),
            ScriptedTurn::text(&test_model(), "followup reply"),
            ScriptedTurn::text(&test_model(), "collect reply"),
        ],
        config_json,
    )
    .await;
    let chat = ChatRef::private("mock", "u1");
    let gateway = harness.gateway.clone();
    // 占住 run:第一条慢消息 spawn
    let gateway2 = gateway.clone();
    let message = dm("u1", "q-0", "占位消息");
    tokio::spawn(async move { gateway2.dispatch_inbound(message).await });
    // 等会话进入 busy(状态 isStreaming)
    let key = "agent:main:mock:direct:u1";
    for _ in 0..300 {
        if let Some(session) = gateway.registry.get(key).await {
            if session.built.session.agent().state_snapshot().is_streaming {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (harness, gateway, chat)
}

#[tokio::test]
async fn queue_mode_steer_injects_into_run() {
    let (harness, gateway, _chat) = build_busy_session(default_config()).await;
    gateway.dispatch_inbound(dm("u1", "q-1", "转向")).await;
    let (steering, follow_up) = gateway
        .registry
        .get("agent:main:mock:direct:u1")
        .await
        .unwrap()
        .built
        .session
        .queue_depths();
    assert_eq!((steering, follow_up), (1, 0), "steer 应入 steering 队列");
    let _ = harness;
}

#[tokio::test]
async fn queue_mode_followup_enqueues_new_turn() {
    let config = serde_json::json!({
        "channels": { "mock": { "dmPolicy": "open", "allowFrom": ["*"] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "mode": "followup", "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    let (harness, gateway, _chat) = build_busy_session(config).await;
    gateway.dispatch_inbound(dm("u1", "q-1", "追加")).await;
    let (steering, follow_up) = gateway
        .registry
        .get("agent:main:mock:direct:u1")
        .await
        .unwrap()
        .built
        .session
        .queue_depths();
    assert_eq!((steering, follow_up), (0, 1), "followup 应入 follow_up 队列");
    let _ = harness;
}

#[tokio::test]
async fn queue_mode_collect_buffers_and_merges() {
    let config = serde_json::json!({
        "channels": { "mock": { "dmPolicy": "open", "allowFrom": ["*"] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "mode": "collect", "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    let (harness, gateway, _chat) = build_busy_session(config).await;
    gateway.dispatch_inbound(dm("u1", "q-1", "第一句")).await;
    gateway.dispatch_inbound(dm("u1", "q-2", "第二句")).await;
    let session = gateway
        .registry
        .get("agent:main:mock:direct:u1")
        .await
        .unwrap();
    assert_eq!(
        session.pending.lock().await.len(),
        2,
        "collect 应在网关缓冲合并"
    );
    let _ = harness;
}

#[tokio::test]
async fn queue_mode_interrupt_aborts_and_reprompts() {
    let config = serde_json::json!({
        "channels": { "mock": { "dmPolicy": "open", "allowFrom": ["*"] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "mode": "interrupt", "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    let (harness, gateway, _chat) = build_busy_session(config).await;
    gateway.dispatch_inbound(dm("u1", "q-1", "打断")).await;
    // interrupt:abort 后重新 prompt → 会话最终空闲
    let session = gateway
        .registry
        .get("agent:main:mock:direct:u1")
        .await
        .unwrap();
    for _ in 0..500 {
        if !session.built.session.agent().state_snapshot().is_streaming {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !session.built.session.agent().state_snapshot().is_streaming,
        "interrupt 应终结忙碌状态"
    );
    let _ = harness;
}

#[tokio::test]
async fn queue_cap_overflow_drop_new_rejects() {
    let config = serde_json::json!({
        "channels": { "mock": { "dmPolicy": "open", "allowFrom": ["*"] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "mode": "collect", "cap": 1, "drop": "new", "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    let (harness, gateway, _chat) = build_busy_session(config).await;
    gateway.dispatch_inbound(dm("u1", "q-1", "第一句")).await;
    gateway.dispatch_inbound(dm("u1", "q-2", "第二句")).await;
    gateway.dispatch_inbound(dm("u1", "q-3", "第三句")).await;
    let texts = wait_for_text(&harness.mock, 1).await;
    assert!(
        texts.iter().any(|t| t.contains("队列已满")),
        "drop=new 应回执拒绝: {texts:?}"
    );
    let session = gateway
        .registry
        .get("agent:main:mock:direct:u1")
        .await
        .unwrap();
    assert_eq!(session.pending.lock().await.len(), 1, "cap=1 只保留一条");
}

// ---------------------------------------------------------------------------
// 配对
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pairing_flow_end_to_end() {
    let config = serde_json::json!({
        "channels": { "mock": { "dmPolicy": "pairing", "allowFrom": [] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    let harness = build_harness(
        vec![ScriptedTurn::text(&test_model(), "配对后的回复")],
        config,
    )
    .await;
    // 陌生人私聊 → 配对码回执(不走模型)
    harness.gateway.dispatch_inbound(dm("stranger", "p-1", "你好")).await;
    let texts = wait_for_text(&harness.mock, 1).await;
    assert!(texts[0].contains("配对码"), "{}", texts[0]);
    // 取码批准(daemon 单一写者)
    let pending = harness.gateway.pairing.list_pending();
    let code = pending[0].1[0].code.clone();
    harness
        .gateway
        .pairing
        .approve("mock", &code, latent_gateway::state::now_ms())
        .unwrap();
    // 再次私聊 → 放行进模型
    harness
        .gateway
        .dispatch_inbound(dm("stranger", "p-2", "又来了"))
        .await;
    let texts = wait_for_text(&harness.mock, 2).await;
    assert_eq!(texts[1], "配对后的回复");
}

// ---------------------------------------------------------------------------
// 审批:请求发 owner DM,/approve 跨会话路由
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approval_routes_to_owner_and_back() {
    let config = serde_json::json!({
        "channels": { "mock": { "dmPolicy": "open", "allowFrom": ["*"] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    // settings 会话模式 Confirm:写命令(touch)触发人审
    let settings = latent_runtime::assembly::SessionSettings {
        session_mode: latent_core::SessionMode::Confirm,
        ..Default::default()
    };
    let toolcall_turn = ScriptedTurn::new(latent_ai::assistant_message(
        &test_model(),
        vec![latent_ai::ContentBlock::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "touch /tmp/latent-gw-ap-test"}),
        }],
        latent_ai::StopReason::ToolUse,
    ));
    let harness = build_harness_full(
        vec![toolcall_turn, ScriptedTurn::text(&test_model(), "工具跑完了")],
        config,
        Arc::new(StateStore::in_memory()),
        Some(settings),
    )
    .await;
    // 审批通道:owner 私聊
    harness
        .gateway
        .approval
        .set_transport(ApprovalTransport {
            handle: harness.mock_handle.clone(),
            chat: ChatRef::private("mock", "owner1"),
        })
        .await;

    // 用户消息触发工具审批(run 挂起等待应答)
    let gateway = harness.gateway.clone();
    let run = tokio::spawn(async move {
        gateway
            .dispatch_inbound(dm("u1", "a-1", "帮我建个文件"))
            .await;
    });

    // 等 owner 收到审批请求
    let mut approval_id: Option<u64> = None;
    for _ in 0..1000 {
        for (chat, outbound) in harness.mock.sent() {
            if chat.conversation_id == "owner1" {
                if let latent_channel::types::Segment::Text(text) = &outbound.segments[0] {
                    if text.contains("需要批准") {
                        // "/approve {id} allow-once|allow-always|deny"
                        for token in text.split_whitespace() {
                            if let Ok(id) = token.parse::<u64>() {
                                approval_id = Some(id);
                            }
                        }
                        break;
                    }
                }
            }
        }
        if approval_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let approval_id = approval_id.expect("owner 应收到审批消息(含自增 id)");
    assert!(harness.gateway.approval.pending_ids().await.contains(&approval_id));

    // owner 用聊天命令应答 allow-once → 审批解析,工具执行,run 收尾
    harness
        .gateway
        .dispatch_inbound(dm("owner1", "a-2", &format!("/approve {approval_id} allow-once")))
        .await;
    run.await.unwrap();

    // 最终回复到达用户私聊
    let mut user_reply = false;
    for _ in 0..500 {
        for (chat, outbound) in harness.mock.sent() {
            if chat.conversation_id == "u1" {
                if let latent_channel::types::Segment::Text(text) = &outbound.segments[0] {
                    if text.contains("工具跑完了") {
                        user_reply = true;
                    }
                }
            }
        }
        if user_reply {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(user_reply, "用户应收到工具执行后的最终回复");
}

// 非 owner 发 /approve 无效(审批应答权仅 owner,§5.2)
#[tokio::test]
async fn approve_command_is_owner_only() {
    let config = serde_json::json!({
        "channels": { "mock": { "dmPolicy": "open", "allowFrom": ["*"] } },
        "commands": { "ownerAllowFrom": ["mock:owner1"] },
        "messages": { "queue": { "debounceMs": 0 }, "groupChat": {
            "requireMention": true, "groupPolicy": "allowlist", "groupAllowFrom": ["mock:group1"] } }
    });
    let harness = build_harness(vec![], config).await;
    harness
        .gateway
        .dispatch_inbound(dm("random", "ap-1", "/approve 1 deny"))
        .await;
    let texts = wait_for_text(&harness.mock, 1).await;
    assert_eq!(texts[0], "该命令仅 owner 可用");
}

// ---------------------------------------------------------------------------
// 重启恢复(state.json)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_resume_from_state_file() {
    let state_path = std::env::temp_dir().join(format!(
        "latent-gw-resume-state-{}-{}.json",
        std::process::id(),
        unique()
    ));
    let key = "agent:main:mock:direct:u1";
    {
        let state = Arc::new(StateStore::load(state_path.clone()));
        let harness = build_harness_full(
            vec![ScriptedTurn::text(&test_model(), "第一轮")],
            default_config(),
            state,
            None,
        )
        .await;
        harness
            .gateway
            .dispatch_inbound(dm("u1", "r-1", "第一句"))
            .await;
        wait_for_text(&harness.mock, 1).await;
        // state.json 已记录 session key → 文件
        let snapshot = harness.state.snapshot();
        assert!(
            snapshot.sessions.contains_key(key),
            "session 文件应持久化进 state.json"
        );
        let file = snapshot.sessions[key].file.clone();
        assert!(std::path::Path::new(&file).exists(), "会话文件应存在");
    }
    // 重启:同一 state.json → resume 同一文件
    {
        let state = Arc::new(StateStore::load(state_path.clone()));
        let harness = build_harness_full(
            vec![ScriptedTurn::text(&test_model(), "第二轮")],
            default_config(),
            state,
            None,
        )
        .await;
        harness
            .gateway
            .dispatch_inbound(dm("u1", "r-2", "第二句"))
            .await;
        wait_for_text(&harness.mock, 1).await;
        let snapshot = harness.state.snapshot();
        let file = &snapshot.sessions[key].file;
        let entries = std::fs::read_to_string(file).unwrap();
        assert!(
            entries.contains("第一句"),
            "resume 应沿用原会话文件(转录即真相): {entries}"
        );
    }
    std::fs::remove_file(&state_path).ok();
}

// ---------------------------------------------------------------------------
// 自消息防环(渠道源头已丢;gateway 层验证防环字段语义)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_echo_messages_are_dropped_by_channel() {
    // mock 渠道不做归一化丢弃(它按测试指令原样推);防环语义由 qq 渠道
    // 归一化层保证(tests/qq.rs)。此处验证:InboundMessage 无自消息标记面,
    // 防环不依赖 gateway 字段。
    let harness = build_harness(
        vec![ScriptedTurn::text(&test_model(), "回复")],
        default_config(),
    )
    .await;
    let message = dm("bot-1", "echo-1", "机器人自己说的");
    harness.gateway.dispatch_inbound(message).await;
    wait_for_text(&harness.mock, 1).await;
    // (防环断言本体在 tests/qq.rs 的 napcat_roundtrip_with_anti_echo)
}
