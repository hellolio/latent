//! agent 循环(03 文档 §3 伪代码逐行实现)。
//!
//! 双层 while:内层"有工具调用或待注入消息就继续 turn",外层"follow-up /
//! 显式 continue 唤醒";`error`/`aborted` 是唯一硬退出(不碰任何队列,不变量 I3);
//! 工具执行四阶段 prepare→execute→finalize→result,批结果以 `Vec<ToolOutcome>`
//! 返回 —— 长度恒等于 toolCall 数,"每个 toolCall 恰好一个 toolResult" 是类型
//! 不变量(03 文档 §10.4,修复 pi 串行 abort 缺口的类型化方案)。
//!
//! 护栏内建:`TurnLimits`(max_turns/max_tool_calls/max_total_tokens/deadline/
//! max_truncation_retries),超限以 `RunStop::BudgetExhausted` 可区分终止,不伪装
//! 成 error。低层无内建 provider 重试(不变量 I2)—— 重试经装饰 `Provider` 在
//! 上层注入(pi 的 retryAssistantCall 等价物)。

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use rpi_ai::{
    normalize_context, AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Model,
    Provider, StopReason, StreamOptions, ThinkingLevel, Tool as DeclaredTool, TranscriptContext,
};

use crate::declare::declare_tool_changes;
use crate::event::{AgentEvent, Subscriber};
use crate::hooks::{
    LoopHooks, ToolCallCtx, ToolPatch, ToolResultCtx, TurnCtx, TurnDecision, TurnUpdate,
};
use crate::message::{now_ms, AgentMessage};
use crate::tool::{Tool, ToolCall, ToolError, ToolExecution, ToolOutput, ToolUpdater};

/// 循环的一次输入快照(pi 的 createContextSnapshot:messages.slice(),09 A2 接缝 2)
#[derive(Clone, Default)]
pub struct AgentContext {
    pub system: Option<String>,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn Tool>>,
}

/// 护栏(03 文档 §10.4):None = 不限制;`max_truncation_retries` 默认 3,
/// 0 表示禁止 length 截断重发。
#[derive(Debug, Clone, Copy)]
pub struct TurnLimits {
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_total_tokens: Option<u64>,
    pub deadline: Option<std::time::Instant>,
    pub max_truncation_retries: u32,
}

impl Default for TurnLimits {
    fn default() -> Self {
        TurnLimits {
            max_turns: None,
            max_tool_calls: None,
            max_total_tokens: None,
            deadline: None,
            max_truncation_retries: 3,
        }
    }
}

/// 一次 run 的循环配置:model/thinking 可被钩子逐轮切换。
#[derive(Clone)]
pub struct LoopConfig {
    pub model: Model,
    /// None = 不请求思考(01 文档:ai 侧级别无 off)
    pub thinking: Option<ThinkingLevel>,
    pub limits: TurnLimits,
    pub stream_options: StreamOptions,
}

impl LoopConfig {
    pub fn new(model: Model) -> Self {
        LoopConfig {
            model,
            thinking: None,
            limits: TurnLimits::default(),
            stream_options: StreamOptions::default(),
        }
    }
}

/// 显式状态阶段(03 文档 §10.3):观察/调试用;控制流由双层 while 表达,
/// Phase 标注当前所处阶段供宿主日志与断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Prompt,
    Streaming,
    ExecutingTools,
    Settling,
    Done,
}

/// 超限的护栏种类(可区分终止,不伪装成 error)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    MaxTurns,
    MaxToolCalls,
    MaxTotalTokens,
    Deadline,
    TruncationRetries,
}

/// run 终止原因。provider 失败已编码为 stopReason=error 的 assistant 消息进转录,
/// 这里只携带归类信息供调用方(重试/恢复策略)决策。
#[derive(Debug, Clone, PartialEq)]
pub enum RunStop {
    EndTurn,
    Aborted,
    /// provider/传输失败(转录内最后一条 assistant 消息带 errorMessage)
    Error(String),
    BudgetExhausted(BudgetKind),
}

/// 循环输出:本次 run 新增的全部消息 + 终止原因。
#[derive(Debug, Clone)]
pub struct LoopOutput {
    pub messages: Vec<AgentMessage>,
    pub stop: RunStop,
}

