//! 07 §8.3 公共分发函数:一条路径覆盖全部埋点。
//!
//! 埋点面 = 两条接缝的现有点位,循环零改动:
//! - **观察类**(通知,不需回应)→ 接缝 #6 `Subscriber`/`SessionSubscriber`,
//!   JSON-RPC notification 发出即走;高频流事件(MessageDelta/MessageUpdate/
//!   ToolExecutionUpdate)默认不上线缆,扩展注册时显式声明才推送;
//! - **决策类**(同步请求-回应)→ 接缝 #2 `LoopHooks` 决策点,按注册顺序串行
//!   await,block 短路,改参链式传递(前一扩展输出 = 后一扩展输入);
//! - **错误隔离**(07 §8.5):超时/断连/错误 → 记 `ExtensionDiagnostic`,不击穿
//!   宿主;fail-open/fail-closed 按该扩展注册时声明(fail-closed 仅对 tool_call
//!   有"拒绝动作"语义 → 拦截执行;其余事件退化为跳过 + 诊断);
//! - 装配在 cli 组合期完成,`Agent`/`AgentSession` 不感知 MCP。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rpi_agent::{
    AgentEvent, AgentMessage, LoopHooks, RequestUpdate, Subscriber, ToolBlock, ToolCallCtx,
    ToolPatch, ToolResultCtx, TurnCtx, TurnDecision, TurnUpdate,
};
use rpi_ai::ThinkingLevel;

use crate::session::{AgentSessionEvent, SessionSubscriber};

use super::mcp_host::McpConnection;

/// rpi/event 决策请求的默认超时(扩展可在注册时逐事件覆盖)。
pub const DEFAULT_EVENT_TIMEOUT_MS: u64 = 5_000;

/// 观察类通知的发送超时(挂在 Subscriber 串行链上,挂起不得击穿宿主)。
const NOTIFICATION_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// 扩展事件全集(07 §8.3):观察类 + 决策类;wire 名 = snake_case
/// (对齐 pi ExtensionEvent 命名)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionEvent {
    // —— 观察类:rpi-agent 循环事件(接缝 #6)——
    AgentStart,
    TurnStart,
    TurnEnd,
    MessageStart,
    MessageDelta,
    MessageUpdate,
    MessageEnd,
    ToolExecutionStart,
    ToolExecutionUpdate,
    ToolExecutionEnd,
    AgentEnd,
    // —— 观察类:session 级事件 ——
    AgentSettled,
    QueueUpdate,
    AutoRetryStart,
    AutoRetryEnd,
    // —— 决策类:LoopHooks 决策点(接缝 #2)——
    ToolCall,
    ToolResult,
    Context,
    BeforeRequest,
    PrepareNextTurn,
    FinishTurn,
}

impl ExtensionEvent {
    pub fn as_str(&self) -> &'static str {
        // serde rename_all 的 wire 名
        match self {
            ExtensionEvent::AgentStart => "agent_start",
            ExtensionEvent::TurnStart => "turn_start",
            ExtensionEvent::TurnEnd => "turn_end",
            ExtensionEvent::MessageStart => "message_start",
            ExtensionEvent::MessageDelta => "message_delta",
            ExtensionEvent::MessageUpdate => "message_update",
            ExtensionEvent::MessageEnd => "message_end",
            ExtensionEvent::ToolExecutionStart => "tool_execution_start",
            ExtensionEvent::ToolExecutionUpdate => "tool_execution_update",
            ExtensionEvent::ToolExecutionEnd => "tool_execution_end",
            ExtensionEvent::AgentEnd => "agent_end",
            ExtensionEvent::AgentSettled => "agent_settled",
            ExtensionEvent::QueueUpdate => "queue_update",
            ExtensionEvent::AutoRetryStart => "auto_retry_start",
            ExtensionEvent::AutoRetryEnd => "auto_retry_end",
            ExtensionEvent::ToolCall => "tool_call",
            ExtensionEvent::ToolResult => "tool_result",
            ExtensionEvent::Context => "context",
            ExtensionEvent::BeforeRequest => "before_request",
            ExtensionEvent::PrepareNextTurn => "prepare_next_turn",
            ExtensionEvent::FinishTurn => "finish_turn",
        }
    }

    /// 观察类通知的发送超时(挂在 Subscriber 串行链上,挂起不得击穿宿主)。
    /// 决策类事件:同步请求-回应,扩展可干预。
    pub fn is_decision(&self) -> bool {
        matches!(
            self,
            ExtensionEvent::ToolCall
                | ExtensionEvent::ToolResult
                | ExtensionEvent::Context
                | ExtensionEvent::BeforeRequest
                | ExtensionEvent::PrepareNextTurn
                | ExtensionEvent::FinishTurn
        )
    }

    /// 高频流事件:默认不上线缆,注册时显式声明才推送(07 §8.3)。
    pub fn is_high_frequency(&self) -> bool {
        matches!(
            self,
            ExtensionEvent::MessageDelta
                | ExtensionEvent::MessageUpdate
                | ExtensionEvent::ToolExecutionUpdate
        )
    }
}

