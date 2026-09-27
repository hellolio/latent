//! AgentSession 集成测试(04 文档):装配、事件层、steer/followUp、
//! setActiveToolsByName、系统提示词 diff、overflow 恢复、持久化。

use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_agent::{
    AgentEvent, AgentMessage, PassthroughHooks, Tool, ToolCall, ToolError, ToolOutput, ToolUpdater,
};
use rpi_ai::{Model, ScriptedProvider, ScriptedTurn};
use rpi_core::{
    create_agent_session, AgentSessionConfig, AgentSessionEvent, CoreError, NoopUi,
    SessionSubscriber, SystemPromptOptions,
};
use tokio_util::sync::CancellationToken;

fn model() -> Model {
    Model::minimal("mock-1", "mock", "mock")
}

fn tool_call(id: &str, name: &str) -> rpi_ai::ContentBlock {
    rpi_ai::ContentBlock::ToolCall {
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
            AgentSessionEvent::Agent(agent_event) => {
                if let AgentEvent::MessageDelta {
                    delta: rpi_agent::MessageDeltaPayload::Text { delta },
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
impl rpi_core::SessionSink for MemorySink {
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
    rpi_core::AgentSession,
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
        extensions: rpi_core::ExtensionRegistry::default(),
        tools,
        active_tool_names: active,
        system_prompt,
        limits: rpi_agent::TurnLimits::default(),
        stream_options: Default::default(),
        subscribers: None,
        session_sink: Some(sink.clone()),
        seed_messages: Vec::new(),
        compactor: None,
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
        matches!(outcome, rpi_core::PromptOutcome::Started(_)),
        "非流式 prompt 应返回 Started"
    );
    assert_eq!(outcome.stop(), rpi_agent::RunStop::EndTurn);
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
        extensions: rpi_core::ExtensionRegistry::default(),
        tools: vec![read],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: rpi_agent::TurnLimits::default(),
        stream_options: Default::default(),
        subscribers: None,
        session_sink: Some(sink.clone()),
        seed_messages: Vec::new(),
        compactor: None,
    })
    .await
    .unwrap();
    let log = Arc::new(EventLog::default());
    session.subscribe(log.clone());

    let stop = session.prompt("开始").await.unwrap().stop();
    assert_eq!(
        stop,
        rpi_agent::RunStop::EndTurn,
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
    impl rpi_core::Extension for Ext {
        fn name(&self) -> &str {
            "ext"
        }
        async fn init(&self, api: &mut rpi_core::ExtensionApi<'_>) -> Result<(), CoreError> {
            api.register_tool(Arc::new(ExtTool));
            Ok(())
        }
    }

    let m = model();
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![ScriptedTurn::text(&m, "ok")],
    ));
    let mut extensions = rpi_core::ExtensionRegistry::default();
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
        limits: rpi_agent::TurnLimits::default(),
        stream_options: Default::default(),
        subscribers: None,
        session_sink: None,
        seed_messages: Vec::new(),
        compactor: None,
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
        matches!(second, rpi_core::PromptOutcome::Enqueued),
        "流式中应返回 Enqueued: {second:?}"
    );
    assert_eq!(
        session.queue_depths(),
        (1, 0),
        "入队消息应计入 steering 深度"
    );

    let first = first.await.unwrap().unwrap();
    assert!(matches!(first, rpi_core::PromptOutcome::Started(_)));
    session.wait_idle().await;
}

/// overflow 恢复走统一 compaction(06 文档):装配了 compactor 时不再内存砍半,
/// compactor 被调用一次 → 返回压缩后上下文 → set_messages → continue 重放。
#[tokio::test]
async fn overflow_recovery_uses_unified_compactor() {
    struct MockCompactor(AtomicU32);
    #[async_trait]
    impl rpi_core::ContextCompactor for MockCompactor {
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
        extensions: rpi_core::ExtensionRegistry::default(),
        tools: vec![read],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: rpi_agent::TurnLimits::default(),
        stream_options: Default::default(),
        session_sink: None,
        seed_messages: Vec::new(),
        compactor: Some(compactor.clone()),
        subscribers: None,
    })
    .await
    .unwrap();

    let stop = session.prompt("开始").await.unwrap().stop();
    assert_eq!(
        stop,
        rpi_agent::RunStop::EndTurn,
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
    impl rpi_core::ContextCompactor for ThresholdCompactor {
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
        extensions: rpi_core::ExtensionRegistry::default(),
        tools: vec![],
        active_tool_names: None,
        system_prompt: SystemPromptOptions::default(),
        limits: rpi_agent::TurnLimits::default(),
        stream_options: Default::default(),
        session_sink: None,
        seed_messages: Vec::new(),
        compactor: Some(compactor.clone()),
        subscribers: None,
    })
    .await
    .unwrap();

    let stop = session.prompt("第一问").await.unwrap().stop();
    assert_eq!(stop, rpi_agent::RunStop::EndTurn);
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
