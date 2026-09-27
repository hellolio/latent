//! 循环与 Agent 的集成测试(03 文档不变量 I1-I6 与 §8 Agent 语义)。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rpi_agent::{
    create_agent, AgentEvent, AgentMessage, LoopHooks, PassthroughHooks, QueueMode,
    SharedSubscriber, Subscriber, Tool, ToolCall, ToolError, ToolExecution, ToolOutput,
    ToolUpdater,
};
use rpi_ai::{ContentBlock, Model, ScriptedProvider, ScriptedTurn, Tool as DeclaredTool};
use tokio_util::sync::CancellationToken;

fn model() -> Model {
    Model::minimal("mock-1", "mock", "mock")
}

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

/// 事件收集订阅者(保序记录事件种类与关键标识)。
#[derive(Default)]
struct Collector {
    events: Mutex<Vec<String>>,
    messages: Mutex<Vec<AgentMessage>>,
}

impl Collector {
    fn record(&self, line: String) {
        self.events.lock().unwrap().push(line);
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl Subscriber for Collector {
    async fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentStart => self.record("agent_start".into()),
            AgentEvent::TurnStart => self.record("turn_start".into()),
            AgentEvent::TurnEnd { .. } => self.record("turn_end".into()),
            AgentEvent::MessageStart { message, .. } => {
                let role = match &**message {
                    AgentMessage::User { .. } => "user",
                    AgentMessage::Assistant(_) => "assistant",
                    AgentMessage::ToolResult { .. } => "toolResult",
                    _ => "system",
                };
                self.record(format!("message_start:{role}"));
            }
            AgentEvent::MessageEnd { message } => {
                self.messages.lock().unwrap().push((**message).clone());
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                ..
            } => {
                self.record(format!("tool_start:{}:{}", tool_name, tool_call_id));
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                self.record(format!("tool_end:{}", tool_call_id));
            }
            AgentEvent::AgentEnd { messages } => {
                self.record(format!("agent_end:{}", messages.len()));
            }
            _ => {}
        }
    }
}

/// 可配置的测试工具:计数调用、可暂停、可声明 terminate。
struct TestTool {
    name: String,
    calls: AtomicU32,
    delay_ms: u64,
    terminate: bool,
    panic: bool,
}

impl TestTool {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(TestTool {
            name: name.to_string(),
            calls: AtomicU32::new(0),
            delay_ms: 0,
            terminate: false,
            panic: false,
        })
    }

    fn with_delay(mut self: Arc<Self>, delay_ms: u64) -> Arc<Self> {
        Arc::get_mut(&mut self).unwrap().delay_ms = delay_ms;
        self
    }

    fn with_terminate(mut self: Arc<Self>) -> Arc<Self> {
        Arc::get_mut(&mut self).unwrap().terminate = true;
        self
    }

    fn count(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Tool for TestTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "required": ["x"], "properties": {"x": {"type": "string"}}})
    }

    async fn execute(
        &self,
        _call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.panic {
            panic!("tool boom");
        }
        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }
        Ok(ToolOutput {
            output: format!("{} done", self.name),
            details: serde_json::Value::Null,
            terminate: self.terminate,
        })
    }
}

async fn run_loop(
    provider: ScriptedProvider,
    prompts: Vec<AgentMessage>,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Arc<dyn LoopHooks>,
) -> rpi_agent::LoopOutput {
    run_loop_full(Arc::new(provider), prompts, tools, hooks).await
}

async fn run_loop_via_arc(
    provider: Arc<ScriptedProvider>,
    prompts: Vec<AgentMessage>,
    tools: Vec<Arc<dyn Tool>>,
) -> rpi_agent::LoopOutput {
    run_loop_full(provider, prompts, tools, Arc::new(PassthroughHooks)).await
}

async fn run_loop_full(
    provider: Arc<ScriptedProvider>,
    prompts: Vec<AgentMessage>,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Arc<dyn LoopHooks>,
) -> rpi_agent::LoopOutput {
    let (sender, receiver) = rpi_agent::create_injection_endpoints();
    let _ = sender;
    run_loop_full_with(receiver, provider, prompts, tools, hooks)
        .await
        .0
}

/// 推送式注入版:接收端随循环进入,返回 (output, receiver) 供队列断言。
async fn run_loop_full_with(
    receiver: rpi_agent::InjectionReceiver,
    provider: Arc<ScriptedProvider>,
    prompts: Vec<AgentMessage>,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Arc<dyn LoopHooks>,
) -> (rpi_agent::LoopOutput, rpi_agent::InjectionReceiver) {
    let model = model();
    let sink: SharedSubscriber = Arc::new(Collector::default());
    rpi_agent::run_agent_loop(
        prompts,
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools,
        },
        hooks,
        rpi_agent::LoopConfig::new(model.clone()),
        provider,
        sink,
        CancellationToken::new(),
        receiver,
    )
    .await
}

async fn run_loop_with(
    receiver: rpi_agent::InjectionReceiver,
    provider: ScriptedProvider,
    prompts: Vec<AgentMessage>,
    tools: Vec<Arc<dyn Tool>>,
) -> rpi_agent::LoopOutput {
    run_loop_full_with(
        receiver,
        Arc::new(provider),
        prompts,
        tools,
        Arc::new(PassthroughHooks),
    )
    .await
    .0
}

