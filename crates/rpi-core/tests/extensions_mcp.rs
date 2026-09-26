//! 扩展机制集成测试(07 §8):in-memory transport 起 mock MCP server,锁行为:
//! 订阅过滤/高频 opt-in/block 短路/改参链式/超时 fail-open/closed/断连诊断/
//! McpTool 执行·进度·取消/elicitation 两路/rpi/register 失败跳过。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CustomNotification,
    CustomRequest, ElicitRequestParams, ElicitationSchema, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::{serve_server, RoleServer, RunningService};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use rpi_agent::{LoopHooks, Subscriber, ToolCall};
use rpi_core::extensions::{
    connect_transport, create_diagnostics_sink, DiagnosticsSink, ExtensionEvent, ExtensionHooks,
};
use rpi_core::{McpConnection, NoopUi, SessionSubscriber};

fn tool_call(id: &str, name: &str, args: Value) -> rpi_agent::ToolCallCtx {
    rpi_agent::ToolCallCtx {
        tool_call_id: id.into(),
        name: name.into(),
        args,
    }
}

// ---------------------------------------------------------------------------
// mock MCP server:行为由 MockState 驱动
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockState {
    /// rpi/register 响应(wire 形态)
    registration: Mutex<Value>,
    /// true → rpi/register 返回协议错误(模拟 init 失败)
    register_error: AtomicBool,
    /// 收到的观察类通知(params:{event, payload})
    observation: Mutex<Vec<Value>>,
    /// 收到的决策类请求载荷
    requests: Mutex<Vec<Value>>,
    /// 每事件罐头响应(队列;空则回 {})
    responses: Mutex<HashMap<String, VecDeque<Value>>>,
    /// 每事件响应前延迟(模拟慢 handler)
    delays_ms: Mutex<HashMap<String, u64>>,
    /// call_tool 是否先发 progress 通知
    progress_enabled: AtomicBool,
    /// slow 工具执行延迟
    tool_delay_ms: AtomicU64,
    /// 收到的 notifications/cancelled
    cancelled: Mutex<Vec<Value>>,
}

impl MockState {
    fn with_registration(self, registration: Value) -> Self {
        *self.registration.lock().unwrap() = registration;
        self
    }

    fn set_response(&self, event: &str, values: Vec<Value>) {
        self.responses
            .lock()
            .unwrap()
            .insert(event.to_string(), values.into());
    }

    fn set_delay(&self, event: &str, ms: u64) {
        self.delays_ms.lock().unwrap().insert(event.to_string(), ms);
    }

    fn observation_events(&self) -> Vec<String> {
        self.observation
            .lock()
            .unwrap()
            .iter()
            .filter_map(|params| params.get("event").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    }

    fn request_payloads(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

struct MockHandler {
    state: Arc<MockState>,
}

impl ServerHandler for MockHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mock-extension", "0.1.0"))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let echo_schema: JsonObject = serde_json::from_value(json!({
            "type": "object",
            "required": ["text"],
            "properties": {"text": {"type": "string"}}
        }))
        .unwrap();
        let slow_schema: JsonObject = serde_json::from_value(json!({"type": "object"})).unwrap();
        Ok(ListToolsResult {
            tools: vec![
                Tool::new("echo", "回显 text 参数", echo_schema),
                Tool::new("slow", "慢工具(测试取消)", slow_schema),
            ],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if self.state.progress_enabled.load(Ordering::SeqCst) {
            if let Some(token) = context.meta.get_progress_token() {
                let param = ProgressNotificationParam::new(token, 1.0)
                    .with_total(2.0)
                    .with_message("halfway");
                let _ = context.peer.notify_progress(param).await;
            }
        }
        match request.name.as_ref() {
            "echo" => {
                let text = request
                    .arguments
                    .as_ref()
                    .and_then(|arguments| arguments.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let Some(reason) = text.strip_prefix("err:") {
                    return Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                        ContentBlock::text(reason.to_string()),
                    ])));
                }
                let mut result =
                    CallToolResult::success(vec![ContentBlock::text(format!("echo: {text}"))]);
                result.structured_content = Some(json!({ "echoed": text }));
                Ok(CallToolResponse::Complete(result))
            }
            "slow" => {
                let delay = self.state.tool_delay_ms.load(Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text("slow done"),
                ])))
            }
            other => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(format!("unknown tool: {other}")),
            ]))),
        }
    }

    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _context: rmcp::service::NotificationContext<RoleServer>,
    ) {
        if notification.method == "rpi/event" {
            if let Some(params) = notification.params {
                self.state.observation.lock().unwrap().push(params);
            }
        }
    }

    async fn on_cancelled(
        &self,
        notification: rmcp::model::CancelledNotificationParam,
        _context: rmcp::service::NotificationContext<RoleServer>,
    ) {
        self.state
            .cancelled
            .lock()
            .unwrap()
            .push(json!({ "requestId": notification.request_id }));
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CustomResult, McpError> {
        match request.method.as_str() {
            "rpi/register" => {
                if self.state.register_error.load(Ordering::SeqCst) {
                    return Err(McpError::internal_error("mock register failure", None));
                }
                let registration = self.state.registration.lock().unwrap().clone();
                Ok(rmcp::model::CustomResult::new(
                    json!({ "rpiResult": registration }),
                ))
            }
            "rpi/event" => {
                let payload = request.params.unwrap_or(Value::Null);
                self.state.requests.lock().unwrap().push(payload.clone());
                let event = payload
                    .get("event")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let delay = self
                    .state
                    .delays_ms
                    .lock()
                    .unwrap()
                    .get(&event)
                    .copied()
                    .unwrap_or(0);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                let response = self
                    .state
                    .responses
                    .lock()
                    .unwrap()
                    .get_mut(&event)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or_else(|| json!({}));
                Ok(rmcp::model::CustomResult::new(
                    json!({ "rpiResult": response }),
                ))
            }
            other => Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                other.to_string(),
                None,
            )),
        }
    }
}

