//! telegram 全链路审批回归:真 TelegramChannel(假 Bot API 服务器)+
//! Gateway + ScriptedProvider + Confirm 模式 —— 复现"owner 回复 /approve
//! 后审批未传达到 agent"的用户报告场景。
//!
//! 链路:telegram 长轮询 → normalize_update → gateway dispatch → 工具审批
//! → ChatApprovalUi 发 owner DM → owner 回复 /approve → 命令层 resolve →
//! 工具执行 → 最终回复经 sendMessage 回到私聊。
//!
//! 另覆盖:owner 未配对 DM 时审批应答仍可达(owner 信任锚绕过 dmPolicy,
//! 回归:配对流程吃掉 /approve 导致审批永远无法 resolve)。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use latent_ai::{ScriptedProvider, ScriptedTurn};
use latent_gateway::agents::SessionFactory;
use latent_gateway::auto_reply::Gateway;
use latent_gateway::channels::{ChannelManager, TaggedChannelEvent};
use latent_gateway::config::GatewayConfig;
use latent_gateway::pairing::PairingStore;
use latent_gateway::state::StateStore;

// ---------------------------------------------------------------------------
// 假 Bot API 服务器(同 tests/telegram.rs,另带 getUpdates 注入队列)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeBotState {
    updates: std::sync::Mutex<Vec<serde_json::Value>>,
    sent: std::sync::Mutex<Vec<serde_json::Value>>,
    /// getMe 请求路径携带的 bot token(凭据物化端到端断言用)
    tokens: std::sync::Mutex<Vec<String>>,
}

async fn get_me(
    State(state): State<Arc<FakeBotState>>,
    Path(token): Path<String>,
) -> Json<serde_json::Value> {
    state.tokens.lock().unwrap().push(token);
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
    Json(serde_json::json!({ "ok": true, "result": { "message_id": 777 } }))
}

async fn spawn_fake_bot(state: Arc<FakeBotState>) -> String {
    let app = Router::new()
        .route("/bot{token}/getMe", get(get_me))
        .route("/bot{token}/getUpdates", get(get_updates))
        .route("/bot{token}/sendMessage", post(send_message))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn private_update(update_id: i64, user_id: i64, text: &str) -> serde_json::Value {
    serde_json::json!({
        "update_id": update_id,
        "message": {
            "message_id": update_id,
            "from": { "id": user_id, "first_name": "Owner" },
            "chat": { "id": user_id, "type": "private" },
            "text": text,
        }
    })
}

/// 等待假服务器收到发给指定 chat 的 sendMessage,返回文本列表。
async fn wait_for_sent(state: &FakeBotState, chat_id: &str, contain: &str) -> Vec<String> {
    for _ in 0..1000 {
        let texts: Vec<String> = state
            .sent
            .lock()
            .unwrap()
            .iter()
            .filter(|body| body["chat_id"].as_str() == Some(chat_id))
            .filter_map(|body| body["text"].as_str().map(str::to_string))
            .collect();
        if texts.iter().any(|text| text.contains(contain)) {
            return texts;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "等待 sendMessage(chat_id={chat_id}, 含 {contain:?}) 超时;已发送: {:?}",
        state.sent.lock().unwrap()
    );
}

// ---------------------------------------------------------------------------
// 装配(daemon 同款:真 telegram 渠道 + 事件循环 + 审批通道 + 防抖循环)
// ---------------------------------------------------------------------------

struct TelegramHarness {
    gateway: Arc<Gateway>,
    bot: Arc<FakeBotState>,
}

fn test_model() -> latent_ai::Model {
    latent_ai::Model::minimal("test-model", "mock", "mock")
}

async fn build_harness(
    bot: Arc<FakeBotState>,
    api_base: &str,
    config_value: serde_json::Value,
    turns: Vec<ScriptedTurn>,
    settings: latent_runtime::assembly::SessionSettings,
) -> TelegramHarness {
    let config: GatewayConfig =
        latent_gateway::config::validate(&config_value, false).expect("测试配置非法");

    let provider: Arc<dyn latent_ai::Provider> = Arc::new(ScriptedProvider::new(&test_model(), turns));
    let approval = latent_gateway::approval::ChatApprovalUi::new();
    let state = Arc::new(StateStore::in_memory());
    let mut pairing = PairingStore::new(state.clone());
    for (channel, policy, allow_from) in ChannelManager::dm_policies(&config) {
        pairing.register_policy(channel, policy, allow_from);
    }

    let sessions_dir = std::env::temp_dir().join(format!(
        "latent-tg-ap-sessions-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&sessions_dir).unwrap();

    let factory = Arc::new(SessionFactory {
        provider,
        model: test_model(),
        settings,
        sessions_dir,
        extension_specs: Vec::new(),
        approval_ui: approval.clone(),
        state: state.clone(),
        typing_config: latent_channel::typing::TypingConfig::default(),
        events: None,
        default_thinking_level: None,
        queue_settings: latent_gateway::auto_reply::queue::QueueSettings::default(),
    });

    let channels = Arc::new(ChannelManager::new());
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel::<TaggedChannelEvent>(256);
    // 真 telegram 渠道走工厂创建(与 daemon 同一通路);假 API 经插件配置注入
    let plugin = latent_gateway::channels::create_channel("telegram").expect("telegram feature");
    plugin
        .apply_config(&serde_json::json!({
            "botToken": "test-token", "apiBase": api_base, "pollTimeoutSecs": 0,
        }))
        .unwrap();
    let _handle = channels
        .attach("telegram", plugin, 4000, events_tx.clone())
        .await
        .expect("telegram 渠道启动");

    let gateway = Gateway::new(
        config,
        channels.clone(),
        factory,
        approval.clone(),
        pairing,
        state,
        tokio::sync::broadcast::channel(64).0,
    );
    // daemon 同款:审批通道 + 防抖冲刷循环 + per-message spawn 事件循环
    latent_gateway::daemon::refresh_approval_transport(&gateway).await;
    tokio::spawn(gateway.clone().run_debounce_loop());
    let loop_gateway = gateway.clone();
    tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            if let TaggedChannelEvent::Inbound { message, .. } = event {
                let gateway = loop_gateway.clone();
                tokio::spawn(async move {
                    gateway.dispatch_inbound(message).await;
                });
            }
        }
    });
    TelegramHarness { gateway, bot }
}

fn confirm_settings() -> latent_runtime::assembly::SessionSettings {
    // Confirm 模式:写命令(touch)触发人审
    latent_runtime::assembly::SessionSettings {
        session_mode: latent_runtime::facade::SessionMode::Confirm,
        ..Default::default()
    }
}

fn approval_turns() -> Vec<ScriptedTurn> {
    let toolcall_turn = ScriptedTurn::new(latent_ai::assistant_message(
        &test_model(),
        vec![latent_ai::ContentBlock::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "touch /tmp/latent-tg-ap-test"}),
        }],
        latent_ai::StopReason::ToolUse,
    ));
    vec![
        toolcall_turn,
        ScriptedTurn::text(&test_model(), "工具跑完了"),
    ]
}

