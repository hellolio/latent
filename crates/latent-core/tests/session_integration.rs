//! AgentSession 集成测试(04 文档):装配、事件层、steer/followUp、
//! setActiveToolsByName、系统提示词 diff、overflow 恢复、持久化。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use latent_agent::{
    AgentEvent, AgentMessage, BudgetKind, PassthroughHooks, RunStop, Tool, ToolCall, ToolError,
    ToolOutput, ToolUpdater, TurnLimits,
};
use latent_ai::{Model, ScriptedProvider, ScriptedTurn};
use latent_core::{
    create_agent_session, permission::PLAN_MODE_ENTER_SECTION, permission::PLAN_MODE_EXIT_SECTION,
    AgentSessionConfig, AgentSessionEvent, ApprovalRules, CoreError, NoopUi, PermissionEngine,
    SandboxConfig, SessionSubscriber, SystemPromptOptions,
};
use tokio_util::sync::CancellationToken;

fn model() -> Model {
    Model::minimal("mock-1", "mock", "mock")
}

fn tool_call(id: &str, name: &str) -> latent_ai::ContentBlock {
    latent_ai::ContentBlock::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: serde_json::json!({"x": "1"}),
    }
}

struct TestTool {
    name: String,
    calls: AtomicU32,
}

#[async_trait]
impl Tool for TestTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "required": ["x"], "properties": {"x": {"type": "string"}}})
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some(format!("{}(x): test tool", self.name))
    }
    async fn execute(
        &self,
        _call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolOutput::text(format!("{} executed", self.name)))
    }
}

#[derive(Default)]
struct EventLog {
    events: Mutex<Vec<String>>,
}