/// 单事件策略:rpi/register 响应中每事件的超时与 fail 语义(07 §8.3)。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EventPolicy {
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// 安全类扩展应 fail-closed(07 §8.5):tool_call 出错即拦截执行;
    /// 其余事件 fail-closed 退化为"跳过 + 诊断"。
    #[serde(default)]
    pub fail_closed: bool,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_EVENT_TIMEOUT_MS
}

/// 扩展能力声明(rpi/register 响应):订阅事件表 + 每事件超时/fail 语义 +
/// 高频事件 opt-in。Serialize 供测试/mock server 生成同一 wire 形态。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionRegistration {
    /// key = 事件 wire 名(如 "tool_call")
    #[serde(default)]
    pub events: HashMap<String, EventPolicy>,
    /// 默认不上线缆的高频流事件,显式声明才推送
    #[serde(default)]
    pub high_frequency: Vec<ExtensionEvent>,
}

/// 运行期诊断:面向 mode 可见(07 §8.5),cli 打 stderr。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionDiagnostic {
    pub extension: String,
    pub message: String,
}

pub type DiagnosticsSink = Arc<Mutex<Vec<ExtensionDiagnostic>>>;

/// 诊断 sink 工厂(装配方创建,与总线/连接共享同一份)。
pub fn create_diagnostics_sink() -> DiagnosticsSink {
    Arc::new(Mutex::new(Vec::new()))
}

/// 运行期诊断可见性(07 §8.5 "诊断面向 mode 可见"):spawn 一个 drain 任务,
/// 把新诊断追加打印到 stderr。mode 可换成自己的消费者(TUI 随 M5)。
pub fn spawn_diagnostics_printer(sink: DiagnosticsSink) {
    tokio::spawn(async move {
        let mut cursor = 0usize;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let entries = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while cursor < entries.len() {
                let diagnostic = &entries[cursor];
                eprintln!(
                    "[rpi][extension:{}] {}",
                    diagnostic.extension, diagnostic.message
                );
                cursor += 1;
            }
        }
    });
}

pub(crate) fn record_diagnostic(sink: &DiagnosticsSink, extension: &str, message: String) {
    // 锁中毒容错:持锁线程 panic 不允许击穿分发层(07 §8.5)
    sink.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(ExtensionDiagnostic {
            extension: extension.to_string(),
            message,
        });
}

/// 决策类分发的聚合结果:各事件只消费自己的字段。
#[derive(Debug, Clone, Default)]
pub struct DecisionOutcome {
    /// tool_call 聚合:block=true 表示被某扩展拦截(短路)或 fail-closed;
    /// args=Some 表示改参链的最终值。
    pub tool_block: Option<ToolBlock>,
    /// tool_result 聚合:逐字段浅覆盖(last-writer)。
    pub patch: ToolPatch,
    /// context 聚合:改写后的转录(None = 无扩展干预)。
    pub messages: Option<Vec<AgentMessage>>,
    /// before_request 聚合。
    pub request: RequestUpdate,
    /// prepare_next_turn 聚合。
    pub turn_update: TurnUpdate,
    /// finish_turn 聚合。
    pub decision: Option<TurnDecision>,
}

pub enum EventOutcome {
    /// 观察类:notification 已发出(或无订阅者)。
    Notified,
    /// 决策类:全部订阅扩展的聚合结果。
    Decision(Box<DecisionOutcome>),
}

/// 公共分发函数宿主:持有按注册顺序排列的扩展连接。
pub struct ExtensionEventBus {
    connections: Vec<Arc<McpConnection>>,
    diagnostics: DiagnosticsSink,
}