/// mock server 句柄:serve_server 在收到 initialize 前会阻塞,必须与 client
/// 并发跑(测试结束直接 abort,不验证优雅关闭)。
struct MockServer {
    task: tokio::task::JoinHandle<
        Result<RunningService<RoleServer, MockHandler>, rmcp::service::ServerInitializeError>,
    >,
}

impl MockServer {
    fn abort(self) {
        self.task.abort();
    }
}

/// in-memory 连接:duplex 两侧分别接 rmcp client(rpi 宿主)与 mock server。
async fn connect_mock(
    name: &str,
    state: Arc<MockState>,
) -> (Arc<McpConnection>, MockServer, DiagnosticsSink) {
    let diagnostics = create_diagnostics_sink();
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let task = tokio::spawn(async move { serve_server(MockHandler { state }, server_io).await });
    let connection = connect_transport(
        name.to_string(),
        client_io,
        Arc::new(NoopUi),
        diagnostics.clone(),
    )
    .await
    .expect("client connect");
    (connection, MockServer { task }, diagnostics)
}

// ---------------------------------------------------------------------------
// 订阅过滤与观察类转发
// ---------------------------------------------------------------------------

#[tokio::test]
async fn observation_events_forwarded_only_to_subscribers() {
    let state = Arc::new(
        MockState::default()
            .with_registration(json!({ "events": { "turn_start": {}, "turn_end": {} } })),
    );
    let (connection, server, _diagnostics) = connect_mock("filter", state.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(vec![connection.clone()], create_diagnostics_sink());

    // 已订阅 → 通知送达
    bus.on_event(&rpi_agent::AgentEvent::TurnStart).await;
    // 未订阅 → 不发
    bus.on_event(&rpi_agent::AgentEvent::AgentStart).await;
    bus.on_event(&rpi_agent::AgentEvent::MessageDelta {
        delta: rpi_agent::MessageDeltaPayload::text("x"),
    })
    .await;
    // 会话级:未订阅
    bus.on_session_event(&rpi_core::AgentSessionEvent::AgentSettled)
        .await;

    // notification 是异步投递,稍等入站循环
    for _ in 0..50 {
        if !state.observation_events().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(state.observation_events(), vec!["turn_start".to_string()]);

    // 决策事件未订阅:不发送请求,聚合结果为空
    let outcome = bus
        .dispatch_tool_call(&tool_call("t1", "bash", json!({"command": "ls"})))
        .await;
    assert!(outcome.tool_block.is_none());
    assert!(state.request_payloads().is_empty());
    connection.terminate();
    server.abort();
}

#[tokio::test]
async fn high_frequency_events_require_opt_in() {
    let opted = Arc::new(MockState::default().with_registration(json!({
        "events": { "message_delta": {} },
        "highFrequency": ["message_delta"],
    })));
    let plain = Arc::new(
        MockState::default().with_registration(json!({ "events": { "message_delta": {} } })),
    );
    let (connection_opted, server_opted, _) = connect_mock("opted", opted.clone()).await;
    let (connection_plain, server_plain, _) = connect_mock("plain", plain.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(
        vec![connection_opted.clone(), connection_plain.clone()],
        create_diagnostics_sink(),
    );

    bus.on_event(&rpi_agent::AgentEvent::MessageDelta {
        delta: rpi_agent::MessageDeltaPayload::text("hi"),
    })
    .await;
    for _ in 0..50 {
        if !opted.observation_events().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        opted.observation_events(),
        vec!["message_delta".to_string()]
    );
    assert!(
        plain.observation_events().is_empty(),
        "未 opt-in 的高频事件不上线缆"
    );

    connection_opted.terminate();
    connection_plain.terminate();
    server_opted.abort();
    server_plain.abort();
}

// ---------------------------------------------------------------------------
// 决策类:block 短路 + 改参链式 + 顺序
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_call_block_short_circuits_remaining_extensions() {
    let blocker = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_call": {} },
    })));
    blocker.set_response(
        "tool_call",
        vec![json!({"block": true, "reason": "no bash"})],
    );

    let observer = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_call": {} },
    })));

    let (connection_a, server_a, _) = connect_mock("a", blocker.clone()).await;
    let (connection_b, server_b, _) = connect_mock("b", observer.clone()).await;
    // 注册顺序:a(blocker)→ b(observer)
    let bus = rpi_core::ExtensionEventBus::new(
        vec![connection_a.clone(), connection_b.clone()],
        create_diagnostics_sink(),
    );

    let outcome = bus
        .dispatch_tool_call(&tool_call("t1", "bash", json!({"command": "ls"})))
        .await;
    let block = outcome.tool_block.expect("应被拦截");
    assert!(block.block);
    assert_eq!(block.reason, "no bash");
    // block 短路:b 未收到请求
    assert!(observer.request_payloads().is_empty());

    connection_a.terminate();
    connection_b.terminate();
    server_a.abort();
    server_b.abort();
}