/// 单个工具调用的结算结果(03 文档 §10.4):批结果 Vec 长度恒等于 toolCall 数。
#[derive(Debug, Clone)]
pub enum ToolOutcome {
    Completed { call: ToolCall, output: ToolOutput, is_error: bool },
    /// abort/超时:以错误 tool result 收尾,保证转录配对
    Cancelled { call: ToolCall },
    /// beforeToolCall 拦截
    Blocked { call: ToolCall, reason: String, terminate: bool },
}

impl ToolOutcome {
    fn call(&self) -> &ToolCall {
        match self {
            ToolOutcome::Completed { call, .. }
            | ToolOutcome::Cancelled { call }
            | ToolOutcome::Blocked { call, .. } => call,
        }
    }

    fn terminate(&self) -> bool {
        match self {
            ToolOutcome::Completed { output, .. } => output.terminate,
            ToolOutcome::Cancelled { .. } => false,
            ToolOutcome::Blocked { terminate, .. } => *terminate,
        }
    }
}

/// 工具执行期进度上报:转发为 `tool_execution_update` 事件。
struct EventUpdater {
    sink: Arc<dyn Subscriber>,
    tool_call_id: String,
    tool_name: String,
}

#[async_trait]
impl ToolUpdater for EventUpdater {
    async fn update(&self, partial: String) {
        self.sink
            .on_event(&AgentEvent::ToolExecutionUpdate {
                tool_call_id: self.tool_call_id.clone(),
                tool_name: self.tool_name.clone(),
                partial,
            })
            .await;
    }
}

/// 工具调用批的执行结果。
struct ToolBatch {
    messages: Vec<AgentMessage>,
    terminate: bool,
}