impl EventLog {
    fn record(&self, line: String) {
        self.events.lock().unwrap().push(line);
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl SessionSubscriber for EventLog {
    async fn on_session_event(&self, event: &AgentSessionEvent) {
        match event {
            AgentSessionEvent::ApprovalRequested { .. }
            | AgentSessionEvent::ApprovalResolved { .. } => {}
            AgentSessionEvent::Agent(agent_event) => {
                if let AgentEvent::MessageDelta {
                    delta: latent_agent::MessageDeltaPayload::Text { delta },
                } = agent_event
                {
                    self.record(format!("delta:{delta}"))
                }
            }
            AgentSessionEvent::AgentSettled => self.record("settled".into()),
            AgentSessionEvent::QueueUpdate {
                steering,
                follow_up,
            } => {
                self.record(format!("queue:{steering}/{follow_up}"));
            }
            AgentSessionEvent::AutoRetryStart { .. } => self.record("retry_start".into()),
            AgentSessionEvent::AutoRetryEnd { .. } => self.record("retry_end".into()),
        }
    }
}

#[derive(Default)]
struct MemorySink(Mutex<Vec<AgentMessage>>);

#[async_trait]
impl latent_core::SessionSink for MemorySink {
    async fn append(&self, message: &AgentMessage) -> Result<(), String> {
        self.0.lock().unwrap().push(message.clone());
        Ok(())
    }
}

async fn build_session(
    turns: Vec<ScriptedTurn>,
    tools: Vec<Arc<dyn Tool>>,
    active: Option<Vec<String>>,
    system_prompt: SystemPromptOptions,
) -> (
    latent_core::AgentSession,
    Arc<EventLog>,
    Arc<MemorySink>,
    Arc<ScriptedProvider>,
) {
    let m = model();
    let provider = Arc::new(ScriptedProvider::new(&m, turns));
    let sink = Arc::new(MemorySink::default());
    let session = create_agent_session(AgentSessionConfig {
        provider: provider.clone(),
        model: m,
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions: latent_core::ExtensionRegistry::default(),
        tools,
        active_tool_names: active,
        system_prompt,
        limits: latent_agent::TurnLimits::default(),
        stream_options: Default::default(),
        subscribers: None,
        session_sink: Some(sink.clone()),
        seed_messages: Vec::new(),
        compactor: None,
        permission: None,
        skills: vec![],
    })
    .await
    .unwrap();
    let log = Arc::new(EventLog::default());
    session.subscribe(log.clone());
    (session, log, sink, provider)
}

#[tokio::test]
async fn prompt_streams_through_session_events_and_persists() {
    let m = model();
    let (session, log, sink, _provider) = build_session(
        vec![ScriptedTurn::text(&m, "你好呀")],
        vec![],
        None,
        SystemPromptOptions {
            cwd: Some("/tmp/proj".into()),
            ..Default::default()
        },
    )
    .await;

    let outcome = session.prompt("hello").await.unwrap();
    assert!(
        matches!(outcome, latent_core::PromptOutcome::Started(_)),
        "非流式 prompt 应返回 Started"
    );
    assert_eq!(outcome.stop(), latent_agent::RunStop::EndTurn);
    session.wait_idle().await;

    // 事件层:delta + settled
    let events = log.events();
    assert!(events.iter().any(|e| e == "delta:你好呀"), "{events:?}");
    assert!(events.iter().any(|e| e == "settled"));

    // 持久化:user + assistant
    let persisted = sink.0.lock().unwrap();
    assert_eq!(persisted.len(), 2);
    assert!(matches!(persisted[0], AgentMessage::User { .. }));
    assert_eq!(
        persisted[1].as_assistant().unwrap().text_content(),
        "你好呀"
    );
}

#[tokio::test]
async fn steer_and_follow_up_emit_queue_updates() {
    let m = model();
    let (session, log, _sink, _provider) = build_session(
        vec![ScriptedTurn::text(&m, "回复")],
        vec![],
        None,
        SystemPromptOptions::default(),
    )
    .await;

    session.steer("插话").await;
    session.follow_up("稍后跟进").await;
    session.wait_idle().await;
    let events = log.events();
    assert!(events.contains(&"queue:1/0".to_string()), "{events:?}");
    assert!(events.contains(&"queue:1/1".to_string()), "{events:?}");
}

#[tokio::test]
async fn active_tools_filter_and_transcript_stays_state_only() {
    let m = model();
    let read = Arc::new(TestTool {
        name: "read".into(),
        calls: AtomicU32::new(0),
    });
    let bash = Arc::new(TestTool {
        name: "bash".into(),
        calls: AtomicU32::new(0),
    });
    let (session, _log, sink, _provider) = build_session(
        vec![
            ScriptedTurn::tool_calls(&m, vec![tool_call("t1", "read")]),
            ScriptedTurn::text(&m, "done"),
        ],
        vec![read.clone(), bash.clone()],
        Some(vec!["read".into()]),
        SystemPromptOptions::default(),
    )
    .await;

    session.prompt("list files").await.unwrap();
    assert_eq!(read.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(bash.calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    // 转录纯净:持久化的只有状态类消息(user/assistant/toolResult),
    // 没有工具 schema 声明等能力规则文本
    let persisted = sink.0.lock().unwrap();
    assert!(
        persisted.iter().all(|m| matches!(
            m,
            AgentMessage::User { .. }
                | AgentMessage::Assistant(_)
                | AgentMessage::ToolResult { .. }
        )),
        "session 只允许状态类消息: {persisted:?}"
    );
}

#[tokio::test]
async fn set_active_tools_updates_tools_without_transcript_noise() {
    let m = model();
    let read = Arc::new(TestTool {
        name: "read".into(),
        calls: AtomicU32::new(0),
    });
    let bash = Arc::new(TestTool {
        name: "bash".into(),
        calls: AtomicU32::new(0),
    });
    let (session, _log, sink, _provider) = build_session(
        vec![ScriptedTurn::text(&m, "ok")],
        vec![read, bash],
        None,
        SystemPromptOptions::default(),
    )
    .await;

    // 切到只有 read:更新不报错,下次 prompt 生效
    session
        .set_active_tools_by_name(&["read".into()])
        .await
        .unwrap();
    // 未知工具报错
    assert!(session.set_active_tools_by_name(&["nope".into()]).await.is_err());
    // 工具集切换不产生任何转录消息(纯元数据,由装配方落 tool_set_change entry)
    assert!(sink.0.lock().unwrap().is_empty());
    session.prompt("q").await.unwrap();
    assert_eq!(session.agent().state_snapshot().tool_count, 1);
}

/// overflow 恢复:错误 assistant 是 context overflow → 丢被中断 turn、裁最老
/// 上下文、continue 重放,每次 run 只尝试一次。
#[tokio::test]
async fn overflow_recovery_trims_and_retries() {
    let m = model();
    let mut m2 = model();
    m2.context_window = 100; // 小窗口,让溢出判定可触发
    let read = Arc::new(TestTool {
        name: "read".into(),
        calls: AtomicU32::new(0),
    });
    // 第一轮带工具调用:循环在工具结果后继续,才会到达 overflow 错误轮
    let provider = Arc::new(ScriptedProvider::new(
        &m2,
        vec![
            ScriptedTurn::tool_calls(&m, vec![tool_call("t1", "read")]),
            // 第二轮:overflow 错误
            ScriptedTurn::error(&m, "prompt is too long: 2000 tokens > 100 maximum"),
            // 恢复轮:成功
            ScriptedTurn::text(&m, "恢复成功"),
        ],
    ));
    let sink = Arc::new(MemorySink::default());
    let session = create_agent_session(AgentSessionConfig {
        provider,
        model: m2.clone(),
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions: latent_core::ExtensionRegistry::default(),
        tools: vec![read],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: latent_agent::TurnLimits::default(),
        stream_options: Default::default(),
        subscribers: None,
        session_sink: Some(sink.clone()),
        seed_messages: Vec::new(),
        compactor: None,
        permission: None,
        skills: vec![],
    })
    .await
    .unwrap();
    let log = Arc::new(EventLog::default());
    session.subscribe(log.clone());

    let stop = session.prompt("开始").await.unwrap().stop();
    assert_eq!(
        stop,
        latent_agent::RunStop::EndTurn,
        "overflow 恢复后应正常结束"
    );

    let events = log.events();
    assert!(events.iter().any(|e| e == "retry_start"), "{events:?}");
    assert!(events.iter().any(|e| e == "retry_end"), "{events:?}");

    // 恢复轮的回复进了转录
    let messages = session.agent().messages();
    let last = messages.last().unwrap().as_assistant().unwrap();
    assert_eq!(last.text_content(), "恢复成功");
}

/// 核心扩展注册:工具经 ExtensionApi 注册进装配(接缝 #4)。
#[tokio::test]
async fn extension_registered_tool_joins_session() {
    struct ExtTool;
    #[async_trait]
    impl Tool for ExtTool {
        fn name(&self) -> &str {
            "ext_tool"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _call: ToolCall,
            _cancel: CancellationToken,
            _updater: &dyn ToolUpdater,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("from extension"))
        }
    }
    struct Ext;
    #[async_trait]
    impl latent_core::Extension for Ext {
        fn name(&self) -> &str {
            "ext"
        }
        async fn init(&self, api: &mut latent_core::ExtensionApi<'_>) -> Result<(), CoreError> {
            api.register_tool(Arc::new(ExtTool));
            Ok(())
        }
    }

    let m = model();
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![ScriptedTurn::text(&m, "ok")],
    ));
    let mut extensions = latent_core::ExtensionRegistry::default();
    extensions.register(Arc::new(Ext));
    let session = create_agent_session(AgentSessionConfig {
        provider,
        model: m,
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions,
        tools: vec![],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: latent_agent::TurnLimits::default(),
        stream_options: Default::default(),
        subscribers: None,
        session_sink: None,
        seed_messages: Vec::new(),
        compactor: None,
        permission: None,
        skills: vec![],
    })
    .await
    .unwrap();
    assert_eq!(session.agent().state_snapshot().tool_count, 1);
}

/// T8:流式中 prompt 返回 Enqueued(不再复用 RunStop::EndTurn 谎报),
/// 空闲时返回 Started 携带 run 终止原因。
#[tokio::test]
async fn prompt_returns_enqueued_while_streaming_and_started_when_idle() {
    let m = model();
    // 延迟 turn:保证第一个 prompt 还在流式中,第二个 prompt 能命中入队路径
    let (session, _log, _sink, _provider) = build_session(
        vec![
            ScriptedTurn::text(&m, "第一轮").with_delay(200),
            ScriptedTurn::text(&m, "第二轮"),
        ],
        vec![],
        None,
        SystemPromptOptions::default(),
    )
    .await;

    let session = Arc::new(session);
    let first = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("开始").await }
    });
    // 等 run 真正进入流式
    while !session.agent().is_streaming() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let second = session.prompt("中途插入").await.unwrap();
    assert!(
        matches!(second, latent_core::PromptOutcome::Enqueued),
        "流式中应返回 Enqueued: {second:?}"
    );
    assert_eq!(
        session.queue_depths(),
        (1, 0),
        "入队消息应计入 steering 深度"
    );

    let first = first.await.unwrap().unwrap();
    assert!(matches!(first, latent_core::PromptOutcome::Started(_)));
    session.wait_idle().await;
}