#[tokio::test]
async fn tool_call_args_chain_across_extensions_in_registration_order() {
    // a:改参 x=a1;b:看到 a1,再改成 b2
    let ext_a = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_call": {} },
    })));
    ext_a.set_response("tool_call", vec![json!({"args": {"x": "a1"}})]);
    let ext_b = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_call": {} },
    })));
    ext_b.set_response("tool_call", vec![json!({"args": {"x": "b2"}})]);

    let (connection_a, server_a, _) = connect_mock("a", ext_a.clone()).await;
    let (connection_b, server_b, _) = connect_mock("b", ext_b.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(
        vec![connection_a.clone(), connection_b.clone()],
        create_diagnostics_sink(),
    );

    let outcome = bus
        .dispatch_tool_call(&tool_call("t1", "read", json!({"x": "original"})))
        .await;
    let block = outcome.tool_block.expect("改参应有聚合结果");
    assert!(!block.block);
    assert_eq!(
        block.args,
        Some(json!({"x": "b2"})),
        "链式传递:最后一写胜出"
    );

    // 改参链:a 收到 original,b 收到 a1(前一扩展输出 = 后一扩展输入)
    let payloads = ext_a.request_payloads();
    assert_eq!(payloads[0]["payload"]["args"], json!({"x": "original"}));
    let payloads = ext_b.request_payloads();
    assert_eq!(payloads[0]["payload"]["args"], json!({"x": "a1"}));

    connection_a.terminate();
    connection_b.terminate();
    server_a.abort();
    server_b.abort();
}

#[tokio::test]
async fn tool_result_patches_merge_last_writer_wins() {
    let ext_a = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_result": {} },
    })));
    ext_a.set_response(
        "tool_result",
        vec![json!({"output": "a-patch", "isError": true})],
    );
    let ext_b = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_result": {} },
    })));
    ext_b.set_response("tool_result", vec![json!({"output": "b-patch"})]);

    let (connection_a, server_a, _) = connect_mock("a", ext_a.clone()).await;
    let (connection_b, server_b, _) = connect_mock("b", ext_b.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(
        vec![connection_a.clone(), connection_b.clone()],
        create_diagnostics_sink(),
    );

    let ctx = rpi_agent::ToolResultCtx {
        tool_call_id: "t1".into(),
        name: "bash".into(),
        output: "original".into(),
        details: Value::Null,
        is_error: false,
        terminate: false,
    };
    let outcome = bus.dispatch_tool_result(&ctx).await;
    assert_eq!(outcome.patch.output.as_deref(), Some("b-patch"));
    assert_eq!(
        outcome.patch.is_error,
        Some(true),
        "b 未覆盖的字段保留 a 的 patch"
    );
    assert_eq!(outcome.patch.terminate, None);

    connection_a.terminate();
    connection_b.terminate();
    server_a.abort();
    server_b.abort();
}

// ---------------------------------------------------------------------------
// 超时 fail-open / fail-closed(07 §8.5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handler_timeout_fails_open_by_default() {
    let ext = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_call": { "timeoutMs": 50 } },
    })));
    ext.set_delay("tool_call", 300);

    let (connection, server, diagnostics) = connect_mock("slow-ext", ext.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(vec![connection.clone()], diagnostics.clone());

    let outcome = bus
        .dispatch_tool_call(&tool_call("t1", "bash", json!({"command": "ls"})))
        .await;
    // fail-open:超时不拦截,继续宿主流程
    assert!(outcome.tool_block.is_none());
    // 诊断已收集,且超时不断连(连接未 stale)
    assert!(!diagnostics.lock().unwrap().is_empty());
    assert!(!connection.is_stale());

    connection.terminate();
    server.abort();
}

