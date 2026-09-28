//! 权限系统集成测试(13 文档 §13 L1):ScriptedProvider 出一次 tool_call,
//! ApprovalHooks 在 Confirm/Plan 模式下的四决策路径、会话缓存与事件广播。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_agent::{
    AgentMessage, PassthroughHooks, Tool, ToolCall, ToolError, ToolOutput, ToolUpdater,
};
use rpi_ai::{Model, ScriptedProvider, ScriptedTurn};
use rpi_core::{
    ApprovalDecision, ApprovalHooks, ApprovalRequest, ApprovalRules, ApprovalUi, PermissionEngine,
    SandboxConfig, SessionMode, SessionSharedSubscriber, SessionSubscriber, ToolRiskClass, Verdict,
};
use tokio_util::sync::CancellationToken;

fn model() -> Model {
    Model::minimal("mock-1", "mock", "mock")
}

struct WriteTool {
    calls: AtomicU32,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
            }
        })
    }
    async fn execute(
        &self,
        _call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("written"))
    }
}

/// 记录审批事件 + 固定决策的 UI(13 文档 §13:四种决策各一例)。
struct MockApprovalUi {
    decision: ApprovalDecision,
    requests: Mutex<Vec<ApprovalRequest>>,
}

#[async_trait]
impl ApprovalUi for MockApprovalUi {
    async fn request_approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        self.requests.lock().unwrap().push(request);
        Some(self.decision)
    }
}

#[derive(Default)]
struct Log {
    events: Mutex<Vec<String>>,
}

#[async_trait]
impl SessionSubscriber for Log {
    async fn on_session_event(&self, event: &rpi_core::AgentSessionEvent) {
        use rpi_core::AgentSessionEvent as E;
        let name = match event {
            E::ApprovalRequested { request } => {
                format!("requested:{}", request.tool_name)
            }
            E::ApprovalResolved { decision, .. } => format!("resolved:{decision:?}"),
            _ => return,
        };
        self.events.lock().unwrap().push(name);
    }
}

/// 装配:Confirm 模式 + 固定决策审批 UI + write 工具。
async fn build_with(
    decision: ApprovalDecision,
) -> (
    Arc<rpi_core::AgentSession>,
    Arc<WriteTool>,
    Arc<MockApprovalUi>,
    Arc<Log>,
) {
    let first = rpi_ai::assistant_message(
        &model(),
        vec![rpi_ai::ContentBlock::ToolCall {
            id: "call-w1".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": "a.txt", "content": "x"}),
        }],
        rpi_ai::StopReason::ToolUse,
    );
    let provider: Arc<dyn rpi_ai::Provider> = Arc::new(ScriptedProvider::new(
        &model(),
        vec![
            ScriptedTurn::new(first),
            ScriptedTurn::text(&model(), "done"),
        ],
    ));
    let tool = Arc::new(WriteTool {
        calls: AtomicU32::new(0),
    });
    let ui = Arc::new(MockApprovalUi {
        decision,
        requests: Mutex::new(Vec::new()),
    });
    let log = Arc::new(Log::default());
    let subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>> =
        Arc::new(Mutex::new(vec![log.clone() as SessionSharedSubscriber]));
    let engine = Arc::new(PermissionEngine::new(
        SessionMode::Confirm,
        SandboxConfig::default(),
        ApprovalRules::default(),
        std::env::temp_dir(),
        true,
    ));
    let hooks: Arc<dyn rpi_agent::LoopHooks> = Arc::new(ApprovalHooks::new(
        Arc::new(PassthroughHooks),
        engine.clone(),
        ui.clone(),
        subscribers.clone(),
    ));
    let session = Arc::new(
        rpi_core::create_agent_session(rpi_core::AgentSessionConfig {
            provider,
            model: model(),
            hooks,
            ui: Arc::new(rpi_core::NoopUi),
            extensions: rpi_core::ExtensionRegistry::default(),
            tools: vec![tool.clone()],
            active_tool_names: None,
            system_prompt: Default::default(),
            limits: Default::default(),
            stream_options: Default::default(),
            session_sink: None,
            seed_messages: Vec::new(),
            compactor: None,
            subscribers: Some(subscribers),
            permission: Some(engine),
            mode_section_cell: None,
        })
        .await
        .unwrap(),
    );
    (session, tool, ui, log)
}

