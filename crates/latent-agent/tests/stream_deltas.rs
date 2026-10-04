//! T2 验收:thinking/toolCall 流式增量(delta 为主)+ partial 快照读口。
//! 事件序满足 start → *_delta → done;增量不改变终态消息内容(终态权威定稿);
//! 快照读口随 delta 原地增长(非每 delta 整份快照)。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use latent_agent::{
    run_agent_loop, AgentEvent, AgentMessage, LoopConfig, MessageDeltaPayload, PassthroughHooks,
    SharedPartial, Subscriber, Tool, ToolCall, ToolError, ToolOutput, ToolUpdater,
};
use latent_ai::{ContentBlock, Model, ScriptedProvider, ScriptedTurn};
use tokio_util::sync::CancellationToken;

fn model() -> Model {
    Model::minimal("mock-1", "mock", "mock")
}

struct Echo;
#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
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

#[derive(Default)]
struct DeltaCollector {
    events: Mutex<Vec<String>>,
    /// 每次 ThinkingDelta 时从 partial 读口取到的 thinking 全文
    thinking_snapshots: Mutex<Vec<String>>,
    /// 每次 ToolCallArgs delta 时读口里的参数 JSON
    args_snapshots: Mutex<Vec<String>>,
    /// MessageEnd 携带的终态 assistant 消息(权威定稿断言用)
    final_messages: Mutex<Vec<latent_ai::AssistantMessage>>,
    partial: Mutex<Option<SharedPartial>>,
}

#[async_trait]
impl Subscriber for DeltaCollector {
    async fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageStart { partial, .. } => {
                *self.partial.lock().unwrap() = partial.clone();
                self.events.lock().unwrap().push("start".into());
            }
            AgentEvent::MessageDelta { delta } => {
                let name = match delta {
                    MessageDeltaPayload::Text { delta } => {
                        self.events.lock().unwrap().push(format!("text:{delta}"));
                        return;
                    }
                    MessageDeltaPayload::Thinking { delta } => {
                        self.events
                            .lock()
                            .unwrap()
                            .push(format!("thinking:{delta}"));
                        "thinking"
                    }
                    MessageDeltaPayload::ToolCallArgs { delta, .. } => {
                        self.events.lock().unwrap().push(format!("args:{delta}"));
                        "args"
                    }
                };
                if let Some(partial) = self.partial.lock().unwrap().as_ref() {
                    let snapshot = partial.read().unwrap();
                    let thinking: String = snapshot
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                            _ => None,
                        })
                        .collect();
                    let args: String = snapshot
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolCall { arguments, .. } => Some(arguments.to_string()),
                            _ => None,
                        })
                        .collect();
                    match name {
                        "thinking" => self.thinking_snapshots.lock().unwrap().push(thinking),
                        "args" => self.args_snapshots.lock().unwrap().push(args),
                        _ => {}
                    }
                }
            }
            AgentEvent::MessageUpdate { .. } => self.events.lock().unwrap().push("update".into()),
            AgentEvent::MessageEnd { message } => {
                if let AgentMessage::Assistant(assistant) = &**message {
                    self.final_messages
                        .lock()
                        .unwrap()
                        .push((**assistant).clone());
                }
                self.events.lock().unwrap().push("end".into());
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn thinking_and_toolcall_args_stream_as_deltas_with_snapshot_port() {
    let m = model();
    // thinking → text → toolcall 的完整流(Stop + toolcall 会执行工具,注册 Echo)
    let turn = ScriptedTurn::new(latent_ai::assistant_message(
        &m,
        vec![
            ContentBlock::Thinking {
                thinking: "思考过程".into(),
                thinking_signature: None,
                redacted: None,
            },
            ContentBlock::text("回答"),
            ContentBlock::ToolCall {
                id: "t1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"x": 1}),
            },
        ],
        latent_ai::StopReason::ToolUse,
    ));
    let provider = ScriptedProvider::new(&m, vec![turn]);

    let collector = Arc::new(DeltaCollector::default());
    let _run = run_agent_loop(
        vec![AgentMessage::user("hi")],
        latent_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: vec![Arc::new(Echo)],
        },
        Arc::new(PassthroughHooks),
        LoopConfig::new(m),
        Arc::new(provider),
        collector.clone() as Arc<dyn latent_agent::Subscriber>,
        CancellationToken::new(),
        latent_agent::create_injection_endpoints().1,
    )
    .await
    .0;

    let events = collector.events.lock().unwrap();
    // 事件序(首个 assistant turn):start → *_delta → update(终态快照) → end
    let think_pos = events
        .iter()
        .position(|e| e.starts_with("thinking:"))
        .expect("thinking delta");
    let start_pos = events[..think_pos]
        .iter()
        .rposition(|e| e == "start")
        .expect("start before delta");
    let update_pos = events[think_pos..]
        .iter()
        .position(|e| e == "update")
        .expect("update")
        + think_pos;
    let end_pos = events[update_pos..]
        .iter()
        .position(|e| e == "end")
        .expect("end")
        + update_pos;
    assert!(
        start_pos < think_pos && think_pos < update_pos && update_pos < end_pos,
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| e == "thinking:思考过程"),
        "thinking 增量应逐块转发: {events:?}"
    );
    assert!(
        events.iter().any(|e| e == "text:回答"),
        "text 增量应转发: {events:?}"
    );
    assert!(
        events.iter().any(|e| e.starts_with("args:")),
        "toolCall 参数增量应转发: {events:?}"
    );

    // 快照读口:thinking 快照随 delta 增长(非整份快照事件,读口逐次变大)
    let thinking = collector.thinking_snapshots.lock().unwrap();
    assert!(!thinking.is_empty());
    assert!(
        thinking.windows(2).all(|w| w[0].len() <= w[1].len()),
        "读口应单调增长: {thinking:?}"
    );
    assert_eq!(thinking.last().unwrap(), "思考过程");

    // toolcall 参数快照逐次可见
    let args = collector.args_snapshots.lock().unwrap();
    assert!(!args.is_empty(), "参数快照应有内容");

    // 终态权威定稿:首个 assistant turn 的 message_end == 脚本内容(增量不改变终态;
    // 后续 turn 是脚本耗尽的 error 终态,不参与断言)
    let final_message = collector
        .final_messages
        .lock()
        .unwrap()
        .iter()
        .find(|message| message.has_tool_calls())
        .cloned()
        .expect("带 toolcall 的终态消息");
    assert_eq!(
        final_message.content.len(),
        3,
        "终态内容应含 thinking+text+toolcall"
    );
    match &final_message.content[0] {
        ContentBlock::Thinking { thinking, .. } => assert_eq!(thinking, "思考过程"),
        other => panic!("expected thinking, got {other:?}"),
    }
    assert_eq!(final_message.content[1], ContentBlock::text("回答"));
    match &final_message.content[2] {
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } => {
            assert_eq!(id, "t1");
            assert_eq!(name, "echo");
            assert_eq!(arguments, &serde_json::json!({"x": 1}));
        }
        other => panic!("expected toolcall, got {other:?}"),
    }
}