#[tokio::test]
async fn handler_timeout_fails_closed_blocks_tool_call() {
    let ext = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_call": { "timeoutMs": 50, "failClosed": true } },
    })));
    ext.set_delay("tool_call", 300);

    let (connection, server, diagnostics) = connect_mock("guard-ext", ext.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(vec![connection.clone()], diagnostics.clone());

    let outcome = bus
        .dispatch_tool_call(&tool_call("t1", "bash", json!({"command": "ls"})))
        .await;
    let block = outcome.tool_block.expect("fail-closed 应拦截");
    assert!(block.block);
    assert!(block.reason.contains("fail-closed"));
    assert!(!diagnostics.lock().unwrap().is_empty());

    connection.terminate();
    server.abort();
}

// ---------------------------------------------------------------------------
// 断连:stale 标记 + 诊断 + 跳过
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disconnect_marks_stale_records_diagnostic_and_skips() {
    let ext = Arc::new(MockState::default().with_registration(json!({
        "events": { "turn_start": {}, "tool_call": {} },
    })));
    let (connection, server, diagnostics) = connect_mock("dead-ext", ext.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(vec![connection.clone()], diagnostics.clone());

    // 杀掉 server 侧:连接断开
    server.abort();
    tokio::time::sleep(Duration::from_millis(50)).await;

    bus.on_event(&rpi_agent::AgentEvent::TurnStart).await;
    assert!(connection.is_stale(), "断连后应标记 stale");
    let recorded = diagnostics.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "断连记一次诊断: {recorded:?}");
    assert_eq!(recorded[0].extension, "dead-ext");

    // 后续分发跳过 stale 连接:不再新增诊断,也不 panic
    bus.on_event(&rpi_agent::AgentEvent::TurnStart).await;
    let outcome = bus
        .dispatch_tool_call(&tool_call("t1", "bash", json!({})))
        .await;
    assert!(outcome.tool_block.is_none());
    assert_eq!(diagnostics.lock().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// McpTool:执行 / 进度 / 取消 / stale
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_tool_executes_with_progress_and_details() {
    let ext = Arc::new(MockState::default().with_registration(json!({ "events": {} })));
    ext.progress_enabled.store(true, Ordering::SeqCst);
    let (connection, server, _diagnostics) = connect_mock("tool-ext", ext.clone()).await;

    let tools = connection.create_tools();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name(), "tool-ext__echo", "工具名带扩展前缀");

    struct RecordingUpdater(Mutex<Vec<String>>);
    #[async_trait]
    impl rpi_agent::ToolUpdater for RecordingUpdater {
        async fn update(&self, partial: String) {
            self.0.lock().unwrap().push(partial);
        }
    }
    let updater = Arc::new(RecordingUpdater(Mutex::new(Vec::new())));

    let output = tools[0]
        .execute(
            ToolCall {
                id: "t1".into(),
                name: "tool-ext__echo".into(),
                args: json!({"text": "hi"}),
            },
            CancellationToken::new(),
            updater.as_ref(),
        )
        .await
        .expect("echo 执行成功");
    assert_eq!(output.output, "echo: hi");
    assert_eq!(
        output.details,
        json!({"echoed": "hi"}),
        "structured content → details"
    );
    assert_eq!(
        *updater.0.lock().unwrap(),
        vec!["halfway".to_string()],
        "progress → ToolUpdater"
    );

    connection.terminate();
    server.abort();
}

#[tokio::test]
async fn mcp_tool_cancel_sends_cancelled_notification() {
    let ext = Arc::new(MockState::default().with_registration(json!({ "events": {} })));
    ext.tool_delay_ms.store(5_000, Ordering::SeqCst);
    let (connection, server, _diagnostics) = connect_mock("cancel-ext", ext.clone()).await;
    let tools = connection.create_tools();
    let slow = &tools[1];

    struct NoopUpdater;
    #[async_trait]
    impl rpi_agent::ToolUpdater for NoopUpdater {
        async fn update(&self, _partial: String) {}
    }
    let cancel = CancellationToken::new();
    let execute = slow.execute(
        ToolCall {
            id: "t1".into(),
            name: "cancel-ext__slow".into(),
            args: json!({}),
        },
        cancel.clone(),
        &NoopUpdater,
    );
    let cancel_task = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    };
    let (result, _) = tokio::join!(execute, cancel_task);
    match result {
        Err(rpi_agent::ToolError::Aborted { name }) => assert_eq!(name, "cancel-ext__slow"),
        other => panic!("应 aborted,得到 {other:?}"),
    }
    // server 侧收到 notifications/cancelled
    for _ in 0..50 {
        if !ext.cancelled.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ext.cancelled.lock().unwrap().len(),
        1,
        "取消应透传 MCP cancelled 通知"
    );

    connection.terminate();
    server.abort();
}