/// 主循环。签名即接缝:快照进(`AgentContext`)、事件出(`Subscriber`)、
/// 无可变全局状态(09 A4)。
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    hooks: Arc<dyn LoopHooks>,
    config: LoopConfig,
    provider: Arc<dyn Provider>,
    sink: Arc<dyn Subscriber>,
    cancel: CancellationToken,
) -> LoopOutput {
    let tools = context.tools;
    let system = context.system;
    let mut current: Vec<AgentMessage> = context.messages;
    let mut model = config.model;
    let mut thinking = config.thinking;
    let stream_options = config.stream_options;
    let limits = config.limits;

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    let mut last_turn: Option<(AssistantMessage, Vec<AgentMessage>)> = None;
    let mut explicit_continuation = false;
    // 循环开始即轮询一次 steering(用户可能已在等待期输入,03 §3)
    let mut pending: Vec<AgentMessage> = hooks.steering_messages().await;
    let mut turn_count: u32 = 0;
    let mut tool_call_count: u32 = 0;
    let mut total_tokens: u64 = 0;
    let mut truncation_turns: u32 = 0;

    sink.on_event(&AgentEvent::AgentStart).await;

    // 首个 turn:turn_start → 注入 prompts + 初始 steering(注入前声明工具集增量)
    sink.on_event(&AgentEvent::TurnStart).await;
    let initial: Vec<AgentMessage> = prompts.into_iter().chain(pending.drain(..)).collect();
    for message in declare_tool_changes(&current, &tools, initial) {
        emit_message_events(&sink, &message).await;
        current.push(message.clone());
        new_messages.push(message);
    }

    let budget_stop =
        |turn_count: u32, tool_call_count: u32, total_tokens: u64, truncation_turns: u32| -> Option<RunStop> {
            if limits.max_turns.is_some_and(|max| turn_count >= max) {
                return Some(RunStop::BudgetExhausted(BudgetKind::MaxTurns));
            }
            if limits.max_tool_calls.is_some_and(|max| tool_call_count >= max) {
                return Some(RunStop::BudgetExhausted(BudgetKind::MaxToolCalls));
            }
            if limits.max_total_tokens.is_some_and(|max| total_tokens >= max) {
                return Some(RunStop::BudgetExhausted(BudgetKind::MaxTotalTokens));
            }
            if limits.deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return Some(RunStop::BudgetExhausted(BudgetKind::Deadline));
            }
            if truncation_turns > limits.max_truncation_retries {
                return Some(RunStop::BudgetExhausted(BudgetKind::TruncationRetries));
            }
            None
        };

    let stop = 'outer: loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending.is_empty() {
            // 预算检查在 drain/prepareNextTurn/TurnStart 之前:耗尽时不丢已入队的
            // 消息、不发无法配对的 TurnStart(03 文档护栏语义)
            if let Some(budget) = budget_stop(turn_count, tool_call_count, total_tokens, truncation_turns) {
                break 'outer budget;
            }
            let mut prepared_messages: Vec<AgentMessage> = Vec::new();
            if let Some((message, tool_results)) = last_turn.clone() {
                // prepareNextTurn:可追加消息、切换 model/thinking(03 §3 ②)
                let update = hooks
                    .prepare_next_turn(TurnCtx {
                        message: Box::new(message),
                        tool_results: tool_results.clone(),
                        new_messages: new_messages.clone(),
                    })
                    .await;
                if let Some(TurnUpdate { messages, model: m, thinking_level: t }) = update {
                    if let Some(messages) = messages {
                        prepared_messages = messages;
                    }
                    if let Some(m) = m {
                        model = m;
                    }
                    if let Some(t) = t {
                        thinking = t;
                    }
                }
                // 补轮询仅当前次为空:one-at-a-time 防双注入(03 §3,pi :203-205)
                if pending.is_empty() {
                    pending = hooks.steering_messages().await;
                }
                sink.on_event(&AgentEvent::TurnStart).await;
            }

            // ① 注入待处理消息(prepared + pending),注入前声明工具集增量
            let mut injectables = prepared_messages;
            injectables.append(&mut pending);
            for message in declare_tool_changes(&current, &tools, injectables) {
                emit_message_events(&sink, &message).await;
                current.push(message.clone());
                new_messages.push(message);
            }
            pending.clear();

            // ② 每次请求前(含第一次)的 prepareRequest
            if let Some(update) = hooks.prepare_request(&model, thinking).await {
                if let Some(m) = update.model {
                    model = m;
                }
                if let Some(t) = update.thinking_level {
                    thinking = t;
                }
            }

            // ③ 流式请求 LLM(partial buffer 于循环局部,done 后一次性 push,09 B5.2)
            let message = stream_assistant_response(
                &current,
                system.as_deref(),
                &tools,
                &hooks,
                &model,
                thinking,
                &stream_options,
                &provider,
                &sink,
                &cancel,
            )
            .await;
            current.push(AgentMessage::Assistant(Box::new(message.clone())));
            new_messages.push(AgentMessage::Assistant(Box::new(message.clone())));
            total_tokens = total_tokens.saturating_add(message.usage.total_tokens);
            turn_count += 1;
            if message.stop_reason == StopReason::Length && message.has_tool_calls() {
                truncation_turns += 1;
            }

            // ④ 硬退出:error / aborted —— 不执行工具、不轮询任何队列(不变量 I3);
            // finishTurn 仍会调用,但返回的决策被忽略(03 §3 ④,pi :245-255)
            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                let _ignored = hooks
                    .finish_turn(TurnCtx {
                        message: Box::new(message.clone()),
                        tool_results: Vec::new(),
                        new_messages: new_messages.clone(),
                    })
                    .await;
                sink.on_event(&AgentEvent::TurnEnd {
                    message: Box::new(message.clone()),
                    tool_results: Vec::new(),
                })
                .await;
                break 'outer match message.stop_reason {
                    StopReason::Aborted => RunStop::Aborted,
                    _ => RunStop::Error(
                        message.error_message.clone().unwrap_or_else(|| "provider error".into()),
                    ),
                };
            }

            // ⑤ 工具调用
            let calls: Vec<ToolCall> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { id, name, arguments } => Some(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        args: arguments.clone(),
                    }),
                    _ => None,
                })
                .collect();
            let mut tool_results: Vec<AgentMessage> = Vec::new();
            has_more_tool_calls = false;
            if !calls.is_empty() {
                tool_call_count += calls.len() as u32;
                let batch = if message.stop_reason == StopReason::Length {
                    // 截断防御:全部拒执行,terminate=false 让模型重发(不变量 I5)
                    fail_tool_calls_from_truncated(&calls, &sink, &mut current, &mut new_messages).await
                } else {
                    execute_tool_calls(&calls, &tools, &hooks, &sink, &cancel, &mut current, &mut new_messages)
                        .await
                };
                tool_results = batch.messages;
                has_more_tool_calls = !batch.terminate;
            }

            // ⑤.5 工具批执行期间 abort:结果已配对结算,run 以 aborted 硬退出
            // (不轮询任何队列,不变量 I3)
            if cancel.is_cancelled() {
                sink.on_event(&AgentEvent::TurnEnd { message: Box::new(message.clone()), tool_results })
                    .await;
                break 'outer RunStop::Aborted;
            }

            // ⑥ turn 收尾:finishTurn → turn_end → 预算检查 → 决策 → steering 轮询
            last_turn = Some((message.clone(), tool_results.clone()));
            let decision = hooks
                .finish_turn(TurnCtx {
                    message: Box::new(message.clone()),
                    tool_results: tool_results.clone(),
                    new_messages: new_messages.clone(),
                })
                .await;
            sink.on_event(&AgentEvent::TurnEnd { message: Box::new(message.clone()), tool_results })
                .await;

            if let Some(budget) = budget_stop(turn_count, tool_call_count, total_tokens, truncation_turns) {
                break 'outer budget;
            }
            if decision == Some(TurnDecision::End) {
                break 'outer RunStop::EndTurn;
            }
            explicit_continuation = decision == Some(TurnDecision::Continue);
            pending = hooks.steering_messages().await;
            if has_more_tool_calls || !pending.is_empty() {
                // 有自然请求时,"continue" 配额被自然消耗,不再额外发一次
                explicit_continuation = false;
            }
        }

        // —— 内层退出:agent 本应停止 ——
        let follow_ups = hooks.follow_up_messages().await;
        if !follow_ups.is_empty() {
            explicit_continuation = false;
            pending = follow_ups;
            continue 'outer;
        }
        if explicit_continuation {
            // 无自然请求时,用"仅上下文"的一轮兑现 continue
            explicit_continuation = false;
            continue 'outer;
        }
        break 'outer RunStop::EndTurn;
    };

    // agent_end 是 run 的最后事件(此前全部 listener 已串行完成,结算语义成立)
    sink.on_event(&AgentEvent::AgentEnd { messages: new_messages.clone() }).await;
    LoopOutput { messages: new_messages, stop }
}