/// T4 联动验证:TurnEnd 携带 provider 返回的 usage(UI 据此展示用量)。
#[tokio::test]
async fn turn_end_event_carries_provider_usage() {
    let m = model();
    let mut message =
        latent_ai::assistant_message(&m, vec![ContentBlock::text("hi")], latent_ai::StopReason::Stop);
    message.usage = latent_ai::Usage {
        input: 42,
        output: 7,
        cache_read: 5,
        cache_write: 3,
        cache_write_1h: None,
        reasoning: Some(2),
        total_tokens: 57,
        cost: latent_ai::Cost {
            total: 0.25,
            ..Default::default()
        },
    };
    let provider = ScriptedProvider::new(&m, vec![ScriptedTurn::new(message)]);

    struct UsageCapture(Mutex<Option<latent_ai::Usage>>);
    #[async_trait]
    impl Subscriber for UsageCapture {
        async fn on_event(&self, event: &AgentEvent) {
            if let AgentEvent::TurnEnd { message, .. } = event {
                *self.0.lock().unwrap() = Some(message.usage);
            }
        }
    }
    let capture = Arc::new(UsageCapture(Mutex::new(None)));
    let _run = run_agent_loop(
        vec![AgentMessage::user("hi")],
        latent_agent::AgentContext {
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
        },
        Arc::new(PassthroughHooks),
        LoopConfig::new(m),
        Arc::new(provider),
        capture.clone() as Arc<dyn latent_agent::Subscriber>,
        CancellationToken::new(),
        latent_agent::create_injection_endpoints().1,
    )
    .await
    .0;

    let usage = capture.0.lock().unwrap().expect("TurnEnd 应携带 usage");
    assert_eq!(usage.input, 42);
    assert_eq!(usage.output, 7);
    assert_eq!(usage.cache_read, 5);
    assert_eq!(usage.cache_write, 3);
    assert_eq!(usage.reasoning, Some(2));
    assert_eq!(usage.total_tokens, 57);
    assert_eq!(usage.cost.total, 0.25);
}