#[tokio::test]
async fn mcp_tool_on_stale_connection_fails_without_panic() {
    let ext = Arc::new(MockState::default().with_registration(json!({ "events": {} })));
    let (connection, server, _diagnostics) = connect_mock("gone-ext", ext.clone()).await;
    let tools = connection.create_tools();
    connection.mark_stale("test stale");
    struct NoopUpdater;
    #[async_trait]
    impl rpi_agent::ToolUpdater for NoopUpdater {
        async fn update(&self, _partial: String) {}
    }
    let result = tools[0]
        .execute(
            ToolCall {
                id: "t1".into(),
                name: "gone-ext__echo".into(),
                args: json!({"text": "x"}),
            },
            CancellationToken::new(),
            &NoopUpdater,
        )
        .await;
    match result {
        Err(rpi_agent::ToolError::Failed { message, .. }) => {
            assert!(message.contains("disconnected"))
        }
        other => panic!("stale 连接应返回 Failed,得到 {other:?}"),
    }
    connection.terminate();
    server.abort();
}

// ---------------------------------------------------------------------------
// elicitation 桥:no-op 与真实现两路(07 §8.6 接缝 #5)
// ---------------------------------------------------------------------------

struct RecordingUi {
    select_choice: Option<usize>,
    input_text: Option<String>,
    confirm_answer: bool,
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl rpi_core::ExtensionUi for RecordingUi {
    async fn notify(&self, message: &str) {
        self.calls.lock().unwrap().push(format!("notify:{message}"));
    }
    async fn confirm(&self, message: &str) -> bool {
        self.calls
            .lock()
            .unwrap()
            .push(format!("confirm:{message}"));
        self.confirm_answer
    }
    async fn select(&self, message: &str, options: &[String]) -> Option<usize> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("select:{message}:{options:?}"));
        self.select_choice
    }
    async fn input(&self, message: &str) -> Option<String> {
        self.calls.lock().unwrap().push(format!("input:{message}"));
        self.input_text.clone()
    }
}

fn bool_elicitation(message: &str) -> ElicitRequestParams {
    ElicitRequestParams::FormElicitationParams {
        meta: None,
        message: message.to_string(),
        requested_schema: ElicitationSchema::builder()
            .required_bool("value")
            .build()
            .expect("schema"),
    }
}

fn enum_elicitation(message: &str, values: Vec<&str>) -> ElicitRequestParams {
    ElicitRequestParams::FormElicitationParams {
        meta: None,
        message: message.to_string(),
        requested_schema: ElicitationSchema::builder()
            .required_enum_schema(
                "value",
                rmcp::model::EnumSchema::builder(values.into_iter().map(str::to_string).collect())
                    .build(),
            )
            .build()
            .expect("schema"),
    }
}

fn string_elicitation(message: &str) -> ElicitRequestParams {
    ElicitRequestParams::FormElicitationParams {
        meta: None,
        message: message.to_string(),
        requested_schema: ElicitationSchema::builder()
            .required_string("value")
            .build()
            .expect("schema"),
    }
}

#[tokio::test]
async fn elicitation_noop_ui_confirm_accepts_select_input_decline() {
    // NoopUi(confirm=true / select=None / input=None):headless 默认语义
    let accept = rpi_core::extensions::bridge_elicitation(&NoopUi, bool_elicitation("确认?"))
        .await
        .unwrap();
    assert_eq!(accept.action, rmcp::model::ElicitationAction::Accept);
    assert_eq!(accept.content, Some(json!({"value": true})));

    let decline =
        rpi_core::extensions::bridge_elicitation(&NoopUi, enum_elicitation("选?", vec!["a", "b"]))
            .await
            .unwrap();
    assert_eq!(decline.action, rmcp::model::ElicitationAction::Decline);

    let decline = rpi_core::extensions::bridge_elicitation(&NoopUi, string_elicitation("填?"))
        .await
        .unwrap();
    assert_eq!(decline.action, rmcp::model::ElicitationAction::Decline);
}

#[tokio::test]
async fn elicitation_real_ui_selects_and_inputs() {
    let ui = RecordingUi {
        select_choice: Some(1),
        input_text: Some("typed".into()),
        confirm_answer: false,
        calls: Mutex::new(Vec::new()),
    };
    let result =
        rpi_core::extensions::bridge_elicitation(&ui, enum_elicitation("选?", vec!["a", "b"]))
            .await
            .unwrap();
    assert_eq!(result.action, rmcp::model::ElicitationAction::Accept);
    assert_eq!(result.content, Some(json!({"value": "b"})));

    let result = rpi_core::extensions::bridge_elicitation(&ui, string_elicitation("填?"))
        .await
        .unwrap();
    assert_eq!(result.content, Some(json!({"value": "typed"})));

    // confirm=false → Decline
    let result = rpi_core::extensions::bridge_elicitation(&ui, bool_elicitation("确认?"))
        .await
        .unwrap();
    assert_eq!(result.action, rmcp::model::ElicitationAction::Decline);
}