fn text_turn(m: &Model, text: &str) -> ScriptedTurn {
    ScriptedTurn::text(m, text)
}

// ---------------------------------------------------------------------------
// 不变量 I5:length 截断防御
// ---------------------------------------------------------------------------

#[tokio::test]
async fn length_truncated_tool_calls_are_rejected_and_resent() {
    let m = model();
    let tool = TestTool::new("read");
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::truncated(
                &m,
                vec![tool_call("t1", "read", serde_json::json!({"x": "1"}))],
            ),
            text_turn(&m, "重发成功"),
        ],
    );
    let output = run_loop(
        provider,
        vec![AgentMessage::user("hi")],
        vec![tool.clone()],
        Arc::new(PassthroughHooks),
    )
    .await;

    // 工具没有真正执行
    assert_eq!(tool.count(), 0);
    // 错误 tool result 提示重发
    let result = output
        .messages
        .iter()
        .find_map(|msg| msg.tool_result_content())
        .expect("应有截断错误 tool result");
    assert!(result.contains("truncated"), "错误文案应提示截断: {result}");
    assert_eq!(output.stop, rpi_agent::RunStop::EndTurn);
    // 转录配对:1 个 toolCall → 1 个 toolResult(I 系列配对不变量)
    let calls = output
        .messages
        .iter()
        .filter(|msg| matches!(msg.as_assistant(), Some(a) if a.has_tool_calls()))
        .count();
    assert_eq!(calls, 1);
}

// ---------------------------------------------------------------------------
// 工具执行:found/missing/validation/before/after
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_execution_happy_path_and_missing_tool() {
    let m = model();
    let tool = TestTool::new("read");
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![
                    tool_call("t1", "read", serde_json::json!({"x": "1"})),
                    tool_call("t2", "missing", serde_json::json!({})),
                ],
            ),
            text_turn(&m, "done"),
        ],
    );
    let output = run_loop(
        provider,
        vec![AgentMessage::user("hi")],
        vec![tool.clone()],
        Arc::new(PassthroughHooks),
    )
    .await;

    assert_eq!(tool.count(), 1);
    let results: Vec<String> = output
        .messages
        .iter()
        .filter_map(|msg| msg.tool_result_content())
        .collect();
    assert_eq!(results, vec!["read done", "Tool not found: missing"]);
}

#[tokio::test]
async fn before_tool_call_blocks_and_after_tool_call_patches() {
    struct BlockingHooks;
    #[async_trait]
    impl LoopHooks for BlockingHooks {
        fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
            PassthroughHooks.convert_to_llm(msgs)
        }
        async fn before_tool_call(
            &self,
            ctx: rpi_agent::ToolCallCtx,
        ) -> Option<rpi_agent::ToolBlock> {
            if ctx.name == "read" {
                Some(rpi_agent::ToolBlock {
                    block: true,
                    reason: "blocked by policy".into(),
                    ..Default::default()
                })
            } else {
                None
            }
        }
        async fn after_tool_call(
            &self,
            _ctx: rpi_agent::ToolResultCtx,
        ) -> Option<rpi_agent::ToolPatch> {
            Some(rpi_agent::ToolPatch {
                output: Some("patched".into()),
                ..Default::default()
            })
        }
    }

    let m = model();
    let tool = TestTool::new("read");
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![tool_call("t1", "read", serde_json::json!({"x": "1"}))],
            ),
            text_turn(&m, "ok"),
        ],
    );
    let output = run_loop(
        provider,
        vec![AgentMessage::user("hi")],
        vec![tool.clone()],
        Arc::new(BlockingHooks),
    )
    .await;
    // 被拦截的工具未执行
    assert_eq!(tool.count(), 0);
    let results: Vec<String> = output
        .messages
        .iter()
        .filter_map(|msg| msg.tool_result_content())
        .collect();
    assert_eq!(results, vec!["blocked by policy"]);
}

/// 改参链(07 §8.6):block=false + args=Some → 工具以改参后的参数执行。
#[tokio::test]
async fn before_tool_call_can_rewrite_args_without_blocking() {
    struct RewriteHooks;
    #[async_trait]
    impl LoopHooks for RewriteHooks {
        fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
            PassthroughHooks.convert_to_llm(msgs)
        }
        async fn before_tool_call(
            &self,
            _ctx: rpi_agent::ToolCallCtx,
        ) -> Option<rpi_agent::ToolBlock> {
            Some(rpi_agent::ToolBlock {
                args: Some(serde_json::json!({"x": "rewritten"})),
                ..Default::default()
            })
        }
    }
    // 记录实际收到参数的工具
    struct RecordingTool(Arc<Mutex<Vec<serde_json::Value>>>);
    #[async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            "read"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "required": ["x"], "properties": {"x": {"type": "string"}}})
        }
        async fn execute(
            &self,
            call: ToolCall,
            _cancel: CancellationToken,
            _updater: &dyn ToolUpdater,
        ) -> Result<ToolOutput, ToolError> {
            self.0.lock().unwrap().push(call.args.clone());
            Ok(ToolOutput::text("ok"))
        }
    }

    let m = model();
    let seen_args = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(RecordingTool(seen_args.clone()));
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![tool_call(
                    "t1",
                    "read",
                    serde_json::json!({"x": "original"}),
                )],
            ),
            text_turn(&m, "done"),
        ],
    );
    run_loop(
        provider,
        vec![AgentMessage::user("hi")],
        vec![tool],
        Arc::new(RewriteHooks),
    )
    .await;
    assert_eq!(
        *seen_args.lock().unwrap(),
        vec![serde_json::json!({"x": "rewritten"})]
    );
}