/// overflow 恢复走统一 compaction(06 文档):装配了 compactor 时不再内存砍半,
/// compactor 被调用一次 → 返回压缩后上下文 → set_messages → continue 重放。
#[tokio::test]
async fn overflow_recovery_uses_unified_compactor() {
    struct MockCompactor(AtomicU32);
    #[async_trait]
    impl latent_core::ContextCompactor for MockCompactor {
        async fn compact(&self, _model: &Model) -> Result<Vec<AgentMessage>, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![AgentMessage::user("精简后的上下文")])
        }
    }

    let m = model();
    let mut m2 = model();
    m2.context_window = 100;
    let read = Arc::new(TestTool {
        name: "read".into(),
        calls: AtomicU32::new(0),
    });
    let provider = Arc::new(ScriptedProvider::new(
        &m2,
        vec![
            ScriptedTurn::tool_calls(&m, vec![tool_call("t1", "read")]),
            ScriptedTurn::error(&m, "prompt is too long: 2000 tokens > 100 maximum"),
            ScriptedTurn::text(&m, "恢复成功"),
        ],
    ));
    let compactor = Arc::new(MockCompactor(AtomicU32::new(0)));
    let session = create_agent_session(AgentSessionConfig {
        provider,
        model: m2.clone(),
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions: latent_core::ExtensionRegistry::default(),
        tools: vec![read],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: latent_agent::TurnLimits::default(),
        stream_options: Default::default(),
        session_sink: None,
        seed_messages: Vec::new(),
        compactor: Some(compactor.clone()),
        subscribers: None,
        permission: None,
        skills: vec![],
    })
    .await
    .unwrap();

    let stop = session.prompt("开始").await.unwrap().stop();
    assert_eq!(
        stop,
        latent_agent::RunStop::EndTurn,
        "统一 compaction 恢复后应正常结束"
    );
    assert_eq!(
        compactor.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "compactor 恰好调用一次"
    );

    // 压缩后上下文回填成功:恢复轮回复进了转录
    let messages = session.agent().messages();
    let last = messages.last().unwrap().as_assistant().unwrap();
    assert_eq!(last.text_content(), "恢复成功");
}