// ---------------------------------------------------------------------------
// rpi/register 失败:init 失败 → 跳过(07 §8.5,加载语义与编译期一致)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registration_failure_and_dead_binary_are_skipped_with_diagnostics() {
    // 1) rpi/register 返回错误 → connect 失败
    let bad = Arc::new(MockState::default());
    bad.register_error.store(true, Ordering::SeqCst);
    let diagnostics = create_diagnostics_sink();
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let server = MockServer {
        task: tokio::spawn(
            async move { serve_server(MockHandler { state: bad }, server_io).await },
        ),
    };
    let result = connect_transport(
        "bad".into(),
        client_io,
        Arc::new(NoopUi),
        diagnostics.clone(),
    )
    .await;
    assert!(result.is_err(), "register 失败应连接失败");
    server.abort();

    // 2) 进程不存在 → connect_stdio 失败;create_extension_event_bus 跳过 + 诊断
    let diagnostics = create_diagnostics_sink();
    let (bus, tools) = rpi_core::create_extension_event_bus(
        vec![rpi_core::McpServerSpec {
            name: Some("nope".into()),
            command: "/nonexistent/rpi-ext-xyz".into(),
            args: vec![],
            env: Default::default(),
        }],
        Arc::new(NoopUi),
        diagnostics.clone(),
    )
    .await;
    assert!(bus.is_empty(), "失败的扩展不应进总线");
    assert!(tools.is_empty(), "失败的扩展不留工具");
    let recorded = diagnostics.lock().unwrap().clone();
    assert!(
        recorded.iter().any(|d| d.extension == "nope"),
        "应有诊断: {recorded:?}"
    );
}

// ---------------------------------------------------------------------------
// ExtensionHooks:决策类先问扩展再透传(接缝 #2 包装)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extension_hooks_passes_inner_hooks_with_chained_args_and_inner_can_block() {
    // 扩展改参 → 内层钩子收到改后的参数并可拦截
    let ext = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_call": {} },
    })));
    let inner_seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    ext.set_response("tool_call", vec![json!({"args": {"command": "rewritten"}})]);
    let (connection, server, _diagnostics) = connect_mock("hook-ext", ext.clone()).await;
    let bus = Arc::new(rpi_core::ExtensionEventBus::new(
        vec![connection.clone()],
        create_diagnostics_sink(),
    ));

    struct InnerHooks(Arc<Mutex<Vec<Value>>>);
    #[async_trait]
    impl LoopHooks for InnerHooks {
        fn convert_to_llm(&self, msgs: &[rpi_agent::AgentMessage]) -> Vec<rpi_ai::Message> {
            rpi_agent::PassthroughHooks.convert_to_llm(msgs)
        }
        async fn before_tool_call(
            &self,
            ctx: rpi_agent::ToolCallCtx,
        ) -> Option<rpi_agent::ToolBlock> {
            // 记录内层看到的参数(应为扩展改参后的值)
            self.0.lock().unwrap().push(ctx.args.clone());
            if ctx.args["command"] == "rewritten" {
                return Some(rpi_agent::ToolBlock {
                    block: true,
                    reason: "inner blocked".into(),
                    ..Default::default()
                });
            }
            None
        }
    }

    let hooks = ExtensionHooks::new(Arc::new(InnerHooks(inner_seen.clone())), bus.clone());
    let block = hooks
        .before_tool_call(tool_call("t1", "bash", json!({"command": "original"})))
        .await
        .expect("内层拦截");
    assert_eq!(block.reason, "inner blocked");
    // 内层看到的是扩展改参后的 args
    assert_eq!(
        *inner_seen.lock().unwrap(),
        vec![json!({"command": "rewritten"})]
    );

    // 观察类事件经总线透传(队列注入已改 mpsc 通道,不经过 hooks)

    connection.terminate();
    server.abort();
}

#[tokio::test]
async fn extension_hooks_without_subscribers_skip_wire_entirely() {
    // 无连接的空总线:决策/观察直通内层,不构造载荷(热路径守卫)
    let bus = Arc::new(rpi_core::ExtensionEventBus::new(
        vec![],
        create_diagnostics_sink(),
    ));
    let hooks = ExtensionHooks::new(Arc::new(rpi_agent::PassthroughHooks), bus.clone());
    let msgs = vec![rpi_agent::AgentMessage::user("hi")];
    let transformed = hooks.transform_context(msgs.clone()).await;
    assert_eq!(transformed.len(), 1);
    assert!(hooks
        .before_tool_call(tool_call("t1", "bash", json!({})))
        .await
        .is_none());
    assert!(!bus.has_subscriber(ExtensionEvent::ToolCall));
}