async fn tool_results_of(session: &rpi_core::AgentSession) -> Vec<(String, bool)> {
    session
        .agent()
        .messages()
        .iter()
        .filter_map(|msg| match msg {
            AgentMessage::ToolResult {
                content, is_error, ..
            } => Some((
                content
                    .iter()
                    .filter_map(|block| block.as_text().map(str::to_string))
                    .collect::<Vec<_>>()
                    .join("\n"),
                *is_error,
            )),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn approve_executes_tool() {
    let (session, tool, ui, log) = build_with(ApprovalDecision::Approve).await;
    let _ = session.prompt("write it").await.unwrap();
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1, "批准后工具执行");
    assert_eq!(ui.requests.lock().unwrap().len(), 1);
    let events = log.events.lock().unwrap();
    assert_eq!(events[0], "requested:write");
    assert!(events[1].starts_with("resolved:Approve"));
}

#[tokio::test]
async fn deny_reason_enters_tool_result() {
    let (session, tool, _ui, _log) = build_with(ApprovalDecision::Deny).await;
    let stop = session.prompt("write it").await.unwrap();
    assert!(matches!(stop, rpi_core::PromptOutcome::Started(_)));
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0, "拒绝后工具不执行");
    let results = tool_results_of(&session).await;
    let (text, is_error) = &results[0];
    assert!(*is_error, "拒绝原因应为错误 tool result");
    assert!(text.contains("用户拒绝"), "{text}");
}

#[tokio::test]
async fn approve_for_session_caches_second_call() {
    // 两个 turn 都调 write:第一次 Ask(ApproveForSession),第二次不再 Ask
    let first = rpi_ai::assistant_message(
        &model(),
        vec![rpi_ai::ContentBlock::ToolCall {
            id: "call-w1".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": "a.txt", "content": "x"}),
        }],
        rpi_ai::StopReason::ToolUse,
    );
    let second = rpi_ai::assistant_message(
        &model(),
        vec![rpi_ai::ContentBlock::ToolCall {
            id: "call-w2".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": "a.txt", "content": "y"}),
        }],
        rpi_ai::StopReason::ToolUse,
    );
    let provider: Arc<dyn rpi_ai::Provider> = Arc::new(ScriptedProvider::new(
        &model(),
        vec![
            ScriptedTurn::new(first),
            ScriptedTurn::new(second),
            ScriptedTurn::text(&model(), "done"),
        ],
    ));
    let tool = Arc::new(WriteTool {
        calls: AtomicU32::new(0),
    });
    let ui = Arc::new(MockApprovalUi {
        decision: ApprovalDecision::ApproveForSession,
        requests: Mutex::new(Vec::new()),
    });
    let log = Arc::new(Log::default());
    let subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>> =
        Arc::new(Mutex::new(vec![log.clone() as SessionSharedSubscriber]));
    let engine = Arc::new(PermissionEngine::new(
        SessionMode::Confirm,
        SandboxConfig::default(),
        ApprovalRules::default(),
        std::env::temp_dir(),
        true,
    ));
    let hooks: Arc<dyn rpi_agent::LoopHooks> = Arc::new(ApprovalHooks::new(
        Arc::new(PassthroughHooks),
        engine.clone(),
        ui.clone(),
        subscribers.clone(),
    ));
    let session = Arc::new(
        rpi_core::create_agent_session(rpi_core::AgentSessionConfig {
            provider,
            model: model(),
            hooks,
            ui: Arc::new(rpi_core::NoopUi),
            extensions: rpi_core::ExtensionRegistry::default(),
            tools: vec![tool.clone()],
            active_tool_names: None,
            system_prompt: Default::default(),
            limits: Default::default(),
            stream_options: Default::default(),
            session_sink: None,
            seed_messages: Vec::new(),
            compactor: None,
            subscribers: Some(subscribers),
            permission: Some(engine),
            mode_section_cell: None,
        })
        .await
        .unwrap(),
    );
    session.prompt("write twice").await.unwrap();

    assert_eq!(tool.calls.load(Ordering::SeqCst), 2, "两次都执行");
    assert_eq!(ui.requests.lock().unwrap().len(), 1, "第二次缓存命中不再 Ask");
    let events = log.events.lock().unwrap();
    let requested = events.iter().filter(|e| e.starts_with("requested:")).count();
    assert_eq!(requested, 1, "缓存命中只广播第一次请求");
}

#[tokio::test]
async fn abort_terminates_run() {
    let (session, tool, _ui, log) = build_with(ApprovalDecision::Abort).await;
    let stop = session.prompt("write it").await.unwrap();
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    // 单工具调用批 + terminate = 提前结束 run(RunStop 仍为 EndTurn 语义,
    // UI 凭 ApprovalResolved 事件区分展示)
    assert!(matches!(
        stop,
        rpi_core::PromptOutcome::Started(rpi_agent::RunStop::EndTurn)
    ));
    let events = log.events.lock().unwrap();
    assert!(events.iter().any(|e| e.starts_with("resolved:Abort")));
}

#[tokio::test]
async fn ui_channel_closed_means_deny() {
    struct Closed;
    #[async_trait]
    impl ApprovalUi for Closed {
        async fn request_approval(&self, _request: ApprovalRequest) -> Option<ApprovalDecision> {
            None
        }
    }
    // 复用 Confirm 装配但换 Closed UI:write 直接拒,模型收到错误结果
    let first = rpi_ai::assistant_message(
        &model(),
        vec![rpi_ai::ContentBlock::ToolCall {
            id: "call-w1".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": "a.txt", "content": "x"}),
        }],
        rpi_ai::StopReason::ToolUse,
    );
    let provider: Arc<dyn rpi_ai::Provider> = Arc::new(ScriptedProvider::new(
        &model(),
        vec![
            ScriptedTurn::new(first),
            ScriptedTurn::text(&model(), "ok"),
        ],
    ));
    let tool = Arc::new(WriteTool {
        calls: AtomicU32::new(0),
    });
    let subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>> =
        Arc::new(Mutex::new(Vec::new()));
    let engine = Arc::new(PermissionEngine::new(
        SessionMode::Confirm,
        SandboxConfig::default(),
        ApprovalRules::default(),
        std::env::temp_dir(),
        true,
    ));
    let hooks: Arc<dyn rpi_agent::LoopHooks> = Arc::new(ApprovalHooks::new(
        Arc::new(PassthroughHooks),
        engine.clone(),
        Arc::new(Closed),
        subscribers.clone(),
    ));
    let session = Arc::new(
        rpi_core::create_agent_session(rpi_core::AgentSessionConfig {
            provider,
            model: model(),
            hooks,
            ui: Arc::new(rpi_core::NoopUi),
            extensions: rpi_core::ExtensionRegistry::default(),
            tools: vec![tool.clone()],
            active_tool_names: None,
            system_prompt: Default::default(),
            limits: Default::default(),
            stream_options: Default::default(),
            session_sink: None,
            seed_messages: Vec::new(),
            compactor: None,
            subscribers: Some(subscribers),
            permission: Some(engine),
            mode_section_cell: None,
        })
        .await
        .unwrap(),
    );
    session.prompt("write it").await.unwrap();
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0, "通道关闭 = Deny");
}