/// 改参在循环校验之后发生,重新过 schema 校验:非法改参 → 错误结果,不执行。
#[tokio::test]
async fn rewritten_args_are_revalidated() {
    struct BreakHooks;
    #[async_trait]
    impl LoopHooks for BreakHooks {
        fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
            PassthroughHooks.convert_to_llm(msgs)
        }
        async fn before_tool_call(
            &self,
            _ctx: rpi_agent::ToolCallCtx,
        ) -> Option<rpi_agent::ToolBlock> {
            // 改成缺 required 字段的非法参数
            Some(rpi_agent::ToolBlock {
                args: Some(serde_json::json!({})),
                ..Default::default()
            })
        }
    }

    let m = model();
    let tool = TestTool::new("read");
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![tool_call(
                    "t1",
                    "read",
                    serde_json::json!({"x": "original"}),
                )],
            ),
            text_turn(&m, "done"),
        ],
    );
    let output = run_loop(
        provider,
        vec![AgentMessage::user("hi")],
        vec![tool.clone()],
        Arc::new(BreakHooks),
    )
    .await;
    assert_eq!(tool.count(), 0, "非法改参不应执行工具");
    let results: Vec<String> = output
        .messages
        .iter()
        .filter_map(|msg| msg.tool_result_content())
        .collect();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].contains("Invalid arguments"),
        "应为校验错误: {results:?}"
    );
}

// ---------------------------------------------------------------------------
// 并行双语义(不变量 I4):end 事件按完成序、result 消息按源序
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_dual_ordering_end_events_by_completion_messages_by_source() {
    let m = model();
    // fast 无延迟、slow 延迟 60ms;slow 在前(源序)
    let slow = TestTool::new("slow").with_delay(60);
    let fast = TestTool::new("fast");
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![
                    tool_call("t-slow", "slow", serde_json::json!({"x": "1"})),
                    tool_call("t-fast", "fast", serde_json::json!({"x": "2"})),
                ],
            ),
            text_turn(&m, "done"),
        ],
    );

    let collector = Arc::new(Collector::default());
    let (output, _receiver) = rpi_agent::run_agent_loop(
        vec![AgentMessage::user("hi")],
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: vec![slow.clone(), fast.clone()],
        },
        Arc::new(PassthroughHooks),
        rpi_agent::LoopConfig::new(m.clone()),
        Arc::new(provider),
        collector.clone(),
        CancellationToken::new(),
        rpi_agent::create_injection_endpoints().1,
    )
    .await;

    // end 事件按完成序:fast(t-fast)先于 slow(t-slow)
    let events = collector.events();
    let fast_pos = events
        .iter()
        .position(|e| e == "tool_end:t-fast")
        .expect("fast end");
    let slow_pos = events
        .iter()
        .position(|e| e == "tool_end:t-slow")
        .expect("slow end");
    assert!(fast_pos < slow_pos, "end 事件应按完成序: {events:?}");

    // 结果消息按源序:slow 先于 fast
    let results: Vec<String> = output
        .messages
        .iter()
        .filter_map(|msg| msg.tool_result_content())
        .collect();
    assert_eq!(results, vec!["slow done", "fast done"]);
}

// ---------------------------------------------------------------------------
// terminate 提前终止
// ---------------------------------------------------------------------------

#[tokio::test]
async fn terminate_batch_ends_run_without_extra_request() {
    let m = model();
    // 第二个脚本项不该被消费(脚本耗尽会报错)
    let t1 = TestTool::new("t1").with_terminate();
    let t2 = TestTool::new("t2").with_terminate();
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![
                    tool_call("a", "t1", serde_json::json!({"x": "1"})),
                    tool_call("b", "t2", serde_json::json!({"x": "2"})),
                ],
            ),
            ScriptedTurn::error(&m, "不应再请求"),
        ],
    );
    let provider = Arc::new(provider);
    let output = run_loop_via_arc(
        provider.clone(),
        vec![AgentMessage::user("hi")],
        vec![t1, t2],
    )
    .await;
    assert_eq!(output.stop, rpi_agent::RunStop::EndTurn);
    assert_eq!(provider.remaining(), 1, "terminate 后不应再请求 provider");
}