// ---------------------------------------------------------------------------
// reviewer 回归:fail-closed × 断连、畸形响应、决策事件 wire 覆盖
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fail_closed_guard_extension_disconnect_still_blocks_tool_call() {
    let ext = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_call": { "failClosed": true } },
    })));
    let (connection, server, diagnostics) = connect_mock("guard", ext.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(vec![connection.clone()], diagnostics.clone());

    // 守卫扩展进程崩溃(abort 是异步的,stale 在下一次发送时落地)
    server.abort();

    // 死掉的守卫不静默放行(07 §8.5 拒绝动作语义):断连走 Err 路径或
    // stale 检查路径,两者都因 fail-closed 拦截
    let outcome = bus
        .dispatch_tool_call(&tool_call("t1", "bash", json!({"command": "ls"})))
        .await;
    let block = outcome.tool_block.expect("断连的 fail-closed 守卫应拦截");
    assert!(block.block);
    assert!(block.reason.contains("fail-closed"), "{}", block.reason);
    assert!(connection.is_stale(), "断连后应已标记失效");
}

#[tokio::test]
async fn malformed_responses_are_skipped_with_diagnostic() {
    let ext = Arc::new(MockState::default().with_registration(json!({
        "events": { "tool_call": {}, "tool_result": {} },
    })));
    ext.set_response("tool_call", vec![json!({"block": "yes"})]);
    ext.set_response("tool_result", vec![json!({"isError": "certainly"})]);
    let (connection, server, diagnostics) = connect_mock("sloppy", ext.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(vec![connection.clone()], diagnostics.clone());

    let call = bus
        .dispatch_tool_call(&tool_call("t1", "bash", json!({})))
        .await;
    assert!(call.tool_block.is_none(), "畸形响应的扩展贡献被丢弃");
    let ctx = rpi_agent::ToolResultCtx {
        tool_call_id: "t1".into(),
        name: "bash".into(),
        output: "out".into(),
        details: Value::Null,
        is_error: false,
        terminate: false,
    };
    let result = bus.dispatch_tool_result(&ctx).await;
    assert!(result.patch.output.is_none());

    // 畸形响应 = handler 失败:必须留诊断(07 §8.5),不静默降级
    let recorded = diagnostics.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2, "两个畸形响应各记一条: {recorded:?}");
    assert!(recorded
        .iter()
        .all(|d| d.message.contains("invalid response")));

    connection.terminate();
    server.abort();
}

#[tokio::test]
async fn decision_events_context_before_request_turns_and_chaining() {
    // a 改写,b 观察链式输入
    // 注册表 key = 事件 wire 名(snake_case)
    let ext_a = Arc::new(MockState::default().with_registration(json!({
        "events": { "context": {}, "before_request": {}, "prepare_next_turn": {}, "finish_turn": {} },
    })));
    ext_a.set_response("context", vec![json!({"messages": serde_json::to_value(vec![rpi_agent::AgentMessage::user("rewritten")]).unwrap()})]);
    ext_a.set_response("before_request", vec![json!({"thinkingLevel": "high"})]);
    ext_a.set_response("prepare_next_turn", vec![json!({"messages": serde_json::to_value(vec![rpi_agent::AgentMessage::user("injected")]).unwrap()})]);
    ext_a.set_response("finish_turn", vec![json!({"decision": "continue"})]);

    let ext_b = Arc::new(MockState::default().with_registration(json!({
        "events": { "context": {}, "before_request": {}, "prepare_next_turn": {}, "finish_turn": {} },
    })));
    // b 全部回 {}:最终聚合 = a 的输出;b 的请求载荷应含 a 的链式输出
    ext_b.set_response("context", vec![json!({})]);
    ext_b.set_response("before_request", vec![json!({})]);
    ext_b.set_response("prepare_next_turn", vec![json!({})]);
    ext_b.set_response("finish_turn", vec![json!({})]);

    let (connection_a, server_a, _) = connect_mock("a", ext_a.clone()).await;
    let (connection_b, server_b, _) = connect_mock("b", ext_b.clone()).await;
    let bus = rpi_core::ExtensionEventBus::new(
        vec![connection_a.clone(), connection_b.clone()],
        create_diagnostics_sink(),
    );

    // context:改写后的转录
    let outcome = bus
        .dispatch_context(vec![rpi_agent::AgentMessage::user("original")])
        .await;
    let messages = outcome.messages.expect("context 改写");
    assert_eq!(messages.len(), 1);
    assert!(
        matches!(&messages[0], rpi_agent::AgentMessage::User { content, .. } if content == "rewritten")
    );
    // 链式:b 看到改写后的 messages
    let b_payloads = ext_b.request_payloads();
    let context_payload = b_payloads.iter().find(|p| p["event"] == "context").unwrap();
    assert_eq!(
        context_payload["payload"]["messages"][0]["content"],
        "rewritten"
    );

    // before_request:thinkingLevel 聚合 + 链式
    let model = rpi_ai::Model::minimal("m", "mock", "mock");
    let outcome = bus.dispatch_before_request(&model, None).await;
    assert_eq!(
        outcome.request.thinking_level,
        Some(Some(rpi_ai::ThinkingLevel::High))
    );
    let b_payloads = ext_b.request_payloads();
    let request_payload = b_payloads
        .iter()
        .find(|p| p["event"] == "before_request")
        .unwrap();
    assert_eq!(request_payload["payload"]["thinkingLevel"], "high");

    // prepare_next_turn:messages 聚合 + 链式
    let turn_ctx = rpi_agent::TurnCtx {
        message: Box::new(rpi_ai::AssistantMessage::pending(&model)),
        tool_results: vec![],
        new_messages: vec![],
    };
    let outcome = bus
        .dispatch_turn_boundary(ExtensionEvent::PrepareNextTurn, &turn_ctx)
        .await;
    assert_eq!(outcome.turn_update.messages.as_ref().map(Vec::len), Some(1));
    let b_payloads = ext_b.request_payloads();
    let turn_payload = b_payloads
        .iter()
        .find(|p| p["event"] == "prepare_next_turn")
        .unwrap();
    assert_eq!(
        turn_payload["payload"]["messages"][0]["content"],
        "injected"
    );

    // finish_turn:决策聚合 + 链式(后续扩展看到当前决策)
    let outcome = bus
        .dispatch_turn_boundary(ExtensionEvent::FinishTurn, &turn_ctx)
        .await;
    assert_eq!(outcome.decision, Some(rpi_agent::TurnDecision::Continue));
    let b_payloads = ext_b.request_payloads();
    let decision_payload = b_payloads
        .iter()
        .find(|p| p["event"] == "finish_turn")
        .unwrap();
    assert_eq!(decision_payload["payload"]["decision"], "continue");

    connection_a.terminate();
    connection_b.terminate();
    server_a.abort();
    server_b.abort();
}