async fn emit_message_events(sink: &Arc<dyn Subscriber>, message: &AgentMessage) {
    sink.on_event(&AgentEvent::MessageStart { message: Box::new(message.clone()) }).await;
    sink.on_event(&AgentEvent::MessageEnd { message: Box::new(message.clone()) }).await;
}

/// 流式处理(03 文档 §4):折叠转录 → 请求 → 事件转发 → 终态消息。
/// partial 不占转录末位(09 B5.2 决策):文本增量经 `MessageDelta` 转发,
/// 终态后 `MessageUpdate`(快照)+ `MessageEnd`。
#[allow(clippy::too_many_arguments)]
async fn stream_assistant_response(
    current: &[AgentMessage],
    system: Option<&str>,
    tools: &[Arc<dyn Tool>],
    hooks: &Arc<dyn LoopHooks>,
    model: &Model,
    thinking: Option<ThinkingLevel>,
    stream_options: &StreamOptions,
    provider: &Arc<dyn Provider>,
    sink: &Arc<dyn Subscriber>,
    cancel: &CancellationToken,
) -> AssistantMessage {
    let transformed = hooks.transform_context(current.to_vec()).await;
    let llm_messages = hooks.convert_to_llm(&transformed);
    let declarations: Vec<DeclaredTool> = tools
        .iter()
        .map(|tool| DeclaredTool::new(tool.name(), tool.description(), tool.schema()))
        .collect();
    let transcript: TranscriptContext = normalize_context(Context {
        system_prompt: system.map(str::to_string),
        messages: llm_messages,
        tools: declarations,
    });

    let mut opts = stream_options.clone();
    opts.reasoning = thinking;
    opts.cancel = Some(cancel.clone());
    if opts.api_key.is_none() {
        opts.api_key = hooks.get_api_key(&model.provider).await;
    }

    let stream = provider.stream(model, transcript, opts).await;
    let mut stream = std::pin::pin!(stream);
    let mut started = false;
    let mut final_message: Option<AssistantMessage> = None;

    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::Start => {
                started = true;
                sink.on_event(&AgentEvent::MessageStart {
                    message: Box::new(AgentMessage::Assistant(Box::new(AssistantMessage::pending(model)))),
                })
                .await;
            }
            AssistantMessageEvent::TextDelta { delta, .. } => {
                sink.on_event(&AgentEvent::MessageDelta { delta }).await;
            }
            AssistantMessageEvent::Done(message) | AssistantMessageEvent::Error(message) => {
                if !started {
                    sink.on_event(&AgentEvent::MessageStart {
                        message: Box::new(AgentMessage::Assistant(Box::new(AssistantMessage::pending(model)))),
                    })
                    .await;
                }
                sink.on_event(&AgentEvent::MessageUpdate { message: message.clone() }).await;
                sink.on_event(&AgentEvent::MessageEnd {
                    message: Box::new(AgentMessage::Assistant(message.clone())),
                })
                .await;
                final_message = Some(*message);
            }
            // thinking/toolcall 的 start/end 不逐个转发:thinking/toolcall 快照
            // 随终态 MessageUpdate 一次性可见(M5 TUI 需要逐块增量时再扩展事件)
            AssistantMessageEvent::TextStart { .. }
            | AssistantMessageEvent::TextEnd { .. }
            | AssistantMessageEvent::ThinkingStart { .. }
            | AssistantMessageEvent::ThinkingDelta { .. }
            | AssistantMessageEvent::ThinkingEnd { .. }
            | AssistantMessageEvent::ToolCallStart { .. }
            | AssistantMessageEvent::ToolCallDelta { .. }
            | AssistantMessageEvent::ToolCallEnd { .. } => {}
        }
    }

    match final_message {
        Some(message) => message,
        // 流意外正常退出(契约要求终态事件;兜底防御)
        None => {
            let message = AssistantMessage::error(model, "stream ended without a terminal event", false);
            sink.on_event(&AgentEvent::MessageStart {
                message: Box::new(AgentMessage::Assistant(Box::new(AssistantMessage::pending(model)))),
            })
            .await;
            sink.on_event(&AgentEvent::MessageEnd {
                message: Box::new(AgentMessage::Assistant(Box::new(message.clone()))),
            })
            .await;
            message
        }
    }
}