/// 自动压缩(06 文档 §3.1):run 成功结束后 should_auto_compact 为真 →
/// 阈值触发主动压缩,set_messages 回填,下一次 prompt 从压缩后上下文开始。
#[tokio::test]
async fn auto_compact_triggers_at_threshold_after_run() {
    struct ThresholdCompactor(AtomicU32);
    #[async_trait]
    impl latent_core::ContextCompactor for ThresholdCompactor {
        async fn compact(&self, _model: &Model) -> Result<Vec<AgentMessage>, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![AgentMessage::user("压缩后的上下文")])
        }
        fn should_auto_compact(&self, _model: &Model, _messages: &[AgentMessage]) -> bool {
            true
        }
    }

    let m = model();
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::text(&m, "第一轮回复"),
            ScriptedTurn::text(&m, "第二轮回复"),
        ],
    ));
    let compactor = Arc::new(ThresholdCompactor(AtomicU32::new(0)));
    let session = create_agent_session(AgentSessionConfig {
        provider,
        model: m.clone(),
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions: latent_core::ExtensionRegistry::default(),
        tools: vec![],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: latent_agent::TurnLimits::default(),
        stream_options: Default::default(),
        session_sink: None,
        seed_messages: Vec::new(),
        compactor: Some(compactor.clone()),
        subscribers: None,
        permission: None,
        skills: vec![],
    })
    .await
    .unwrap();

    let stop = session.prompt("第一问").await.unwrap().stop();
    assert_eq!(stop, latent_agent::RunStop::EndTurn);
    assert_eq!(
        compactor.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "run 结束后应触发一次自动压缩"
    );
    // 压缩结果已回填:转录被替换为压缩后上下文(最后一条来自 compact 返回)
    let messages = session.agent().messages();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, AgentMessage::User { content, .. } if content == "压缩后的上下文")),
        "自动压缩后上下文应回填: {messages:?}"
    );

    // 第二次成功 run 结束后同样触发(每次 EndTurn 后都检查阈值)
    session.prompt("第二问").await.unwrap();
    assert_eq!(
        compactor.0.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "每次成功 run 后都应检查阈值"
    );
}