// ---------------------------------------------------------------------------
// steering / follow-up / 硬退出不碰队列(不变量 I3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steering_injected_at_turn_boundary_and_follow_up_wakes_stopped_agent() {
    let m = model();
    let provider = ScriptedProvider::new(
        &m,
        vec![
            text_turn(&m, "一"),
            text_turn(&m, "二"),
            text_turn(&m, "三"),
        ],
    );
    // 推送式注入(03 §10.5):循环开始前 push steering;follow-up 在停止点整流
    let (sender, receiver) = rpi_agent::create_injection_endpoints();
    sender.steer(AgentMessage::user("先说这个"));
    sender.follow_up(AgentMessage::user("继续"));

    let output = run_loop_with(receiver, provider, Vec::new(), vec![]).await;
    assert_eq!(output.stop, rpi_agent::RunStop::EndTurn);
    // 消费了 3 个脚本 turn:初始 + steering 注入轮 + follow-up 唤醒轮
    let user_texts: Vec<&str> = output
        .messages
        .iter()
        .filter_map(|msg| match msg {
            AgentMessage::User { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(user_texts, vec!["先说这个", "继续"]);
}

/// 推送式注入(03 §10.5,T5 验收):流式期间 push 的 steering 在下个 turn
/// 边界生效 —— 轮询制下这是做不到的。
#[tokio::test]
async fn steering_pushed_mid_stream_is_injected_at_turn_boundary() {
    let m = model();
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::text(&m, "slow first").with_delay(80),
            text_turn(&m, "second"),
        ],
    ));
    let (sender, receiver) = rpi_agent::create_injection_endpoints();
    let sink: SharedSubscriber = Arc::new(Collector::default());
    let handle = tokio::spawn({
        let provider = provider.clone();
        async move {
            rpi_agent::run_agent_loop(
                vec![AgentMessage::user("hi")],
                rpi_agent::AgentContext {
                    system: None,
                    messages: Vec::new(),
                    tools: vec![],
                },
                Arc::new(PassthroughHooks),
                rpi_agent::LoopConfig::new(model()),
                provider,
                sink,
                CancellationToken::new(),
                receiver,
            )
            .await
            .0
        }
    });
    // turn 1 流式期间(80ms 延迟内)推送 steering
    tokio::time::sleep(Duration::from_millis(20)).await;
    sender.steer(AgentMessage::user("插话"));
    let output = handle.await.unwrap();
    let user_texts: Vec<&str> = output
        .messages
        .iter()
        .filter_map(|msg| match msg {
            AgentMessage::User { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_texts,
        vec!["hi", "插话"],
        "流式期间的 steering 应在下个 turn 注入"
    );
    assert_eq!(output.stop, rpi_agent::RunStop::EndTurn);
}

/// P1-1 回归:流式期间已消费的 steering,在硬退出时经 requeued 交还宿主
/// (不静默丢失,深度镜像不泄漏)。
#[tokio::test]
async fn steering_consumed_mid_stream_is_requeued_on_hard_exit() {
    let m = model();
    // turn 1 流式挂住后以 error 终态收尾
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![ScriptedTurn::error(&m, "boom 503").with_delay(80)],
    ));
    let (sender, receiver) = rpi_agent::create_injection_endpoints();
    let sink: SharedSubscriber = Arc::new(Collector::default());
    let handle = tokio::spawn({
        let provider = provider.clone();
        async move {
            rpi_agent::run_agent_loop(
                vec![AgentMessage::user("hi")],
                rpi_agent::AgentContext {
                    system: None,
                    messages: Vec::new(),
                    tools: vec![],
                },
                Arc::new(PassthroughHooks),
                rpi_agent::LoopConfig::new(model()),
                provider,
                sink,
                CancellationToken::new(),
                receiver,
            )
            .await
        }
    });
    // 流式期间 push steering:被 select! 消费进 deferred,随后流 error 硬退出
    tokio::time::sleep(Duration::from_millis(20)).await;
    sender.steer(AgentMessage::user("不该丢"));
    let (output, receiver) = handle.await.unwrap();
    assert_eq!(output.stop, rpi_agent::RunStop::Error("boom 503".into()));
    assert_eq!(
        output.requeued_steering.len(),
        1,
        "已消费未注入的 steering 应交还宿主"
    );
    assert!(
        !output
            .messages
            .iter()
            .any(|msg| matches!(msg, AgentMessage::User { content, .. } if content == "不该丢")),
        "该消息不应被注入"
    );
    assert_eq!(receiver.depths().0, 1, "深度计数不应泄漏");
}

#[tokio::test]
async fn provider_error_hard_exits_without_touching_queues() {
    let m = model();
    let provider = ScriptedProvider::new(&m, vec![ScriptedTurn::error(&m, "boom 503")]);
    // one-at-a-time:初始边界只取一条注入;其余留在通道里(硬退出不消费,I3)
    let (sender, receiver) = rpi_agent::create_injection_endpoints();
    sender.steer(AgentMessage::user("不该被消费"));
    sender.steer(AgentMessage::user("也不该被消费"));

    let (output, receiver) = run_loop_full_with(
        receiver,
        Arc::new(provider),
        vec![AgentMessage::user("hi")],
        vec![],
        Arc::new(PassthroughHooks),
    )
    .await;
    match &output.stop {
        rpi_agent::RunStop::Error(message) => assert!(message.contains("503")),
        other => panic!("expected error stop, got {other:?}"),
    }
    // error 硬退出后不消费任何注入通道(I3):第二条仍在通道里
    let (steering_depth, follow_up_depth) = receiver.depths();
    assert_eq!(steering_depth, 1, "剩余 steering 不应被硬退出消费");
    assert_eq!(follow_up_depth, 0, "follow-up 不应被碰");
    assert!(
        !output.messages.iter().any(
            |msg| matches!(msg, AgentMessage::User { content, .. } if content == "也不该被消费")
        ),
        "第二条 steering 不应被注入"
    );
    assert!(output
        .messages
        .iter()
        .any(|msg| matches!(msg, AgentMessage::User { content, .. } if content == "不该被消费")));
    // 错误 assistant 消息已进转录(错误编码进流)
    let last = output.messages.last().expect("messages");
    let assistant = last.as_assistant().expect("last is assistant");
    assert_eq!(assistant.stop_reason, rpi_ai::StopReason::Error);
    assert!(assistant.error_message.as_deref().unwrap().contains("503"));
}