/// length 截断防御(03 文档 §5.6,不变量 I5):对全部 tool call 发
/// start + 错误结果,terminate=false 继续循环让模型重发。
async fn fail_tool_calls_from_truncated(
    calls: &[ToolCall],
    sink: &Arc<dyn Subscriber>,
    transcript: &mut Vec<AgentMessage>,
    new_messages: &mut Vec<AgentMessage>,
) -> ToolBatch {
    let mut messages = Vec::with_capacity(calls.len());
    for call in calls {
        sink.on_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.args.clone(),
        })
        .await;
        let output = "The previous response may have been truncated, so this tool call may be \
                      incomplete. Re-issue the tool call with complete arguments.";
        sink.on_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            output: output.to_string(),
            is_error: true,
        })
        .await;
        let result = AgentMessage::tool_result_text(&call.id, &call.name, output, true);
        emit_message_events(sink, &result).await;
        transcript.push(result.clone());
        new_messages.push(result.clone());
        messages.push(result);
    }
    ToolBatch { messages, terminate: false }
}

/// 工具执行(03 文档 §5):模式选择 → 串行/并行 → 结果消息。
/// `Vec<ToolOutcome>` 长度恒等于 calls 数(类型不变量,§10.4)。
async fn execute_tool_calls(
    calls: &[ToolCall],
    tools: &[Arc<dyn Tool>],
    hooks: &Arc<dyn LoopHooks>,
    sink: &Arc<dyn Subscriber>,
    cancel: &CancellationToken,
    transcript: &mut Vec<AgentMessage>,
    new_messages: &mut Vec<AgentMessage>,
) -> ToolBatch {
    let sequential = hooks.tool_execution() == ToolExecution::Serial
        || calls.iter().any(|call| {
            tools
                .iter()
                .find(|tool| tool.name() == call.name)
                .and_then(|tool| tool.execution_mode())
                == Some(ToolExecution::Serial)
        });

    let (outcomes, messages) = if sequential {
        // 串行:逐工具 start → prepare → execute → finalize → end → message_start/end 交错(pi §5.2)
        let outcomes =
            execute_batch_sequential(calls, tools, hooks, sink, cancel, transcript, new_messages).await;
        let messages: Vec<AgentMessage> = outcomes
            .iter()
            .map(outcome_to_message)
            .collect();
        (outcomes, messages)
    } else {
        // 并行:end 事件已按完成序发出;结果消息按源序补发(不变量 I4)
        let outcomes = execute_batch_parallel(calls, tools, hooks, sink, cancel).await;
        let mut messages = Vec::with_capacity(outcomes.len());
        for outcome in &outcomes {
            let result = outcome_to_message(outcome);
            emit_message_events(sink, &result).await;
            transcript.push(result.clone());
            new_messages.push(result.clone());
            messages.push(result);
        }
        (outcomes, messages)
    };
    // 提前终止:批非空且每个结果 terminate(03 文档 §5.5)
    let terminate = !outcomes.is_empty() && outcomes.iter().all(ToolOutcome::terminate);
    ToolBatch { messages, terminate }
}