// ---- 模式节 append-only(替代旧的"每请求尾插 Developer 消息")----

use latent_core::SessionMode;

/// 模式切换把 ModeSection 消息 append 进转录并落盘;同模式重复切换去重;
/// 切换到新模式再追加一条(旧节点保留,entering/exiting 成对自描述)。
#[tokio::test]
async fn set_mode_appends_mode_section_and_dedupes() {
    let m = model();
    let (session, _log, sink, _provider) = build_session(
        vec![ScriptedTurn::text(&m, "回复")],
        vec![],
        None,
        SystemPromptOptions::default(),
    )
    .await;

    session.set_mode(SessionMode::Plan).await.unwrap();
    session.set_mode(SessionMode::Plan).await.unwrap();
    let messages = session.agent().messages();
    assert_eq!(messages.len(), 1, "同模式重复切换只追加一条节点");
    assert!(matches!(&messages[0], AgentMessage::ModeSection { content, .. } if content.contains(PLAN_MODE_ENTER_SECTION)));

    session.set_mode(SessionMode::FullAccess).await.unwrap();
    let messages = session.agent().messages();
    assert_eq!(messages.len(), 2, "新模式节点追加,旧节点保留");
    assert!(matches!(&messages[1], AgentMessage::ModeSection { content, .. } if content.contains(PLAN_MODE_EXIT_SECTION)));

    // 持久化:sink 收到两条 ModeSection(mode_change entry 走 trait 默认空实现)
    let persisted = sink.0.lock().unwrap();
    assert_eq!(persisted.len(), 2);
    assert!(persisted.iter().all(|m| matches!(m, AgentMessage::ModeSection { .. })));
}