// ---------------------------------------------------------------------------
// 护栏 TurnLimits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_turns_budget_stops_infinite_tool_loop() {
    let m = model();
    let tool = TestTool::new("loop");
    // 模型永远调用工具;护栏必须在脚本耗尽前停止
    let provider = ScriptedProvider::new(
        &m,
        (0..50)
            .map(|i| {
                ScriptedTurn::tool_calls(
                    &m,
                    vec![tool_call(
                        format!("t{i}").leak(),
                        "loop",
                        serde_json::json!({"x": "1"}),
                    )],
                )
            })
            .collect(),
    );
    let config_limits = rpi_agent::TurnLimits {
        max_turns: Some(3),
        ..Default::default()
    };
    let model2 = model();
    let (output, _receiver) = rpi_agent::run_agent_loop(
        vec![AgentMessage::user("hi")],
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: vec![tool],
        },
        Arc::new(PassthroughHooks),
        rpi_agent::LoopConfig {
            limits: config_limits,
            ..rpi_agent::LoopConfig::new(model2)
        },
        Arc::new(provider),
        Arc::new(Collector::default()),
        CancellationToken::new(),
        rpi_agent::create_injection_endpoints().1,
    )
    .await;
    assert_eq!(
        output.stop,
        rpi_agent::RunStop::BudgetExhausted(rpi_agent::BudgetKind::MaxTurns)
    );
}

// ---------------------------------------------------------------------------
// declareToolChanges(不变量 I6,端到端)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_changes_declared_into_transcript_on_injection() {
    let m = model();
    struct LoopTool(String);
    #[async_trait]
    impl Tool for LoopTool {
        fn name(&self) -> &str {
            &self.0
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
            Ok(ToolOutput::text("ok"))
        }
    }
    let provider = ScriptedProvider::new(&m, vec![text_turn(&m, "ok")]);
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(LoopTool("read".into())),
        Arc::new(LoopTool("bash".into())),
    ];
    let collector = Arc::new(Collector::default());
    let (output, _receiver) = rpi_agent::run_agent_loop(
        vec![AgentMessage::user("hi")],
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: tools.clone(),
        },
        Arc::new(PassthroughHooks),
        rpi_agent::LoopConfig::new(m.clone()),
        Arc::new(provider),
        collector.clone(),
        CancellationToken::new(),
        rpi_agent::create_injection_endpoints().1,
    )
    .await;

    // 注入路径上声明了工具集增量:user 之前出现 system 消息,重放 == 可执行集
    let injected_system = output
        .messages
        .iter()
        .find_map(|msg| match msg {
            AgentMessage::System { tools_added, .. } if !tools_added.is_empty() => Some(
                tools_added
                    .iter()
                    .map(|t| t.name.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .expect("应有工具声明 system 消息");
    let mut expected: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    let mut actual = injected_system.clone();
    expected.sort();
    actual.sort();
    assert_eq!(actual, expected);
}

// ---------------------------------------------------------------------------
// Agent 类:reducer / 队列模式 / continue / reset / abort
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_reducer_builds_transcript_from_events() {
    let m = model();
    let tool = TestTool::new("read");
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![tool_call("t1", "read", serde_json::json!({"x": "1"}))],
            ),
            text_turn(&m, "finished"),
        ],
    ));
    let agent = create_agent(provider.clone(), Arc::new(PassthroughHooks));
    let collector = Arc::new(Collector::default());
    agent.subscribe(collector.clone());
    agent.set_model(m);
    agent.install_tools(vec![tool]);

    let stop = agent.prompt("hello").await.unwrap();
    assert_eq!(stop, rpi_agent::RunStop::EndTurn);

    // 事件驱动的转录:user → assistant(toolUse) → toolResult → assistant(stop)
    let messages = agent.messages();
    assert_eq!(messages.len(), 5);
    // [0] = 工具声明 system 消息(declareToolChanges 注入)
    assert!(matches!(messages[0], AgentMessage::System { .. }));
    assert!(matches!(messages[1], AgentMessage::User { .. }));
    assert!(messages[2].as_assistant().unwrap().has_tool_calls());
    assert!(matches!(messages[3], AgentMessage::ToolResult { .. }));
    assert_eq!(
        messages[4].as_assistant().unwrap().text_content(),
        "finished"
    );
    // 最后一次 agent_end 前 collector 收到的 agent_end 事件计数
    assert_eq!(
        collector
            .events()
            .iter()
            .filter(|e| e.starts_with("agent_end"))
            .count(),
        1
    );
}