/// 串行批:逐个 start → prepare → execute → finalize → end。
/// abort 后剩余调用不再执行,但**仍以 Cancelled 结算**,保证配对完整
/// (修复 pi 串行 break 缺口,03 文档 §10.2.1)。
async fn execute_batch_sequential(
    calls: &[ToolCall],
    tools: &[Arc<dyn Tool>],
    hooks: &Arc<dyn LoopHooks>,
    sink: &Arc<dyn Subscriber>,
    cancel: &CancellationToken,
    transcript: &mut Vec<AgentMessage>,
    new_messages: &mut Vec<AgentMessage>,
) -> Vec<ToolOutcome> {
    let mut outcomes = Vec::with_capacity(calls.len());
    for call in calls {
        sink.on_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.args.clone(),
        })
        .await;
        if cancel.is_cancelled() {
            emit_tool_end(sink, call, "Operation aborted", true).await;
            let outcome = ToolOutcome::Cancelled { call: call.clone() };
            let result = outcome_to_message(&outcome);
            emit_message_events(sink, &result).await;
            transcript.push(result.clone());
            new_messages.push(result);
            outcomes.push(outcome);
            continue;
        }
        let outcome = match prepare_call(call, tools, hooks).await {
            Prepared::Immediate(outcome) => outcome,
            Prepared::Ready(tool, effective) => {
                execute_and_finalize(&effective, tool, hooks.clone(), sink.clone(), cancel.clone()).await
            }
        };
        emit_tool_end_for_outcome(sink, &outcome).await;
        let result = outcome_to_message(&outcome);
        emit_message_events(sink, &result).await;
        transcript.push(result.clone());
        new_messages.push(result);
        outcomes.push(outcome);
    }
    outcomes
}

/// 并行批(03 文档 §5.3):start+prepare 顺序执行(immediate 结果就地落定);
/// 其余 thunk 并发执行,`tool_execution_end` **按完成序**发出(thunk 完成时自发)。
async fn execute_batch_parallel(
    calls: &[ToolCall],
    tools: &[Arc<dyn Tool>],
    hooks: &Arc<dyn LoopHooks>,
    sink: &Arc<dyn Subscriber>,
    cancel: &CancellationToken,
) -> Vec<ToolOutcome> {
    // Phase 1:start + prepare(顺序),immediate 结果就地落定
    let mut deferred: Vec<(usize, ToolCall, Arc<dyn Tool>)> = Vec::new();
    let mut slots: Vec<Option<ToolOutcome>> = vec![None; calls.len()];
    for (index, call) in calls.iter().enumerate() {
        sink.on_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.args.clone(),
        })
        .await;
        if cancel.is_cancelled() {
            emit_tool_end(sink, call, "Operation aborted", true).await;
            slots[index] = Some(ToolOutcome::Cancelled { call: call.clone() });
            continue;
        }
        match prepare_call(call, tools, hooks).await {
            Prepared::Immediate(outcome) => {
                emit_tool_end_for_outcome(sink, &outcome).await;
                slots[index] = Some(outcome);
            }
            Prepared::Ready(tool, effective) => deferred.push((index, effective, tool)),
        }
    }

    // Phase 2:并发执行 thunk;end 事件由 thunk 完成时自发(完成序)
    let mut join_set = tokio::task::JoinSet::new();
    for (index, call, tool) in deferred {
        let hooks = hooks.clone();
        let sink = sink.clone();
        let cancel = cancel.clone();
        join_set.spawn(async move {
            // thunk 内自捕获 panic:转错误结果并正常发 end 事件,保证事件/消息配对
            let future = execute_and_finalize(&call, tool, hooks, sink.clone(), cancel);
            let outcome = match AssertUnwindSafe(future).catch_unwind().await {
                Ok(outcome) => outcome,
                Err(panic) => {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    ToolOutcome::Completed {
                        call: call.clone(),
                        output: ToolOutput::text(format!("tool execution panicked: {message}")),
                        is_error: true,
                    }
                }
            };
            emit_tool_end_for_outcome(&sink, &outcome).await;
            (index, outcome)
        });
    }
    while let Some(joined) = join_set.join_next().await {
        if let Ok((index, outcome)) = joined {
            slots[index] = Some(outcome);
        }
    }

    // Phase 3:收集为按源序的完整 Vec(任何槽位缺失都回填 Cancelled,保配对)
    let mut outcomes: Vec<ToolOutcome> = Vec::with_capacity(calls.len());
    for (slot, call) in slots.into_iter().zip(calls.iter()) {
        outcomes.push(slot.unwrap_or_else(|| ToolOutcome::Cancelled { call: call.clone() }));
    }
    outcomes
}