/// 审批往返:注入触发消息 → 等审批请求 → 注入 /approve → 等最终回复。
async fn run_approval_roundtrip(harness: &TelegramHarness, owner_id: i64, mut update_id: i64) {
    // 1. 用户(owner)私聊发消息 → 触发 run + 工具审批
    harness
        .bot
        .updates
        .lock()
        .unwrap()
        .push(private_update(update_id, owner_id, "帮我建个文件"));
    update_id += 1;

    // 2. owner 收到审批请求(sendMessage)
    let texts = wait_for_sent(&harness.bot, &owner_id.to_string(), "需要批准").await;
    let approval_text = texts
        .iter()
        .find(|text| text.contains("需要批准"))
        .unwrap()
        .clone();
    let approval_id: u64 = approval_text
        .split_whitespace()
        .find_map(|token| token.parse::<u64>().ok())
        .expect("审批消息含自增 id");
    assert!(
        harness.gateway.approval.pending_ids().await.contains(&approval_id),
        "审批应处于待决状态"
    );

    // 3. owner 回复 /approve → 命令层 resolve → 工具执行
    harness
        .bot
        .updates
        .lock()
        .unwrap()
        .push(private_update(
            update_id,
            owner_id,
            &format!("/approve {approval_id} allow-once"),
        ));

    // 4. 审批回执 + 最终回复经 sendMessage 回到 owner 私聊(= 审批传达到了 agent)
    wait_for_sent(&harness.bot, &owner_id.to_string(), "已应用审批").await;
    wait_for_sent(&harness.bot, &owner_id.to_string(), "工具跑完了").await;
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[tokio::test]
async fn telegram_confirm_approval_roundtrip_reaches_agent() {
    let bot = Arc::new(FakeBotState::default());
    let api_base = spawn_fake_bot(bot.clone()).await;
    let harness = build_harness(
        bot,
        &api_base,
        serde_json::json!({
            "gateway": { "auth": { "token": "test-token" } },
            "channels": { "telegram": {
                "enabled": true, "botToken": "test-token", "dmPolicy": "open",
                "allowFrom": ["*"] } },
            "commands": { "ownerAllowFrom": ["telegram:1001"] },
            "messages": { "queue": { "debounceMs": 0 } }
        }),
        approval_turns(),
        confirm_settings(),
    )
    .await;

    run_approval_roundtrip(&harness, 1001, 101).await;

    // 工具确实执行(touch 落盘)
    assert!(
        std::path::Path::new("/tmp/latent-tg-ap-test").exists(),
        "批准后 bash 工具应已执行"
    );
}

/// owner 未配对 DM(dmPolicy=pairing 默认):/approve 应答不得被配对流程
/// 吞掉 —— owner 是信任锚,绕过 dmPolicy(回归:confirm 审批传达到不了
/// agent 的用户报告;修复前 owner 回复会被"配对码 XXXX"顶掉)。
#[tokio::test]
async fn telegram_owner_bypasses_dm_pairing_for_approvals() {
    let bot = Arc::new(FakeBotState::default());
    let api_base = spawn_fake_bot(bot.clone()).await;
    // dmPolicy=pairing 且 allowFrom 不含 owner → 修复前 owner 消息进配对流程
    let harness = build_harness(
        bot,
        &api_base,
        serde_json::json!({
            "gateway": { "auth": { "token": "test-token" } },
            "channels": { "telegram": {
                "enabled": true, "botToken": "test-token", "dmPolicy": "pairing" } },
            "commands": { "ownerAllowFrom": ["telegram:1001"] },
            "messages": { "queue": { "debounceMs": 0 } }
        }),
        approval_turns(),
        confirm_settings(),
    )
    .await;

    run_approval_roundtrip(&harness, 1001, 201).await;

    // 全程不应出现配对码回复
    let sent = harness.bot.sent.lock().unwrap();
    assert!(
        !sent
            .iter()
            .any(|body| body["text"].as_str().unwrap_or("").contains("配对码")),
        "owner 不应被要求配对: {:?}",
        *sent
    );
}

/// 陌生人未配对 DM 仍走配对流程(owner 绕过不放宽普通人门禁)。
#[tokio::test]
async fn telegram_stranger_still_gets_pairing_code() {
    let bot = Arc::new(FakeBotState::default());
    let api_base = spawn_fake_bot(bot.clone()).await;
    let harness = build_harness(
        bot,
        &api_base,
        serde_json::json!({
            "gateway": { "auth": { "token": "test-token" } },
            "channels": { "telegram": {
                "enabled": true, "botToken": "test-token", "dmPolicy": "pairing" } },
            "commands": { "ownerAllowFrom": ["telegram:1001"] },
            "messages": { "queue": { "debounceMs": 0 } }
        }),
        vec![ScriptedTurn::text(&test_model(), "你好")],
        confirm_settings(),
    )
    .await;

    harness
        .bot
        .updates
        .lock()
        .unwrap()
        .push(private_update(301, 9999, "你好"));
    wait_for_sent(&harness.bot, "9999", "配对码").await;
}

/// P0-2/E7 回归:`$ENV` 凭据经 materialize_channel_credentials 写回 raw
/// 后,start_enabled 的 raw overlay 不得把 `$ENV` 字面量打回插件 ——
/// 假服务器必须收到**解析后**的 token(修复前 getMe 拿到
/// `bot$TELEGRAM_BOT_TOKEN/getMe` → 404)。
#[tokio::test]
async fn start_enabled_delivers_resolved_env_credentials() {
    std::env::set_var("LAT_GW_P02_TG_TOKEN", "resolved-token-42");
    let bot = Arc::new(FakeBotState::default());
    let api_base = spawn_fake_bot(bot.clone()).await;
    let config_value = serde_json::json!({
        "gateway": { "auth": { "token": "test-token" } },
        "channels": { "telegram": {
            "enabled": true, "botToken": "$LAT_GW_P02_TG_TOKEN", "dmPolicy": "pairing",
            "apiBase": api_base, "pollTimeoutSecs": 0 } }
    });
    let config: GatewayConfig =
        latent_gateway::config::validate(&config_value, false).expect("测试配置非法");
    // daemon 同序:load raw → materialize(解析结果同时写回类型化与 raw)
    let mut raw = config_value.clone();
    let config = latent_gateway::config::materialize_channel_credentials(config, &mut raw)
        .await
        .unwrap();
    let manager = ChannelManager::new();
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let failures = manager
        .start_enabled(&config, raw.get("channels").unwrap(), tx)
        .await;
    assert!(failures.is_empty(), "渠道应启动成功: {failures:?}");
    // 等首个 getMe 到达(轮询循环启动即自检)
    for _ in 0..500 {
        if !bot.tokens.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let tokens = bot.tokens.lock().unwrap();
    assert!(
        tokens.iter().any(|token| token == "resolved-token-42"),
        "getMe 必须收到解析后的 token: {tokens:?}"
    );
    assert!(
        !tokens.iter().any(|token| token.contains("$LAT_GW")),
        "不得把 $ENV 字面量发给平台: {tokens:?}"
    );
}