impl ExtensionEventBus {
    pub fn new(connections: Vec<Arc<McpConnection>>, diagnostics: DiagnosticsSink) -> Self {
        ExtensionEventBus {
            connections,
            diagnostics,
        }
    }

    pub fn diagnostics(&self) -> &DiagnosticsSink {
        &self.diagnostics
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    pub fn connections(&self) -> &[Arc<McpConnection>] {
        &self.connections
    }

    /// 是否有扩展订阅了该事件(热路径守卫:无订阅者时跳过载荷构造,07 §8.3
    /// 高频事件默认不上线缆的前提)。
    pub fn has_subscriber(&self, event: ExtensionEvent) -> bool {
        self.connections.iter().any(|connection| {
            !connection.is_stale()
                && (connection.registration.events.contains_key(event.as_str())
                    || (event.is_high_frequency()
                        && connection.registration.high_frequency.contains(&event)))
        })
    }

    /// 公共分发函数(核心交付,07 §8.3):订阅过滤 → 串行分发 → 聚合。
    pub async fn dispatch(&self, event: ExtensionEvent, mut payload: Value) -> EventOutcome {
        if !event.is_decision() {
            for connection in &self.connections {
                if connection.is_stale() {
                    continue;
                }
                if !connection.registration.events.contains_key(event.as_str()) {
                    continue;
                }
                if event.is_high_frequency()
                    && !connection.registration.high_frequency.contains(&event)
                {
                    continue;
                }
                // 通知发送也要超时(07 §8 挂起防护):send_notification 会 await
                // transport 写完成,挂起的扩展可让管道写满并永久 pending
                let send = connection.send_event_notification(event, &payload);
                if tokio::time::timeout(NOTIFICATION_SEND_TIMEOUT, send)
                    .await
                    .unwrap_or_else(|_| Err("notification send timed out".into()))
                    .is_err()
                {
                    connection.mark_stale(format!(
                        "event `{}` notification send failed or timed out",
                        event.as_str()
                    ));
                }
            }
            return EventOutcome::Notified;
        }

        let mut outcome = DecisionOutcome::default();
        for connection in &self.connections {
            if connection.is_stale() {
                // fail-closed 的守卫扩展断连后不再静默放行(07 §8.5 拒绝动作语义)
                if event == ExtensionEvent::ToolCall {
                    if let Some(policy) = connection.registration.events.get(event.as_str()) {
                        if policy.fail_closed {
                            outcome.tool_block = Some(ToolBlock {
                                block: true,
                                reason: format!(
                                    "blocked: guard extension `{}` disconnected (fail-closed)",
                                    connection.name()
                                ),
                                ..Default::default()
                            });
                            break;
                        }
                    }
                }
                continue;
            }
            let Some(policy) = connection.registration.events.get(event.as_str()) else {
                continue;
            };
            let timeout = Duration::from_millis(policy.timeout_ms);
            match connection
                .send_event_request(event, &payload, timeout)
                .await
            {
                Ok(response) => {
                    if merge_decision(
                        event,
                        &response,
                        &mut payload,
                        &mut outcome,
                        connection.name(),
                        &self.diagnostics,
                    ) {
                        // block 短路:不再询问后续扩展(07 §8.3)
                        break;
                    }
                }
                Err(message) => {
                    if !connection.is_stale() {
                        record_diagnostic(
                            &self.diagnostics,
                            connection.name(),
                            format!("event `{}` handler failed: {message}", event.as_str()),
                        );
                    }
                    if policy.fail_closed && event == ExtensionEvent::ToolCall {
                        // fail-closed 的"拒绝动作"语义:拦截执行(07 §8.5)
                        outcome.tool_block = Some(ToolBlock {
                            block: true,
                            reason: format!(
                                "blocked: extension `{}` failed (fail-closed): {message}",
                                connection.name()
                            ),
                            ..Default::default()
                        });
                        break;
                    }
                    // 其余情况 fail-open:跳过该扩展的本次贡献,继续链
                }
            }
        }
        EventOutcome::Decision(Box::new(outcome))
    }

    // ---- 决策事件的类型化入口(ExtensionHooks 使用) ----

    pub async fn dispatch_tool_call(&self, ctx: &ToolCallCtx) -> DecisionOutcome {
        match self
            .dispatch(
                ExtensionEvent::ToolCall,
                serde_json::to_value(ctx).unwrap_or(Value::Null),
            )
            .await
        {
            EventOutcome::Decision(outcome) => *outcome,
            EventOutcome::Notified => DecisionOutcome::default(),
        }
    }

    pub async fn dispatch_tool_result(&self, ctx: &ToolResultCtx) -> DecisionOutcome {
        match self
            .dispatch(
                ExtensionEvent::ToolResult,
                serde_json::to_value(ctx).unwrap_or(Value::Null),
            )
            .await
        {
            EventOutcome::Decision(outcome) => *outcome,
            EventOutcome::Notified => DecisionOutcome::default(),
        }
    }

    pub async fn dispatch_context(&self, messages: Vec<AgentMessage>) -> DecisionOutcome {
        match self
            .dispatch(ExtensionEvent::Context, json!({ "messages": messages }))
            .await
        {
            EventOutcome::Decision(outcome) => *outcome,
            EventOutcome::Notified => DecisionOutcome::default(),
        }
    }

    pub async fn dispatch_before_request(
        &self,
        model: &rpi_ai::Model,
        thinking: Option<ThinkingLevel>,
    ) -> DecisionOutcome {
        let payload = json!({
            "model": model,
            "thinkingLevel": thinking,
        });
        match self.dispatch(ExtensionEvent::BeforeRequest, payload).await {
            EventOutcome::Decision(outcome) => *outcome,
            EventOutcome::Notified => DecisionOutcome::default(),
        }
    }

    pub async fn dispatch_turn_boundary(
        &self,
        event: ExtensionEvent,
        ctx: &TurnCtx,
    ) -> DecisionOutcome {
        debug_assert!(matches!(
            event,
            ExtensionEvent::PrepareNextTurn | ExtensionEvent::FinishTurn
        ));
        // TurnCtx 非 Serialize 类型,手工组 wire 载荷(字段均为可序列化类型)
        let payload = json!({
            "message": ctx.message,
            "toolResults": ctx.tool_results,
            "newMessages": ctx.new_messages,
        });
        match self.dispatch(event, payload).await {
            EventOutcome::Decision(outcome) => *outcome,
            EventOutcome::Notified => DecisionOutcome::default(),
        }
    }
}

/// 合并单个扩展的决策响应到聚合结果;返回 true = 短路(block)。
/// 改参链:每个决策事件都把扩展输出回写 payload,前一扩展输出 = 后一扩展输入
/// (07 §8.3);畸形响应(反序列化失败)= handler 失败 → 诊断 + 跳过(07 §8.5)。
fn merge_decision(
    event: ExtensionEvent,
    response: &Value,
    payload: &mut Value,
    outcome: &mut DecisionOutcome,
    extension: &str,
    diagnostics: &DiagnosticsSink,
) -> bool {
    /// 解析失败 = 该扩展本次贡献被丢弃,但不静默(07 §8.5)
    macro_rules! parse {
        ($ty:ty, $value:expr) => {
            match serde_json::from_value::<$ty>($value.clone()) {
                Ok(parsed) => parsed,
                Err(error) => {
                    record_diagnostic(
                        diagnostics,
                        extension,
                        format!("event `{}` invalid response: {error}", event.as_str()),
                    );
                    return false;
                }
            }
        };
    }

    match event {
        ExtensionEvent::ToolCall => {
            let block: ToolBlock = parse!(ToolBlock, response);
            if block.block {
                outcome.tool_block = Some(block);
                return true;
            }
            if let Some(args) = block.args {
                if let Some(object) = payload.as_object_mut() {
                    object.insert("args".into(), args.clone());
                }
                match &mut outcome.tool_block {
                    Some(existing) => existing.args = Some(args),
                    None => {
                        outcome.tool_block = Some(ToolBlock {
                            block: false,
                            args: Some(args),
                            ..Default::default()
                        })
                    }
                }
            }
            false
        }
        ExtensionEvent::ToolResult => {
            let patch: ToolPatch = parse!(ToolPatch, response);
            if patch.output.is_some() {
                outcome.patch.output = patch.output.clone();
            }
            if patch.details.is_some() {
                outcome.patch.details = patch.details.clone();
            }
            if patch.is_error.is_some() {
                outcome.patch.is_error = patch.is_error;
            }
            if patch.terminate.is_some() {
                outcome.patch.terminate = patch.terminate;
            }
            false
        }
        ExtensionEvent::Context => {
            let Some(messages) = response.get("messages") else {
                return false;
            };
            let messages: Vec<AgentMessage> = parse!(Vec<AgentMessage>, messages);
            if let Some(object) = payload.as_object_mut() {
                object.insert(
                    "messages".into(),
                    serde_json::to_value(&messages).unwrap_or(Value::Null),
                );
            }
            outcome.messages = Some(messages);
            false
        }
        ExtensionEvent::BeforeRequest => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct RequestResponse {
                model: Option<rpi_ai::Model>,
                thinking_level: Option<Option<ThinkingLevel>>,
            }
            let parsed: RequestResponse = parse!(RequestResponse, response);
            // 链式回写:后续扩展看到前一个扩展改过的 model/thinking
            if parsed.model.is_some() {
                outcome.request.model = parsed.model.clone();
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "model".into(),
                        serde_json::to_value(&parsed.model).unwrap_or(Value::Null),
                    );
                }
            }
            if parsed.thinking_level.is_some() {
                outcome.request.thinking_level = parsed.thinking_level;
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "thinkingLevel".into(),
                        serde_json::to_value(parsed.thinking_level).unwrap_or(Value::Null),
                    );
                }
            }
            false
        }
        ExtensionEvent::PrepareNextTurn => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct TurnResponse {
                messages: Option<Vec<AgentMessage>>,
                model: Option<rpi_ai::Model>,
                thinking_level: Option<Option<ThinkingLevel>>,
            }
            let parsed: TurnResponse = parse!(TurnResponse, response);
            if parsed.messages.is_some() {
                outcome.turn_update.messages = parsed.messages.clone();
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "messages".into(),
                        serde_json::to_value(&parsed.messages).unwrap_or(Value::Null),
                    );
                }
            }
            if parsed.model.is_some() {
                outcome.turn_update.model = parsed.model.clone();
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "model".into(),
                        serde_json::to_value(&parsed.model).unwrap_or(Value::Null),
                    );
                }
            }
            if parsed.thinking_level.is_some() {
                outcome.turn_update.thinking_level = parsed.thinking_level;
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "thinkingLevel".into(),
                        serde_json::to_value(parsed.thinking_level).unwrap_or(Value::Null),
                    );
                }
            }
            false
        }
        ExtensionEvent::FinishTurn => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct DecisionResponse {
                decision: Option<String>,
            }
            let parsed: DecisionResponse = parse!(DecisionResponse, response);
            if let Some(decision) = parsed.decision {
                // 链式回写:后续扩展在 payload 里看到当前决策
                if let Some(object) = payload.as_object_mut() {
                    object.insert("decision".into(), json!(decision));
                }
                match decision.as_str() {
                    "continue" => outcome.decision = Some(TurnDecision::Continue),
                    "end" => outcome.decision = Some(TurnDecision::End),
                    other => {
                        record_diagnostic(
                            diagnostics,
                            extension,
                            format!("event `finish_turn` invalid decision: {other}"),
                        );
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// 观察类:循环事件 → 通知(接缝 #6 的一个订阅者)。
#[async_trait]
impl Subscriber for ExtensionEventBus {
    async fn on_event(&self, event: &AgentEvent) {
        let (event, payload) = match event {
            AgentEvent::AgentStart => (ExtensionEvent::AgentStart, json!({})),
            AgentEvent::TurnStart => (ExtensionEvent::TurnStart, json!({})),
            AgentEvent::TurnEnd {
                message,
                tool_results,
            } => (
                ExtensionEvent::TurnEnd,
                json!({ "message": message, "toolResults": tool_results }),
            ),
            AgentEvent::MessageStart { message, .. } => {
                (ExtensionEvent::MessageStart, json!({ "message": message }))
            }
            AgentEvent::MessageDelta { delta } => {
                // T2 类型化增量:Text 保持旧 wire 形态,Thinking/ToolCallArgs 各自展开
                let payload = match delta {
                    rpi_agent::MessageDeltaPayload::Text { delta } => json!({ "delta": delta }),
                    rpi_agent::MessageDeltaPayload::Thinking { delta } => {
                        json!({ "thinking": delta })
                    }
                    rpi_agent::MessageDeltaPayload::ToolCallArgs {
                        content_index,
                        delta,
                    } => json!({
                        "toolCall": { "contentIndex": content_index, "delta": delta }
                    }),
                };
                (ExtensionEvent::MessageDelta, payload)
            }
            AgentEvent::MessageUpdate { message } => {
                (ExtensionEvent::MessageUpdate, json!({ "message": message }))
            }
            AgentEvent::MessageEnd { message } => {
                (ExtensionEvent::MessageEnd, json!({ "message": message }))
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => (
                ExtensionEvent::ToolExecutionStart,
                json!({ "toolCallId": tool_call_id, "toolName": tool_name, "args": args }),
            ),
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                partial,
            } => (
                ExtensionEvent::ToolExecutionUpdate,
                json!({ "toolCallId": tool_call_id, "toolName": tool_name, "partial": partial }),
            ),
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                output,
                is_error,
            } => (
                ExtensionEvent::ToolExecutionEnd,
                json!({ "toolCallId": tool_call_id, "toolName": tool_name, "output": output, "isError": is_error }),
            ),
            AgentEvent::AgentEnd { messages } => {
                (ExtensionEvent::AgentEnd, json!({ "messages": messages }))
            }
        };
        // 热路径守卫:无订阅者不构造/不发送载荷(07 §8.3)
        if !self.has_subscriber(event) {
            return;
        }
        self.dispatch(event, payload).await;
    }
}

/// 观察类:session 级事件 → 通知(循环事件已由 Subscriber 面覆盖)。
#[async_trait]
impl SessionSubscriber for ExtensionEventBus {
    async fn on_session_event(&self, event: &AgentSessionEvent) {
        let (event, payload) = match event {
            AgentSessionEvent::Agent(_) => return,
            AgentSessionEvent::AgentSettled => (ExtensionEvent::AgentSettled, json!({})),
            AgentSessionEvent::QueueUpdate {
                steering,
                follow_up,
            } => (
                ExtensionEvent::QueueUpdate,
                json!({ "steering": steering, "followUp": follow_up }),
            ),
            AgentSessionEvent::AutoRetryStart {
                attempt,
                delay_ms,
                reason,
            } => (
                ExtensionEvent::AutoRetryStart,
                json!({ "attempt": attempt, "delayMs": delay_ms, "reason": reason }),
            ),
            AgentSessionEvent::AutoRetryEnd { success, reason } => (
                ExtensionEvent::AutoRetryEnd,
                json!({ "success": success, "reason": reason }),
            ),
        };
        if !self.has_subscriber(event) {
            return;
        }
        self.dispatch(event, payload).await;
    }
}

/// 接缝 #2 包装:决策类先问扩展再透传内层 hooks(07 §8.3 接入方式)。
/// 观察类不经此层(总线直接作为 Subscriber 订阅)。
pub struct ExtensionHooks {
    inner: Arc<dyn LoopHooks>,
    bus: Arc<ExtensionEventBus>,
}

impl ExtensionHooks {
    pub fn new(inner: Arc<dyn LoopHooks>, bus: Arc<ExtensionEventBus>) -> Self {
        ExtensionHooks { inner, bus }
    }
}

#[async_trait]
impl LoopHooks for ExtensionHooks {
    fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
        self.inner.convert_to_llm(msgs)
    }

    async fn transform_context(&self, msgs: Vec<AgentMessage>) -> Vec<AgentMessage> {
        if !self.bus.has_subscriber(ExtensionEvent::Context) {
            return self.inner.transform_context(msgs).await;
        }
        let outcome = self.bus.dispatch_context(msgs.clone()).await;
        match outcome.messages {
            Some(messages) => self.inner.transform_context(messages).await,
            None => self.inner.transform_context(msgs).await,
        }
    }

    async fn get_api_key(&self, provider: &str) -> Option<String> {
        self.inner.get_api_key(provider).await
    }

    async fn prepare_request(
        &self,
        model: &rpi_ai::Model,
        thinking: Option<ThinkingLevel>,
    ) -> Option<RequestUpdate> {
        if !self.bus.has_subscriber(ExtensionEvent::BeforeRequest) {
            return self.inner.prepare_request(model, thinking).await;
        }
        let mut outcome = self.bus.dispatch_before_request(model, thinking).await;
        // 扩展先问,内层(宿主权威)后到且覆盖同字段
        if let Some(inner) = self.inner.prepare_request(model, thinking).await {
            if inner.model.is_some() {
                outcome.request.model = inner.model;
            }
            if inner.thinking_level.is_some() {
                outcome.request.thinking_level = inner.thinking_level;
            }
        }
        let has_any = outcome.request.model.is_some() || outcome.request.thinking_level.is_some();
        has_any.then_some(outcome.request)
    }

    async fn prepare_next_turn(&self, ctx: TurnCtx) -> Option<TurnUpdate> {
        if !self.bus.has_subscriber(ExtensionEvent::PrepareNextTurn) {
            return self.inner.prepare_next_turn(ctx).await;
        }
        let mut outcome = self
            .bus
            .dispatch_turn_boundary(ExtensionEvent::PrepareNextTurn, &ctx)
            .await;
        if let Some(inner) = self.inner.prepare_next_turn(ctx).await {
            if inner.messages.is_some() {
                outcome.turn_update.messages = inner.messages;
            }
            if inner.model.is_some() {
                outcome.turn_update.model = inner.model;
            }
            if inner.thinking_level.is_some() {
                outcome.turn_update.thinking_level = inner.thinking_level;
            }
        }
        let has_any = outcome.turn_update.messages.is_some()
            || outcome.turn_update.model.is_some()
            || outcome.turn_update.thinking_level.is_some();
        has_any.then_some(outcome.turn_update)
    }

    async fn finish_turn(&self, ctx: TurnCtx) -> Option<TurnDecision> {
        if !self.bus.has_subscriber(ExtensionEvent::FinishTurn) {
            return self.inner.finish_turn(ctx).await;
        }
        let outcome = self
            .bus
            .dispatch_turn_boundary(ExtensionEvent::FinishTurn, &ctx)
            .await;
        // 与 prepare_request/prepare_next_turn 统一:内层(宿主权威)胜出
        match self.inner.finish_turn(ctx).await {
            Some(decision) => Some(decision),
            None => outcome.decision,
        }
    }

    async fn before_tool_call(&self, mut ctx: ToolCallCtx) -> Option<ToolBlock> {
        if !self.bus.has_subscriber(ExtensionEvent::ToolCall) {
            return self.inner.before_tool_call(ctx).await;
        }
        let outcome = self.bus.dispatch_tool_call(&ctx).await;
        if let Some(block) = &outcome.tool_block {
            if block.block {
                // 扩展拦截:短路,不透传内层(07 §8.3)
                return Some(block.clone());
            }
            // 改参透传:内层钩子看到链式传递后的参数
            if let Some(args) = &block.args {
                ctx.args = args.clone();
            }
        }
        self.inner.before_tool_call(ctx).await
    }

    async fn after_tool_call(&self, mut ctx: ToolResultCtx) -> Option<ToolPatch> {
        if !self.bus.has_subscriber(ExtensionEvent::ToolResult) {
            return self.inner.after_tool_call(ctx).await;
        }
        let outcome = self.bus.dispatch_tool_result(&ctx).await;
        // 扩展 patch 先落到 ctx(内层看到改后值),内层 patch 覆盖同字段
        if let Some(text) = &outcome.patch.output {
            ctx.output = text.clone();
        }
        if let Some(details) = &outcome.patch.details {
            ctx.details = details.clone();
        }
        if let Some(flag) = outcome.patch.is_error {
            ctx.is_error = flag;
        }
        if let Some(flag) = outcome.patch.terminate {
            ctx.terminate = flag;
        }
        match self.inner.after_tool_call(ctx).await {
            Some(inner_patch) => {
                let mut merged = outcome.patch;
                if inner_patch.output.is_some() {
                    merged.output = inner_patch.output;
                }
                if inner_patch.details.is_some() {
                    merged.details = inner_patch.details;
                }
                if inner_patch.is_error.is_some() {
                    merged.is_error = inner_patch.is_error;
                }
                if inner_patch.terminate.is_some() {
                    merged.terminate = inner_patch.terminate;
                }
                (!merged.is_empty()).then_some(merged)
            }
            None => (!outcome.patch.is_empty()).then_some(outcome.patch),
        }
    }

    fn tool_execution(&self) -> rpi_agent::ToolExecution {
        self.inner.tool_execution()
    }
}