enum Prepared {
    Ready(Arc<dyn Tool>, ToolCall),
    Immediate(ToolOutcome),
}

/// prepare 阶段(03 文档 §5.4.1):按名找工具 → 参数校验 → beforeToolCall。
/// immediate 失败/拦截直接落定;成功返回待执行工具 + 生效参数(改参后重新校验)。
async fn prepare_call(call: &ToolCall, tools: &[Arc<dyn Tool>], hooks: &Arc<dyn LoopHooks>) -> Prepared {
    let Some(tool) = tools.iter().find(|tool| tool.name() == call.name) else {
        return Prepared::Immediate(ToolOutcome::Completed {
            call: call.clone(),
            output: ToolOutput::text(format!("Tool not found: {}", call.name)),
            is_error: true,
        });
    };

    if let Err(message) = validate_arguments(&tool.schema(), &call.args) {
        return Prepared::Immediate(ToolOutcome::Completed {
            call: call.clone(),
            output: ToolOutput::text(format!(
                "Invalid arguments for tool `{}`: {message}",
                call.name
            )),
            is_error: true,
        });
    }

    // beforeToolCall 钩子:block=true → 错误结果(可带 terminate);
    // block=false 且 args=Some → 以改参后的参数继续执行(07 §8.6)
    let mut effective = call.clone();
    if let Some(decision) = hooks
        .before_tool_call(ToolCallCtx {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            args: call.args.clone(),
        })
        .await
    {
        if decision.block {
            return Prepared::Immediate(ToolOutcome::Blocked {
                call: call.clone(),
                reason: decision.reason,
                terminate: decision.terminate.unwrap_or(false),
            });
        }
        if let Some(args) = decision.args {
            effective.args = args;
            // 改参来源在循环校验之后,重新过一遍 schema 校验再执行
            if let Err(message) = validate_arguments(&tool.schema(), &effective.args) {
                return Prepared::Immediate(ToolOutcome::Completed {
                    call: call.clone(),
                    output: ToolOutput::text(format!(
                        "Invalid arguments for tool `{}`: {message}",
                        call.name
                    )),
                    is_error: true,
                });
            }
        }
    }

    Prepared::Ready(tool.clone(), effective)
}

/// execute + finalize(03 文档 §5.4.2/.3):工具抛错 → 转错误结果;
/// afterToolCall 逐字段浅覆盖(None = 保持原值)。
async fn execute_and_finalize(
    call: &ToolCall,
    tool: Arc<dyn Tool>,
    hooks: Arc<dyn LoopHooks>,
    sink: Arc<dyn Subscriber>,
    cancel: CancellationToken,
) -> ToolOutcome {
    let updater = EventUpdater {
        sink: sink.clone(),
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
    };
    let mut output = match tool.execute(call.clone(), cancel, &updater).await {
        Ok(output) => output,
        Err(ToolError::Aborted { .. }) => return ToolOutcome::Cancelled { call: call.clone() },
        Err(ToolError::Failed { name, message }) => {
            return ToolOutcome::Completed {
                call: call.clone(),
                output: ToolOutput::text(format!("tool `{name}` failed: {message}")),
                is_error: true,
            };
        }
    };

    let mut is_error = false;
    if let Some(ToolPatch { output: patched_output, details, is_error: patched_is_error, terminate }) =
        hooks
            .after_tool_call(ToolResultCtx {
                tool_call_id: call.id.clone(),
                name: call.name.clone(),
                output: output.output.clone(),
                details: output.details.clone(),
                is_error,
                terminate: output.terminate,
            })
            .await
    {
        if let Some(text) = patched_output {
            output.output = text;
        }
        if let Some(details) = details {
            output.details = details;
        }
        if let Some(flag) = patched_is_error {
            is_error = flag;
        }
        if let Some(flag) = terminate {
            output.terminate = flag;
        }
    }

    ToolOutcome::Completed { call: call.clone(), output, is_error }
}