/// 挂起式 provider:stream 等门放行,制造"流式中"窗口。
struct GatedProvider {
    model: Model,
    rx: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl latent_ai::Provider for GatedProvider {
    async fn stream(
        &self,
        _model: &Model,
        _ctx: latent_ai::TranscriptContext,
        _opts: latent_ai::StreamOptions,
    ) -> latent_ai::AssistantMessageEventStream {
        let rx = self.rx.lock().unwrap().take();
        let model = self.model.clone();
        Box::pin(async_stream::stream! {
            if let Some(rx) = rx {
                let _ = rx.await;
            }
            yield latent_ai::AssistantMessageEvent::Done(Box::new(latent_ai::assistant_message(
                &model,
                vec![latent_ai::ContentBlock::text("done")],
                latent_ai::StopReason::Stop,
            )));
        })
    }
}

/// 流式期间 set_mode:引擎已切档但模式节 append 被跳过(agent busy);
/// turn 正常结束后按当前模式补追加 —— 模型在下一 turn 能看到退出提示词。
#[tokio::test]
async fn mode_switch_while_streaming_heals_section_at_turn_end() {
    let m = model();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let provider = Arc::new(GatedProvider {
        model: m.clone(),
        rx: Mutex::new(Some(rx)),
    });
    let session = Arc::new(
        create_agent_session(AgentSessionConfig {
            provider,
            model: m,
            hooks: Arc::new(PassthroughHooks),
            ui: Arc::new(NoopUi),
            extensions: latent_core::ExtensionRegistry::default(),
            tools: vec![],
            active_tool_names: None,
            system_prompt: SystemPromptOptions::default(),
            limits: latent_agent::TurnLimits::default(),
            stream_options: Default::default(),
            subscribers: None,
            session_sink: None,
            seed_messages: Vec::new(),
            compactor: None,
            permission: None,
            skills: vec![],
        })
        .await
        .unwrap(),
    );

    session.set_mode(SessionMode::Plan).await.unwrap();
    let runner = {
        let session = session.clone();
        tokio::spawn(async move { session.prompt("hi").await })
    };
    for _ in 0..500 {
        if session.agent().is_streaming() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(session.agent().is_streaming(), "流应已开始");

    // 流式中切档:节点 append 被跳过(stderr 提示),引擎已切档
    session.set_mode(SessionMode::FullAccess).await.unwrap();
    assert_eq!(session.mode(), SessionMode::FullAccess);
    tx.send(()).unwrap();
    runner.await.unwrap().unwrap();

    // turn 结束后补追加:最后一条 ModeSection = 新模式的退出提示词
    let last = session
        .agent()
        .messages()
        .into_iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::ModeSection { content, .. } => Some(content),
            _ => None,
        })
        .expect("转录应含模式节");
    assert!(
        last.contains(PLAN_MODE_EXIT_SECTION),
        "turn 结束应补齐退出 Plan 的模式节,实际 {last}"
    );
}

/// 压缩会吞掉切点之前的模式节点:compact 后若转录中无当前模式节点则补追加。
#[tokio::test]
async fn compact_reappends_current_mode_section() {
    struct DropAllCompactor;
    #[async_trait]
    impl latent_core::ContextCompactor for DropAllCompactor {
        async fn compact(&self, _model: &Model) -> Result<Vec<AgentMessage>, String> {
            Ok(vec![AgentMessage::user("压缩后的上下文")])
        }
    }

    let m = model();
    let provider = Arc::new(ScriptedProvider::new(&m, vec![ScriptedTurn::text(&m, "回复")]));
    let session = create_agent_session(AgentSessionConfig {
        provider,
        model: m.clone(),
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions: latent_core::ExtensionRegistry::default(),
        tools: vec![],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: latent_agent::TurnLimits::default(),
        stream_options: Default::default(),
        session_sink: None,
        seed_messages: Vec::new(),
        compactor: Some(Arc::new(DropAllCompactor)),
        subscribers: None,
        permission: None,
        skills: vec![],
    })
    .await
    .unwrap();

    session.set_mode(SessionMode::Plan).await.unwrap();
    session.compact().await.unwrap();
    let messages = session.agent().messages();
    assert_eq!(
        messages.len(),
        2,
        "压缩投影 + 补追加的当前模式节点,实际 {messages:?}"
    );
    assert!(matches!(&messages[0], AgentMessage::User { .. }));
    assert!(matches!(
        &messages[1],
        AgentMessage::ModeSection { content, .. } if content.contains(PLAN_MODE_ENTER_SECTION)
    ));
}