#[tokio::test]
async fn agent_one_at_a_time_steering_during_run() {
    let m = model();
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::text(&m, "一").with_delay(30),
            ScriptedTurn::text(&m, "二"),
            ScriptedTurn::text(&m, "三"),
        ],
    ));
    let agent = create_agent(provider.clone(), Arc::new(PassthroughHooks));
    agent.set_model(m);
    agent.set_queue_modes(QueueMode::OneAtATime, QueueMode::OneAtATime);

    let run = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("开始").await }
    });
    // run 进行中 steer 两条:one-at-a-time 每轮只注入一条
    while !agent.is_streaming() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    agent.steer(AgentMessage::user("插一"));
    agent.steer(AgentMessage::user("插二"));
    let stop = run.await.unwrap().unwrap();
    assert_eq!(stop, rpi_agent::RunStop::EndTurn);

    let user_texts: Vec<String> = agent
        .messages()
        .iter()
        .filter_map(|msg| match msg {
            AgentMessage::User { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(user_texts, vec!["开始", "插一", "插二"]);
}

#[tokio::test]
async fn agent_continue_consumes_steering_queue() {
    let m = model();
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![text_turn(&m, "第一次"), text_turn(&m, "续跑")],
    ));
    let agent = create_agent(provider.clone(), Arc::new(PassthroughHooks));
    agent.set_model(m);

    agent.prompt("q1").await.unwrap();
    // 空队列 + 最后是 assistant → 报错
    assert!(matches!(
        agent.continue_run().await,
        Err(rpi_agent::AgentError::NothingToContinue)
    ));
    // steer 后 continue → 队列内容被注入为下一轮 user 消息
    agent.steer(AgentMessage::user("下一题"));
    let stop = agent.continue_run().await.unwrap();
    assert_eq!(stop, rpi_agent::RunStop::EndTurn);
    let user_texts: Vec<String> = agent
        .messages()
        .iter()
        .filter_map(|msg| match msg {
            AgentMessage::User { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(user_texts, vec!["q1", "下一题"]);
    assert_eq!(agent.last_assistant_content(), Some("续跑".to_string()));
}

#[tokio::test]
async fn agent_reset_keeps_baseline_system_message() {
    let m = model();
    let provider = Arc::new(ScriptedProvider::new(&m, vec![text_turn(&m, "hi")]));
    let agent = create_agent(provider, Arc::new(PassthroughHooks));
    agent.set_model(m);
    agent.set_system_prompt(Some("base".into()));
    agent.prompt("q").await.unwrap();
    assert!(agent.messages().len() >= 2);
    agent.reset().unwrap();
    let messages = agent.messages();
    assert_eq!(
        messages.len(),
        0,
        "baseline system prompt 由 state.system 承载,不进消息"
    );
    assert!(!agent.has_queued_messages());
}

#[tokio::test]
async fn agent_abort_produces_aborted_stop() {
    let m = model();
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![ScriptedTurn::text(&m, "很慢的回复").with_delay(10_000)],
    ));
    let agent = create_agent(provider, Arc::new(PassthroughHooks));
    agent.set_model(m);

    let run = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("hi").await }
    });
    while !agent.is_streaming() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    agent.abort();
    let stop = run.await.unwrap().unwrap();
    assert_eq!(stop, rpi_agent::RunStop::Aborted);
    assert!(!agent.is_streaming());
}

#[tokio::test]
async fn concurrent_prompt_rejected() {
    let m = model();
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::text(&m, "a").with_delay(50),
            ScriptedTurn::text(&m, "b"),
        ],
    ));
    let agent = create_agent(provider, Arc::new(PassthroughHooks));
    agent.set_model(m);

    eprintln!("main: before spawn");
    let run = tokio::spawn({
        let agent = agent.clone();
        async move {
            eprintln!("spawned: task started");
            let r = agent.prompt("one").await;
            eprintln!("spawned: prompt returned {r:?}");
            r
        }
    });
    eprintln!("main: spawned");
    let mut seen = false;
    for i in 0..200 {
        if agent.is_streaming() {
            seen = true;
            eprintln!("main: streaming seen at iter {i}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    if !seen {
        eprintln!("main: NEVER saw streaming");
    }
    assert!(matches!(
        agent.prompt("two").await,
        Err(rpi_agent::AgentError::AlreadyRunning)
    ));
    eprintln!("main: second prompt rejected, joining");
    run.await.unwrap().unwrap();
    eprintln!("main: joined");
}

/// I6 的 provider 侧验证:每次请求的工具声明重放 == 可执行集。
#[tokio::test]
async fn provider_sees_tools_equal_to_context_tools() {
    struct RecordingProvider {
        inner: ScriptedProvider,
        seen: Mutex<Vec<Vec<String>>>,
    }
    #[async_trait]
    impl rpi_ai::Provider for RecordingProvider {
        async fn stream(
            &self,
            model: &Model,
            ctx: rpi_ai::TranscriptContext,
            opts: rpi_ai::StreamOptions,
        ) -> rpi_ai::AssistantMessageEventStream {
            let tools = rpi_ai::get_current_tools(&ctx.messages);
            self.seen
                .lock()
                .unwrap()
                .push(tools.into_iter().map(|t| t.name).collect());
            self.inner.stream(model, ctx, opts).await
        }
    }

    struct LoopTool;
    #[async_trait]
    impl Tool for LoopTool {
        fn name(&self) -> &str {
            "read"
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
            Ok(ToolOutput::text("ok"))
        }
    }

    let m = model();
    let inner = ScriptedProvider::new(&m, vec![text_turn(&m, "ok")]);
    let provider = Arc::new(RecordingProvider {
        inner,
        seen: Mutex::new(Vec::new()),
    });
    let sink: SharedSubscriber = Arc::new(Collector::default());
    rpi_agent::run_agent_loop(
        vec![AgentMessage::user("hi")],
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: vec![Arc::new(LoopTool) as Arc<dyn Tool>],
        },
        Arc::new(PassthroughHooks),
        rpi_agent::LoopConfig::new(m.clone()),
        provider.clone(),
        sink,
        CancellationToken::new(),
        rpi_agent::create_injection_endpoints().1,
    )
    .await;
    assert_eq!(
        *provider.seen.lock().unwrap(),
        vec![vec!["read".to_string()]]
    );

    // 未使用的导入防抖:DeclaredTool 仅用于类型对齐
    let _ = std::any::type_name::<DeclaredTool>();
}