fn outcome_to_message(outcome: &ToolOutcome) -> AgentMessage {
    let call = outcome.call();
    match outcome {
        ToolOutcome::Completed { output, is_error, .. } => AgentMessage::ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: vec![ContentBlock::text(output.output.clone())],
            details: Some(output.details.clone()),
            usage: None,
            is_error: *is_error,
            timestamp: now_ms(),
        },
        ToolOutcome::Cancelled { .. } => {
            AgentMessage::tool_result_text(&call.id, &call.name, "Operation aborted", true)
        }
        ToolOutcome::Blocked { reason, .. } => {
            AgentMessage::tool_result_text(&call.id, &call.name, reason.clone(), true)
        }
    }
}

async fn emit_tool_end(sink: &Arc<dyn Subscriber>, call: &ToolCall, output: &str, is_error: bool) {
    sink.on_event(&AgentEvent::ToolExecutionEnd {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        output: output.to_string(),
        is_error,
    })
    .await;
}

async fn emit_tool_end_for_outcome(sink: &Arc<dyn Subscriber>, outcome: &ToolOutcome) {
    let call = outcome.call();
    let (output, is_error) = match outcome {
        ToolOutcome::Completed { output, is_error, .. } => (output.output.clone(), *is_error),
        ToolOutcome::Cancelled { .. } => ("Operation aborted".to_string(), true),
        ToolOutcome::Blocked { reason, .. } => (reason.clone(), true),
    };
    emit_tool_end(sink, call, &output, is_error).await;
}

/// 参数校验(JSON Schema 子集:type / required / 嵌套 properties)。
/// 完整 JSON Schema 校验待引入 jsonschema crate 后替换(09 B2)。
pub fn validate_arguments(schema: &serde_json::Value, args: &serde_json::Value) -> Result<(), String> {
    use serde_json::Value;
    let Some(schema_obj) = schema.as_object() else {
        return Ok(());
    };
    if let Some(expected) = schema_obj.get("type").and_then(Value::as_str) {
        let ok = match expected {
            "object" => args.is_object(),
            "string" => args.is_string(),
            "number" | "integer" => args.is_number(),
            "boolean" => args.is_boolean(),
            "array" => args.is_array(),
            "null" => args.is_null(),
            _ => true,
        };
        if !ok {
            return Err(format!("expected {expected}, got {args}"));
        }
    }
    if let Some(required) = schema_obj.get("required").and_then(Value::as_array) {
        for key in required {
            if let Some(key) = key.as_str() {
                if args.get(key).map(Value::is_null).unwrap_or(true) {
                    return Err(format!("missing required argument `{key}`"));
                }
            }
        }
    }
    if let Some(properties) = schema_obj.get("properties").and_then(Value::as_object) {
        if let Some(args_obj) = args.as_object() {
            for (key, property_schema) in properties {
                if let Some(value) = args_obj.get(key) {
                    if !value.is_null() {
                        validate_arguments(property_schema, value)?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_validation_checks_type_required_and_nesting() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer"}
            }
        });
        assert!(validate_arguments(&schema, &serde_json::json!({"path": "a.txt"})).is_ok());
        assert!(validate_arguments(&schema, &serde_json::json!({"path": "a", "offset": 3})).is_ok());
        // 缺 required
        assert!(validate_arguments(&schema, &serde_json::json!({})).is_err());
        // 类型错误
        assert!(validate_arguments(&schema, &serde_json::json!({"path": 1})).is_err());
        assert!(validate_arguments(&schema, &serde_json::json!({"path": "a", "offset": "x"})).is_err());
        // 非 object 顶层
        assert!(validate_arguments(&schema, &serde_json::json!("boom")).is_err());
        // 无 schema 不校验
        assert!(validate_arguments(&serde_json::Value::Null, &serde_json::json!(1)).is_ok());
    }
}