#[tokio::test]
async fn plan_mode_denies_write_without_asking() {
    // Plan 模式:write 在引擎层 Deny,不产生任何审批事件
    let first = rpi_ai::assistant_message(
        &model(),
        vec![rpi_ai::ContentBlock::ToolCall {
            id: "call-w1".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": "a.txt", "content": "x"}),
        }],
        rpi_ai::StopReason::ToolUse,
    );
    let provider: Arc<dyn rpi_ai::Provider> = Arc::new(ScriptedProvider::new(
        &model(),
        vec![
            ScriptedTurn::new(first),
            ScriptedTurn::text(&model(), "understood"),
        ],
    ));
    let tool = Arc::new(WriteTool {
        calls: AtomicU32::new(0),
    });
    let ui = Arc::new(MockApprovalUi {
        decision: ApprovalDecision::Approve,
        requests: Mutex::new(Vec::new()),
    });
    let log = Arc::new(Log::default());
    let subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>> =
        Arc::new(Mutex::new(vec![log.clone() as SessionSharedSubscriber]));
    let engine = Arc::new(PermissionEngine::new(
        SessionMode::Plan,
        SandboxConfig::default(),
        ApprovalRules::default(),
        std::env::temp_dir(),
        true,
    ));
    // 引擎层直接校验 Deny(Plan)
    let ctx = rpi_agent::ToolCallCtx {
        tool_call_id: "call-w1".into(),
        name: "write".into(),
        args: serde_json::json!({"path": "a.txt"}),
    };
    assert!(matches!(
        engine.evaluate(&ctx, rpi_core::ToolRiskClass::FileWrite),
        Verdict::Deny(_)
    ));
    let hooks: Arc<dyn rpi_agent::LoopHooks> = Arc::new(ApprovalHooks::new(
        Arc::new(PassthroughHooks),
        engine.clone(),
        ui.clone(),
        subscribers.clone(),
    ));
    let session = Arc::new(
        rpi_core::create_agent_session(rpi_core::AgentSessionConfig {
            provider,
            model: model(),
            hooks,
            ui: Arc::new(rpi_core::NoopUi),
            extensions: rpi_core::ExtensionRegistry::default(),
            tools: vec![tool.clone()],
            active_tool_names: None,
            system_prompt: Default::default(),
            limits: Default::default(),
            stream_options: Default::default(),
            session_sink: None,
            seed_messages: Vec::new(),
            compactor: None,
            subscribers: Some(subscribers),
            permission: Some(engine),
            mode_section_cell: None,
        })
        .await
        .unwrap(),
    );
    session.prompt("write it").await.unwrap();
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert_eq!(ui.requests.lock().unwrap().len(), 0, "Plan 模式不弹审批");
    assert!(log.events.lock().unwrap().is_empty());
}