// ---------------------------------------------------------------------------
// 串行批(execution_mode=Serial / hooks.tool_execution=Serial)
// ---------------------------------------------------------------------------

struct SerialHooks;
#[async_trait]
impl LoopHooks for SerialHooks {
    fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
        PassthroughHooks.convert_to_llm(msgs)
    }
    fn tool_execution(&self) -> ToolExecution {
        ToolExecution::Serial
    }
}

#[tokio::test]
async fn serial_batch_interleaves_events_and_stays_paired() {
    let m = model();
    let slow = TestTool::new("slow").with_delay(40);
    let fast = TestTool::new("fast");
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![
                    tool_call("t-slow", "slow", serde_json::json!({"x": "1"})),
                    tool_call("t-fast", "fast", serde_json::json!({"x": "2"})),
                ],
            ),
            text_turn(&m, "done"),
        ],
    );
    let collector = Arc::new(Collector::default());
    let (output, _receiver) = rpi_agent::run_agent_loop(
        vec![AgentMessage::user("hi")],
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: vec![slow.clone(), fast.clone()],
        },
        Arc::new(SerialHooks),
        rpi_agent::LoopConfig::new(m.clone()),
        Arc::new(provider),
        collector.clone(),
        CancellationToken::new(),
        rpi_agent::create_injection_endpoints().1,
    )
    .await;

    // 串行:slow 完整先于 fast(start→end→消息 交错)
    let events = collector.events();
    let slow_end = events.iter().position(|e| e == "tool_end:t-slow").unwrap();
    let fast_start = events
        .iter()
        .position(|e| e == "tool_start:fast:t-fast")
        .unwrap();
    assert!(slow_end < fast_start, "串行批应逐个执行: {events:?}");
    // 配对:2 个 toolCall → 2 个 toolResult,顺序 = 源序
    let results: Vec<String> = output
        .messages
        .iter()
        .filter_map(|msg| msg.tool_result_content())
        .collect();
    assert_eq!(results, vec!["slow done", "fast done"]);
}

#[tokio::test]
async fn serial_batch_cancel_midway_still_pairs_all_results() {
    let m = model();
    // 第一个工具执行中取消;第二个工具应得到 "Operation aborted" 结果(配对修复)
    struct CancelOnFirstHooks {
        cancel: CancellationToken,
    }
    #[async_trait]
    impl LoopHooks for CancelOnFirstHooks {
        fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
            PassthroughHooks.convert_to_llm(msgs)
        }
        async fn after_tool_call(
            &self,
            _ctx: rpi_agent::ToolResultCtx,
        ) -> Option<rpi_agent::ToolPatch> {
            self.cancel.cancel();
            None
        }
        fn tool_execution(&self) -> ToolExecution {
            ToolExecution::Serial
        }
    }
    let cancel = CancellationToken::new();
    let t1 = TestTool::new("t1");
    let t2 = TestTool::new("t2");
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![
                    tool_call("a", "t1", serde_json::json!({"x": "1"})),
                    tool_call("b", "t2", serde_json::json!({"x": "2"})),
                ],
            ),
            ScriptedTurn::error(&m, "不应再请求"),
        ],
    );
    let (output, _receiver) = rpi_agent::run_agent_loop(
        vec![AgentMessage::user("hi")],
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: vec![t1, t2],
        },
        Arc::new(CancelOnFirstHooks {
            cancel: cancel.clone(),
        }),
        rpi_agent::LoopConfig::new(m.clone()),
        Arc::new(provider),
        Arc::new(Collector::default()),
        cancel,
        rpi_agent::create_injection_endpoints().1,
    )
    .await;
    // abort 后 run 终止
    assert_eq!(output.stop, rpi_agent::RunStop::Aborted);
    // 配对:每个 toolCall 都有 toolResult(第二个 = Operation aborted)
    let results: Vec<String> = output
        .messages
        .iter()
        .filter_map(|msg| msg.tool_result_content())
        .collect();
    assert_eq!(
        results.len(),
        2,
        "串行 abort 后剩余调用也必须有结果消息: {results:?}"
    );
    assert_eq!(results[1], "Operation aborted");
}

#[tokio::test]
async fn tool_panic_becomes_error_result() {
    let m = model();
    struct PanicTool;
    #[async_trait]
    impl Tool for PanicTool {
        fn name(&self) -> &str {
            "panic_tool"
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
            panic!("boom from tool");
        }
    }
    let provider = ScriptedProvider::new(
        &m,
        vec![
            ScriptedTurn::tool_calls(
                &m,
                vec![tool_call("t1", "panic_tool", serde_json::json!({}))],
            ),
            text_turn(&m, "recovered"),
        ],
    );
    let (output, _receiver) = rpi_agent::run_agent_loop(
        vec![AgentMessage::user("hi")],
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: vec![Arc::new(PanicTool)],
        },
        Arc::new(PassthroughHooks),
        rpi_agent::LoopConfig::new(m.clone()),
        Arc::new(provider),
        Arc::new(Collector::default()),
        CancellationToken::new(),
        rpi_agent::create_injection_endpoints().1,
    )
    .await;
    let results: Vec<String> = output
        .messages
        .iter()
        .filter_map(|msg| msg.tool_result_content())
        .collect();
    assert_eq!(results, vec!["tool execution panicked: boom from tool"]);
    assert_eq!(output.stop, rpi_agent::RunStop::EndTurn);
}