#[tokio::test]
async fn extension_hooks_transform_context_through_extension() {
    let ext = Arc::new(MockState::default().with_registration(json!({
        "events": { "context": {} },
    })));
    ext.set_response(
        "context",
        vec![json!({"messages": serde_json::to_value(vec![rpi_agent::AgentMessage::user("rewritten")]).unwrap()})],
    );
    let (connection, server, _diagnostics) = connect_mock("ctx-ext", ext.clone()).await;
    let bus = Arc::new(rpi_core::ExtensionEventBus::new(
        vec![connection.clone()],
        create_diagnostics_sink(),
    ));
    let hooks = ExtensionHooks::new(Arc::new(rpi_agent::PassthroughHooks), bus.clone());

    let transformed = hooks
        .transform_context(vec![rpi_agent::AgentMessage::user("original")])
        .await;
    assert!(
        matches!(&transformed[0], rpi_agent::AgentMessage::User { content, .. } if content == "rewritten")
    );

    connection.terminate();
    server.abort();
}

#[tokio::test]
async fn tool_error_result_maps_to_failed_and_disconnect_mid_call_marks_stale() {
    let ext = Arc::new(MockState::default().with_registration(json!({ "events": {} })));
    ext.tool_delay_ms.store(3_000, Ordering::SeqCst);
    let (connection, server, _diagnostics) = connect_mock("err-ext", ext.clone()).await;
    let tools = connection.create_tools();

    struct NoopUpdater;
    #[async_trait]
    impl rpi_agent::ToolUpdater for NoopUpdater {
        async fn update(&self, _partial: String) {}
    }

    // 1) isError=true 的工具结果 → ToolError::Failed(错误编码约定)
    let result = tools[0]
        .execute(
            ToolCall {
                id: "t1".into(),
                name: "err-ext__echo".into(),
                args: json!({"text": "err:bad input"}),
            },
            CancellationToken::new(),
            &NoopUpdater,
        )
        .await;
    match result {
        Err(rpi_agent::ToolError::Failed { message, .. }) => assert_eq!(message, "bad input"),
        other => panic!("isError 结果应转 Failed,得到 {other:?}"),
    }
    assert!(!connection.is_stale(), "工具级错误不应断连");

    // 2) tools/call 进行中断连 → Failed + mark_stale(与事件路径语义一致)
    let execute = tools[1].execute(
        ToolCall {
            id: "t2".into(),
            name: "err-ext__slow".into(),
            args: json!({}),
        },
        CancellationToken::new(),
        &NoopUpdater,
    );
    let killer = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        server.abort();
    };
    let (result, _) = tokio::join!(execute, killer);
    match result {
        Err(rpi_agent::ToolError::Failed { .. }) => {}
        other => panic!("断连应转 Failed,得到 {other:?}"),
    }
    assert!(connection.is_stale(), "tools/call 中断连应标记失效");
}