/// resume 老会话文件(无 ModeSection 消息,只有 mode_change entry):恢复后
/// 按当前模式补一条节点 —— 老格式会话文件的向后兼容路径。
#[tokio::test]
async fn resume_without_mode_section_appends_node() {
    let m = model();
    let provider = Arc::new(ScriptedProvider::new(&m, Vec::new()));
    let session = create_agent_session(AgentSessionConfig {
        provider,
        model: m.clone(),
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions: latent_core::ExtensionRegistry::default(),
        tools: vec![],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: latent_agent::TurnLimits::default(),
        stream_options: Default::default(),
        session_sink: None,
        // 老格式转录:只有对话消息,没有模式节
        seed_messages: vec![AgentMessage::user("之前的问题")],
        compactor: None,
        subscribers: None,
        permission: None,
        skills: vec![],
    })
    .await
    .unwrap();

    session.apply_mode_without_persist(SessionMode::Plan).await.unwrap();
    let messages = session.agent().messages();
    assert_eq!(messages.len(), 2, "历史 + 补追加的模式节点");
    assert!(matches!(&messages[1], AgentMessage::ModeSection { content, .. } if content.contains(PLAN_MODE_ENTER_SECTION)));
}

/// run 内自动压缩(06 文档 §3.1 工具边界触发点):模型连续调工具、run 未结束
/// 时,工具结果回到模型前按阈值压缩;压缩后转录被采纳(run 继续到正常结束),
/// Agent 状态同步(下次 prompt 从压缩后上下文开始)。
#[tokio::test]
async fn auto_compact_fires_mid_run_at_tool_boundary() {
    use std::sync::atomic::AtomicBool;

    struct OnceCompactor {
        armed: AtomicBool,
        calls: AtomicU32,
    }
    #[async_trait]
    impl latent_core::ContextCompactor for OnceCompactor {
        async fn compact(&self, _model: &Model) -> Result<Vec<AgentMessage>, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.armed.store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![AgentMessage::user("压缩后的上下文")])
        }
        fn should_auto_compact(&self, _model: &Model, _messages: &[AgentMessage]) -> bool {
            self.armed.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    let m = model();
    let tool = Arc::new(TestTool {
        name: "test_tool".into(),
        calls: AtomicU32::new(0),
    });
    let compactor = Arc::new(OnceCompactor {
        armed: AtomicBool::new(true),
        calls: AtomicU32::new(0),
    });
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(&m, vec![tool_call("t1", "test_tool")]),
            ScriptedTurn::text(&m, "第二轮回复"),
        ],
    ));
    let session = create_agent_session(AgentSessionConfig {
        provider,
        model: m.clone(),
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions: latent_core::ExtensionRegistry::default(),
        tools: vec![tool],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: latent_agent::TurnLimits::default(),
        stream_options: Default::default(),
        session_sink: None,
        seed_messages: Vec::new(),
        compactor: Some(compactor.clone()),
        subscribers: None,
        permission: None,
        skills: vec![],
    })
    .await
    .unwrap();

    let stop = session.prompt("第一问").await.unwrap().stop();
    assert_eq!(stop, latent_agent::RunStop::EndTurn);
    // 压缩恰好一次且发生在 run 中途:EndTurn 后阈值已撤销(armed=false),不再触发
    assert_eq!(
        compactor.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "工具结果回到模型时触发一次,EndTurn 后不再重复触发"
    );
    // 压缩后转录被采纳且 run 继续完成:压缩摘要在先,第二轮回复在后
    let messages = session.agent().messages();
    assert_eq!(messages.len(), 2);
    assert!(
        matches!(&messages[0], AgentMessage::User { content, .. } if content == "压缩后的上下文"),
        "Agent 状态应同步为压缩后转录"
    );
    let last = messages.last().unwrap().as_assistant().unwrap();
    assert_eq!(last.text_content(), "第二轮回复");
}