#[tokio::test]
async fn max_tool_calls_budget_stops_batch_loops() {
    let m = model();
    let tool = TestTool::new("loop");
    let provider = ScriptedProvider::new(
        &m,
        (0..50)
            .map(|i| {
                ScriptedTurn::tool_calls(
                    &m,
                    vec![tool_call(
                        format!("t{i}").leak(),
                        "loop",
                        serde_json::json!({"x": "1"}),
                    )],
                )
            })
            .collect(),
    );
    let limits = rpi_agent::TurnLimits {
        max_tool_calls: Some(4),
        ..Default::default()
    };
    let (output, _receiver) = rpi_agent::run_agent_loop(
        vec![AgentMessage::user("hi")],
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: vec![tool],
        },
        Arc::new(PassthroughHooks),
        rpi_agent::LoopConfig {
            limits,
            ..rpi_agent::LoopConfig::new(model())
        },
        Arc::new(provider),
        Arc::new(Collector::default()),
        CancellationToken::new(),
        rpi_agent::create_injection_endpoints().1,
    )
    .await;
    assert_eq!(
        output.stop,
        rpi_agent::RunStop::BudgetExhausted(rpi_agent::BudgetKind::MaxToolCalls)
    );
}

#[tokio::test]
async fn max_truncation_retries_budget_stops_oscillation() {
    let m = model();
    let tool = TestTool::new("read");
    // 模型持续以 length 截断 + 工具调用响应
    let provider = ScriptedProvider::new(
        &m,
        (0..50)
            .map(|i| {
                ScriptedTurn::truncated(
                    &m,
                    vec![tool_call(
                        format!("t{i}").leak(),
                        "read",
                        serde_json::json!({"x": "1"}),
                    )],
                )
            })
            .collect(),
    );
    // 默认 max_truncation_retries=3:第 4 次 length 轮后应停止
    let (output, _receiver) = rpi_agent::run_agent_loop(
        vec![AgentMessage::user("hi")],
        rpi_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: vec![tool],
        },
        Arc::new(PassthroughHooks),
        rpi_agent::LoopConfig::new(model()),
        Arc::new(provider),
        Arc::new(Collector::default()),
        CancellationToken::new(),
        rpi_agent::create_injection_endpoints().1,
    )
    .await;
    assert_eq!(
        output.stop,
        rpi_agent::RunStop::BudgetExhausted(rpi_agent::BudgetKind::TruncationRetries)
    );
}

/// T8:wait_idle 经 watch 等待(去 10ms 轮询):流式中阻塞,run 结束后立即返回。
#[tokio::test]
async fn wait_idle_blocks_until_run_finishes_without_polling() {
    let m = model();
    let provider = Arc::new(ScriptedProvider::new(
        &m,
        vec![ScriptedTurn::text(&m, "慢回复").with_delay(100)],
    ));
    let agent = create_agent(provider.clone(), Arc::new(PassthroughHooks));
    agent.set_model(m);

    let run = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("开始").await }
    });
    while !agent.is_streaming() {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    let waiter = tokio::spawn({
        let agent = agent.clone();
        async move { agent.wait_idle().await }
    });
    // 流式中 wait_idle 尚未返回
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!waiter.is_finished(), "流式中 wait_idle 应阻塞");

    run.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("run 结束后 wait_idle 应及时返回")
        .unwrap();
    assert!(!agent.is_streaming());
}

/// trim_oldest_messages 只保留末尾 keep_last 条:打头 system baseline 永不删;
/// 恰好多出 1 条(唯一可删的是 baseline)时保持原样。
#[test]
fn trim_oldest_never_removes_system_baseline() {
    let provider = Arc::new(ScriptedProvider::new(&model(), vec![]));
    let agent = create_agent(provider, Arc::new(PassthroughHooks));
    agent.set_system_prompt(Some("baseline".into()));
    agent
        .set_messages(vec![
            AgentMessage::System { content: "baseline".into(), sections: Default::default(), tools_added: Vec::new(), tools_removed: Vec::new(), timestamp: 0 },
            AgentMessage::user("m1"),
            AgentMessage::user("m2"),
            AgentMessage::user("m3"),
            AgentMessage::user("m4"),
        ])
        .unwrap();

    // 5 条,keep_last=3:可删 m1、m2,删除后 baseline 仍打头
    agent.trim_oldest_messages(3);
    let messages = agent.messages();
    assert!(
        matches!(messages.first(), Some(AgentMessage::System { .. })),
        "baseline 必须保留: {messages:?}"
    );
    assert_eq!(messages.len(), 4, "baseline + 末尾 keep_last 条");

    // 边界:恰好多出 1 条(唯一可删的就是 baseline)→ 不做任何删除
    agent.trim_oldest_messages(3);
    assert_eq!(agent.messages().len(), 4);
}
