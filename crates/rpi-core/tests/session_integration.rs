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
            AgentSessionEvent::Agent(agent_event) => if let AgentEvent::MessageDelta { delta } = agent_event { self.record(format!("delta:{delta}")) },
            AgentSessionEvent::AgentSettled => self.record("settled".into()),
            AgentSessionEvent::QueueUpdate { steering, follow_up } => {
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
) -> (rpi_core::AgentSession, Arc<EventLog>, Arc<MemorySink>, Arc<ScriptedProvider>) {
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
        SystemPromptOptions { cwd: Some("/tmp/proj".into()), ..Default::default() },
    )
    .await;

    let stop = session.prompt("hello").await.unwrap();
    assert_eq!(stop, rpi_agent::RunStop::EndTurn);
    session.wait_idle().await;

    // 事件层:delta + settled
    let events = log.events();
    assert!(events.iter().any(|e| e == "delta:你好呀"), "{events:?}");
    assert!(events.iter().any(|e| e == "settled"));

    // 持久化:user + assistant
    let persisted = sink.0.lock().unwrap();
    assert_eq!(persisted.len(), 2);
    assert!(matches!(persisted[0], AgentMessage::User { .. }));
    assert_eq!(persisted[1].as_assistant().unwrap().text_content(), "你好呀");
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
async fn active_tools_filter_and_system_prompt_sections() {
    let m = model();
    let read = Arc::new(TestTool { name: "read".into(), calls: AtomicU32::new(0) });
    let bash = Arc::new(TestTool { name: "bash".into(), calls: AtomicU32::new(0) });
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

    // 工具片段进了系统提示词
    let persisted = sink.0.lock().unwrap();
    assert!(matches!(persisted[0], AgentMessage::System { .. }), "工具声明 system 消息应持久化");
}

#[tokio::test]
async fn set_active_tools_diffs_system_prompt() {
    let m = model();
    let read = Arc::new(TestTool { name: "read".into(), calls: AtomicU32::new(0) });
    let bash = Arc::new(TestTool { name: "bash".into(), calls: AtomicU32::new(0) });
    let (session, _log, _sink, _provider) = build_session(
        vec![ScriptedTurn::text(&m, "ok")],
        vec![read, bash],
        None,
        SystemPromptOptions::default(),
    )
    .await;

    // 切到只有 read:更新不报错,下次 prompt 生效
    session.set_active_tools_by_name(&["read".into()]).unwrap();
    // 未知工具报错
    assert!(session.set_active_tools_by_name(&["nope".into()]).is_err());
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
    let read = Arc::new(TestTool { name: "read".into(), calls: AtomicU32::new(0) });
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
    })
    .await
    .unwrap();
    let log = Arc::new(EventLog::default());
    session.subscribe(log.clone());

    let stop = session.prompt("开始").await.unwrap();
    assert_eq!(stop, rpi_agent::RunStop::EndTurn, "overflow 恢复后应正常结束");

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
    let provider = Arc::new(ScriptedProvider::new(&m, vec![ScriptedTurn::text(&m, "ok")]));
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
    })
    .await
    .unwrap();
    assert_eq!(session.agent().state_snapshot().tool_count, 1);
}