// ---- 工具调用护栏仅 Plan 模式生效(SessionCompactionHooks 门控) ----

/// 带 PermissionEngine 的护栏测试会话:12 轮工具调用 + 1 轮文本作答,
/// max_tool_calls = 2(硬停阈值 = 2 + GRACE 10 = 12)。
async fn build_guard_session(mode: SessionMode) -> (latent_core::AgentSession, Arc<TestTool>) {
    let m = model();
    let turns: Vec<ScriptedTurn> = (0..12)
        .map(|i| ScriptedTurn::tool_calls(&m, vec![tool_call(&format!("t{i}"), "counter")]))
        .chain(std::iter::once(ScriptedTurn::text(&m, "完成")))
        .collect();
    let provider = Arc::new(ScriptedProvider::new(&m, turns));
    let tool = Arc::new(TestTool {
        name: "counter".into(),
        calls: AtomicU32::new(0),
    });
    let engine = Arc::new(PermissionEngine::new(
        mode,
        SandboxConfig::default(),
        ApprovalRules::default(),
        std::env::temp_dir(),
        true,
    ));
    let session = create_agent_session(AgentSessionConfig {
        provider,
        model: m,
        hooks: Arc::new(PassthroughHooks),
        ui: Arc::new(NoopUi),
        extensions: latent_core::ExtensionRegistry::default(),
        tools: vec![tool.clone()],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: TurnLimits {
            max_tool_calls: Some(2),
            ..Default::default()
        },
        stream_options: Default::default(),
        subscribers: None,
        session_sink: None,
        seed_messages: Vec::new(),
        compactor: None,
        permission: Some(engine),
        skills: vec![],
    })
    .await
    .unwrap();
    (session, tool)
}

#[tokio::test]
async fn tool_call_guard_hard_stops_in_plan_mode() {
    let (session, tool) = build_guard_session(SessionMode::Plan).await;
    let outcome = session.prompt("hi").await.unwrap();
    assert_eq!(
        outcome.stop(),
        RunStop::BudgetExhausted(BudgetKind::MaxToolCalls),
        "Plan 模式下达 12 次工具调用(2 + GRACE 10)应硬停"
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 12);

    // 收敛提示恰好一条(初始 prompt + 提示),终止通知为最后一条消息
    let messages = session.agent().messages();
    let user_count = messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::User { .. }))
        .count();
    assert_eq!(user_count, 2, "初始 prompt + 恰好一条收敛提示");
    let last = messages.last().unwrap().as_assistant().unwrap();
    assert!(
        last.text_content()
            .contains("maximum number of consecutive tool calls"),
        "终止通知应说明护栏触发"
    );
}

#[tokio::test]
async fn tool_call_guard_inactive_in_confirm_mode() {
    let (session, tool) = build_guard_session(SessionMode::Confirm).await;
    let outcome = session.prompt("hi").await.unwrap();
    assert_eq!(
        outcome.stop(),
        RunStop::EndTurn,
        "非 Plan 模式不设工具调用上限,跑满脚本正常结束"
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 12);

    // 无收敛提示、无终止通知
    let messages = session.agent().messages();
    let user_count = messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::User { .. }))
        .count();
    assert_eq!(user_count, 1, "不应注入收敛提示");
    assert!(
        !messages.iter().any(|m| m
            .as_assistant()
            .map(|a| a
                .text_content()
                .contains("maximum number of consecutive tool calls"))
            .unwrap_or(false)),
        "不应合成终止通知"
    );
}
