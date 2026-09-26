//! agent 循环(03 文档 §10.3 显式状态机 + §10.5 推送式注入)。
//!
//! 控制流是真实的 step 状态机:`Phase` 枚举 + `step_*` 转移函数,调度器退化
//! 为 match 循环;三种 `Wake`(steering / follow-up / 显式 continue)是不同
//! 类型,注入路径只有一条 —— pi 的 `pendingMessages` 单通道、补轮询与
//! continue 配额补丁(03 §10.2.2)由此消失。
//!
//! 注入通道是 mpsc(§10.5):循环在流式期间 `select!` 接收 steering(收到即
//! 缓存,下个 turn 边界注入 —— 真正的中途注入受 provider 限制仍做不到);
//! turn 边界按 `QueueMode` 批量整流。`error`/`aborted` 是唯一硬退出,不碰
//! 任何注入通道(不变量 I3)。
//!
//! 工具执行四阶段 prepare→execute→finalize→result,批结果以 `Vec<ToolOutcome>`
//! 返回 —— 长度恒等于 toolCall 数,"每个 toolCall 恰好一个 toolResult" 是类型
//! 不变量(03 文档 §10.4,修复 pi 串行 abort 缺口的类型化方案)。护栏
//! `TurnLimits` 超限以 `RunStop::BudgetExhausted` 可区分终止。低层无内建
//! provider 重试(不变量 I2)—— 重试经装饰 `Provider` 在上层注入。

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rpi_ai::{
    normalize_context, AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Model,
    Provider, StopReason, StreamOptions, ThinkingLevel, Tool as DeclaredTool, TranscriptContext,
};

use crate::agent::QueueMode;
use crate::declare::declare_tool_changes;
use crate::event::{AgentEvent, MessageDeltaPayload, SharedPartial, Subscriber};
use crate::hooks::{
    LoopHooks, ToolCallCtx, ToolPatch, ToolResultCtx, TurnCtx, TurnDecision, TurnUpdate,
};
use crate::message::{now_ms, AgentMessage};
use crate::tool::{Tool, ToolCall, ToolError, ToolExecution, ToolOutput, ToolUpdater};

// ---------------------------------------------------------------------------
// 注入通道(03 §10.5):发送端在宿主,接收端随 run 进入循环
// ---------------------------------------------------------------------------

/// 注入深度计数(UI/会话镜像;发送端递增,循环消费时递减)。
#[derive(Debug, Default)]
pub struct InjectionDepth {
    steering: AtomicUsize,
    follow_up: AtomicUsize,
}

impl InjectionDepth {
    pub fn steering(&self) -> usize {
        self.steering.load(Ordering::SeqCst)
    }

    pub fn follow_up(&self) -> usize {
        self.follow_up.load(Ordering::SeqCst)
    }
}

/// 注入发送端(宿主持有;run 期间也可 push,消息缓冲在通道里)。
#[derive(Clone)]
pub struct InjectionSender {
    steering: mpsc::UnboundedSender<AgentMessage>,
    follow_up: mpsc::UnboundedSender<AgentMessage>,
    depth: Arc<InjectionDepth>,
}

impl InjectionSender {
    pub fn steer(&self, message: AgentMessage) {
        let _ = self.steering.send(message);
        self.depth.steering.fetch_add(1, Ordering::SeqCst);
    }

    pub fn follow_up(&self, message: AgentMessage) {
        let _ = self.follow_up.send(message);
        self.depth.follow_up.fetch_add(1, Ordering::SeqCst);
    }

    /// 深度计数句柄(宿主镜像读口;与 receiver 在途与否无关)。
    pub fn depth(&self) -> Arc<InjectionDepth> {
        self.depth.clone()
    }

    /// 重新入队(不增计数):循环异常终止时把已取出未消费的消息放回通道
    /// (深度计数仍算在这些消息上,归还后镜像保持一致)。
    pub fn requeue_steering(&self, message: AgentMessage) {
        let _ = self.steering.send(message);
    }

    pub fn requeue_follow_up(&self, message: AgentMessage) {
        let _ = self.follow_up.send(message);
    }
}

/// 注入接收端(run 开始时移交给循环)。
pub struct InjectionReceiver {
    steering: mpsc::UnboundedReceiver<AgentMessage>,
    follow_up: mpsc::UnboundedReceiver<AgentMessage>,
    depth: Arc<InjectionDepth>,
}

impl InjectionReceiver {
    /// 深度镜像(steering, follow_up)。
    pub fn depths(&self) -> (usize, usize) {
        (self.depth.steering(), self.depth.follow_up())
    }

    fn dec_steering(&self) {
        self.depth.steering.fetch_sub(1, Ordering::SeqCst);
    }

    fn dec_follow_up(&self) {
        self.depth.follow_up.fetch_sub(1, Ordering::SeqCst);
    }

    /// 流式期间 select! 接收一条 steering(消费计数延迟到注入时递减)。
    pub async fn recv_steering(&mut self) -> Option<AgentMessage> {
        self.steering.recv().await
    }

    /// 取走通道中现存的全部 steering(进入本地缓冲)。
    pub fn poll_steering(&mut self, buffer: &mut Vec<AgentMessage>) {
        while let Ok(message) = self.steering.try_recv() {
            buffer.push(message);
        }
    }

    /// 非阻塞取一条 steering(one-at-a-time 批量模式;通道余量原地保留,
    /// 硬退出时不会丢失)。
    pub fn take_one_steering(&mut self) -> Option<AgentMessage> {
        self.steering.try_recv().ok()
    }

    /// 取走全部 follow-up(agent 本应停止时)。
    pub fn drain_follow_up(&mut self) -> Vec<AgentMessage> {
        let mut out = Vec::new();
        while let Ok(message) = self.follow_up.try_recv() {
            out.push(message);
        }
        out
    }

    /// 通道排空(仅限 idle 时清理)。
    pub fn clear(&mut self) {
        while self.steering.try_recv().is_ok() {
            self.dec_steering();
        }
        while self.follow_up.try_recv().is_ok() {
            self.dec_follow_up();
        }
        self.depth.steering.store(0, Ordering::SeqCst);
        self.depth.follow_up.store(0, Ordering::SeqCst);
    }
}

/// 工厂:创建注入通道端点对(发送端归宿主,接收端随 run 进循环)。
pub fn create_injection_endpoints() -> (InjectionSender, InjectionReceiver) {
    let (steering_tx, steering_rx) = mpsc::unbounded_channel();
    let (follow_up_tx, follow_up_rx) = mpsc::unbounded_channel();
    let depth = Arc::new(InjectionDepth::default());
    (
        InjectionSender { steering: steering_tx, follow_up: follow_up_tx, depth: depth.clone() },
        InjectionReceiver { steering: steering_rx, follow_up: follow_up_rx, depth },
    )
}

// ---------------------------------------------------------------------------
// 循环状态机(03 §10.3)
// ---------------------------------------------------------------------------

/// 唤醒原因(03 §10.3):三种类型不同,注入路径只有一条。
#[derive(Debug, Clone, PartialEq)]
pub enum Wake {
    /// turn 边界收到的 steering(携带首条,其余仍在本地缓冲/通道)。
    /// 当前实现里 steering 走自然续跑(wake=None)注入,本变体保留以对齐
    /// 03 §10.3 的 Wake 三类型(供显式 wake 语义扩展使用)。
    Steering(AgentMessage),
    /// agent 本应停止时收到的 follow-up(携带首条)
    FollowUp(AgentMessage),
    /// finishTurn 的显式 continue(无新消息的"仅上下文"一轮)
    ExplicitContinue,
}

/// 显式状态阶段(03 文档 §10.3):真实控制流,`step_*` 是转移函数。
#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// 即将注入消息并发起请求(Box 压缩变体尺寸:调度器每轮 match 复制 Phase)
    AwaitingRequest { wake: Option<Box<Wake>> },
    /// LLM 流式中
    Streaming,
    /// 工具批执行中
    ExecutingTools,
    /// turn 收尾决策点
    Settling,
    /// 终态(调度器退出)
    Done(RunStop),
}

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
    /// steering 批量模式(03 文档 QueueMode;Agent 默认 one-at-a-time)
    pub steering_mode: QueueMode,
}

impl LoopConfig {
    pub fn new(model: Model) -> Self {
        LoopConfig {
            model,
            thinking: None,
            limits: TurnLimits::default(),
            stream_options: StreamOptions::default(),
            steering_mode: QueueMode::OneAtATime,
        }
    }
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
    /// run 终止时"已从通道取出但未消费"的注入消息(硬退出/预算耗尽路径),
    /// 宿主应经 InjectionSender::requeue_* 放回 —— 已消费即丢失会违反 I3 精神。
    pub requeued_steering: Vec<AgentMessage>,
    pub requeued_follow_up: Vec<AgentMessage>,
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

/// 状态机的全部可变状态(03 §10.3:状态只有 LoopState 一份)。
struct LoopState {
    hooks: Arc<dyn LoopHooks>,
    provider: Arc<dyn Provider>,
    sink: Arc<dyn Subscriber>,
    cancel: CancellationToken,
    system: Option<String>,
    tools: Vec<Arc<dyn Tool>>,
    stream_options: StreamOptions,
    limits: TurnLimits,
    steering_mode: QueueMode,

    model: Model,
    thinking: Option<ThinkingLevel>,
    /// 模型可见转录(单一真相,不变量 I1)
    current: Vec<AgentMessage>,
    new_messages: Vec<AgentMessage>,
    last_turn: Option<(AssistantMessage, Vec<AgentMessage>)>,

    /// 流式期间 select! 收到的 steering(注入路径的本地缓冲)
    deferred_steering: Vec<AgentMessage>,
    /// 首轮一次性载荷(prompts;与批量通道分开,不受 QueueMode 影响)
    initial_prompts: Vec<AgentMessage>,
    /// agent 本应停止时整流出的 follow-up(注入前暂存)
    follow_up_batch: Vec<AgentMessage>,
    receiver: InjectionReceiver,

    /// 当前 turn 的活动 assistant 消息(Streaming → ExecutingTools → Settling 载荷)
    active_message: Option<AssistantMessage>,
    tool_batch: Option<ToolBatch>,

    first_turn: bool,
    turn_count: u32,
    tool_call_count: u32,
    total_tokens: u64,
    truncation_turns: u32,
}

impl LoopState {
    fn budget_stop(&self) -> Option<RunStop> {
        if self.limits.max_turns.is_some_and(|max| self.turn_count >= max) {
            return Some(RunStop::BudgetExhausted(BudgetKind::MaxTurns));
        }
        if self.limits.max_tool_calls.is_some_and(|max| self.tool_call_count >= max) {
            return Some(RunStop::BudgetExhausted(BudgetKind::MaxToolCalls));
        }
        if self.limits.max_total_tokens.is_some_and(|max| self.total_tokens >= max) {
            return Some(RunStop::BudgetExhausted(BudgetKind::MaxTotalTokens));
        }
        if self
            .limits
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            return Some(RunStop::BudgetExhausted(BudgetKind::Deadline));
        }
        if self.truncation_turns > self.limits.max_truncation_retries {
            return Some(RunStop::BudgetExhausted(BudgetKind::TruncationRetries));
        }
        None
    }

    /// 注入一条消息(message_start/message_end 事件 + 转录 + new_messages)。
    async fn inject(&mut self, message: AgentMessage) {
        emit_message_events(&self.sink, &message).await;
        self.current.push(message.clone());
        self.new_messages.push(message);
    }

    /// turn 边界取 steering 批(one-at-a-time 只取一条;通道余量原地保留,
    /// 硬退出时不会丢失)。
    async fn take_steering_batch(&mut self) -> Vec<AgentMessage> {
        let mut batch = Vec::new();
        match self.steering_mode {
            QueueMode::All => {
                self.receiver.poll_steering(&mut self.deferred_steering);
                batch.append(&mut self.deferred_steering);
            }
            QueueMode::OneAtATime => {
                if self.deferred_steering.is_empty() {
                    if let Some(message) = self.receiver.take_one_steering() {
                        batch.push(message);
                    }
                } else {
                    batch.push(self.deferred_steering.remove(0));
                }
            }
        }
        for _ in &batch {
            self.receiver.dec_steering();
        }
        batch
    }

    /// turn 边界收集注入内容:prepared + wake 载荷 + follow-up 批 + steering 批。
    /// follow-up 消费计数在本函数递减(注入时点;requeue 路径不重复计)。
    async fn collect_injectables(
        &mut self,
        wake: Option<Box<Wake>>,
        prepared: Vec<AgentMessage>,
    ) -> Vec<AgentMessage> {
        let mut injectables = prepared;
        match wake.map(|boxed| *boxed) {
            Some(Wake::Steering(message)) => {
                self.receiver.dec_steering();
                injectables.push(message);
            }
            Some(Wake::FollowUp(message)) => {
                self.receiver.dec_follow_up();
                injectables.push(message);
            }
            Some(Wake::ExplicitContinue) | None => {}
        }
        // follow-up 批量模式:All 整批,OneAtATime 每轮一条(余量原地保留)
        match self.steering_mode {
            QueueMode::All => {
                for _ in 0..self.follow_up_batch.len() {
                    self.receiver.dec_follow_up();
                }
                injectables.append(&mut self.follow_up_batch);
            }
            QueueMode::OneAtATime => {
                if !self.follow_up_batch.is_empty() {
                    let message = self.follow_up_batch.remove(0);
                    self.receiver.dec_follow_up();
                    injectables.push(message);
                }
            }
        }
        let steering = self.take_steering_batch().await;
        injectables.extend(steering);
        injectables
    }
}

/// 主循环入口(调度器:状态只有一份,Phase 驱动 step 转移;快照进、事件出,
/// 无可变全局状态,09 A4)。
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    hooks: Arc<dyn LoopHooks>,
    config: LoopConfig,
    provider: Arc<dyn Provider>,
    sink: Arc<dyn Subscriber>,
    cancel: CancellationToken,
    receiver: InjectionReceiver,
) -> (LoopOutput, InjectionReceiver) {
    let mut state = LoopState {
        hooks,
        provider,
        sink: sink.clone(),
        cancel,
        system: context.system,
        tools: context.tools,
        stream_options: config.stream_options.clone(),
        limits: config.limits,
        steering_mode: config.steering_mode,
        model: config.model,
        thinking: config.thinking,
        current: context.messages,
        new_messages: Vec::new(),
        last_turn: None,
        deferred_steering: Vec::new(),
        initial_prompts: prompts,
        follow_up_batch: Vec::new(),
        receiver,
        active_message: None,
        tool_batch: None,
        first_turn: true,
        turn_count: 0,
        tool_call_count: 0,
        total_tokens: 0,
        truncation_turns: 0,
    };

    sink.on_event(&AgentEvent::AgentStart).await;

    // 初始 prompts 作为首轮注入载荷(独立于批量通道,不受 QueueMode 影响)
    let mut phase = Phase::AwaitingRequest { wake: None };
    let stop = loop {
        phase = match phase {
            Phase::AwaitingRequest { wake } => step_awaiting_request(&mut state, wake).await,
            Phase::Streaming => step_streaming(&mut state).await,
            Phase::ExecutingTools => step_executing_tools(&mut state).await,
            Phase::Settling => step_settling(&mut state).await,
            Phase::Done(stop) => break stop,
        };
    };

    // agent_end 是 run 的最后事件(此前全部 listener 已串行完成,结算语义成立)
    sink.on_event(&AgentEvent::AgentEnd { messages: state.new_messages.clone() }).await;
    // 硬退出/预算路径:已取出未消费的注入消息交还宿主重新入队(不静默丢失)
    let requeued_steering = std::mem::take(&mut state.deferred_steering);
    let requeued_follow_up = std::mem::take(&mut state.follow_up_batch);
    (
        LoopOutput { messages: state.new_messages, stop, requeued_steering, requeued_follow_up },
        state.receiver,
    )
}

/// AwaitingRequest:预算检查 → prepareNextTurn → TurnStart → 注入 → prepareRequest。
async fn step_awaiting_request(state: &mut LoopState, wake: Option<Box<Wake>>) -> Phase {
    // 预算检查在 drain/TurnStart 之前:耗尽时不丢已入队的消息、不发无法配对的
    // TurnStart(03 文档护栏语义)
    if let Some(budget) = state.budget_stop() {
        return Phase::Done(budget);
    }

    let mut prepared: Vec<AgentMessage> = Vec::new();
    if state.first_turn {
        state.first_turn = false;
    } else if let Some((message, tool_results)) = state.last_turn.clone() {
        // prepareNextTurn:可追加消息、切换 model/thinking(03 §3 ②)
        let update = state
            .hooks
            .prepare_next_turn(TurnCtx {
                message: Box::new(message),
                tool_results: tool_results.clone(),
                new_messages: state.new_messages.clone(),
            })
            .await;
        if let Some(TurnUpdate { messages, model: m, thinking_level: t }) = update {
            if let Some(messages) = messages {
                prepared = messages;
            }
            if let Some(m) = m {
                state.model = m;
            }
            if let Some(t) = t {
                state.thinking = t;
            }
        }
    }

    state.sink.on_event(&AgentEvent::TurnStart).await;

    // 注入(初始 prompts + prepared + wake 载荷 + follow-up/steering 批),
    // 注入前声明工具集增量
    let mut injectables = std::mem::take(&mut state.initial_prompts);
    injectables.extend(state.collect_injectables(wake, prepared).await);
    for message in declare_tool_changes(&state.current, &state.tools, injectables) {
        state.inject(message).await;
    }

    // prepareRequest(每次请求前,含第一次)
    if let Some(update) = state.hooks.prepare_request(&state.model, state.thinking).await {
        if let Some(m) = update.model {
            state.model = m;
        }
        if let Some(t) = update.thinking_level {
            state.thinking = t;
        }
    }

    Phase::Streaming
}

/// Streaming:流式请求 + select! 接收 steering;终态后进入工具/硬退出决策。
async fn step_streaming(state: &mut LoopState) -> Phase {
    let message = stream_assistant_response(
        &state.current,
        state.system.as_deref(),
        &state.tools,
        &state.hooks,
        &state.model,
        state.thinking,
        &state.stream_options,
        &state.provider,
        &state.sink,
        &state.cancel,
        &mut state.receiver,
        &mut state.deferred_steering,
    )
    .await;
    state.total_tokens = state.total_tokens.saturating_add(message.usage.total_tokens);
    state.turn_count += 1;
    if message.stop_reason == StopReason::Length && message.has_tool_calls() {
        state.truncation_turns += 1;
    }

    // 硬退出:error / aborted —— 不执行工具、不碰任何注入通道(不变量 I3);
    // finishTurn 仍会调用,但返回的决策被忽略(03 §3 ④)
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        let _ignored = state
            .hooks
            .finish_turn(TurnCtx {
                message: Box::new(message.clone()),
                tool_results: Vec::new(),
                new_messages: state.new_messages.clone(),
            })
            .await;
        state
            .sink
            .on_event(&AgentEvent::TurnEnd {
                message: Box::new(message.clone()),
                tool_results: Vec::new(),
            })
            .await;
        state.current.push(AgentMessage::Assistant(Box::new(message.clone())));
        state.new_messages.push(AgentMessage::Assistant(Box::new(message.clone())));
        return Phase::Done(match message.stop_reason {
            StopReason::Aborted => RunStop::Aborted,
            _ => RunStop::Error(
                message.error_message.clone().unwrap_or_else(|| "provider error".into()),
            ),
        });
    }

    state.current.push(AgentMessage::Assistant(Box::new(message.clone())));
    state.new_messages.push(AgentMessage::Assistant(Box::new(message.clone())));
    state.active_message = Some(message);
    Phase::ExecutingTools
}

/// ExecutingTools:截断防御或工具批执行;批执行中 abort → aborted 硬退出。
async fn step_executing_tools(state: &mut LoopState) -> Phase {
    let message = state.active_message.clone().expect("active message set by Streaming");
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

    // 无工具调用:terminate=true 使 has_more_tool_calls=false,内层自然停止
    // (pi :266-268 的 hasMoreToolCalls=false 语义)
    let batch = if calls.is_empty() {
        ToolBatch { messages: Vec::new(), terminate: true }
    } else {
        state.tool_call_count += calls.len() as u32;
        if message.stop_reason == StopReason::Length {
            // 截断防御:全部拒执行,terminate=false 让模型重发(不变量 I5)
            fail_tool_calls_from_truncated(
                &calls,
                &state.sink,
                &mut state.current,
                &mut state.new_messages,
            )
            .await
        } else {
            execute_tool_calls(
                &calls,
                &state.tools,
                &state.hooks,
                &state.sink,
                &state.cancel,
                &mut state.current,
                &mut state.new_messages,
            )
            .await
        }
    };

    // 工具批执行期间 abort:结果已配对结算,run 以 aborted 硬退出(不碰队列,I3)
    if state.cancel.is_cancelled() {
        state
            .sink
            .on_event(&AgentEvent::TurnEnd {
                message: Box::new(message.clone()),
                tool_results: batch.messages,
            })
            .await;
        return Phase::Done(RunStop::Aborted);
    }

    state.tool_batch = Some(batch);
    Phase::Settling
}

/// Settling:finishTurn → turn_end → 预算/决策 → 下一个 Phase(wake 显式化)。
async fn step_settling(state: &mut LoopState) -> Phase {
    let message = state.active_message.clone().expect("active message set by Streaming");
    let batch = state.tool_batch.take().expect("tool batch set by ExecutingTools");
    let has_more_tool_calls = !batch.terminate;

    state.last_turn = Some((message.clone(), batch.messages.clone()));
    let decision = state
        .hooks
        .finish_turn(TurnCtx {
            message: Box::new(message.clone()),
            tool_results: batch.messages.clone(),
            new_messages: state.new_messages.clone(),
        })
        .await;
    state
        .sink
        .on_event(&AgentEvent::TurnEnd {
            message: Box::new(message.clone()),
            tool_results: batch.messages,
        })
        .await;

    if let Some(budget) = state.budget_stop() {
        return Phase::Done(budget);
    }
    if decision == Some(TurnDecision::End) {
        return Phase::Done(RunStop::EndTurn);
    }

    // 自然请求:工具批继续,或 steering 已在途 —— continue 配额被自然消耗
    state.receiver.poll_steering(&mut state.deferred_steering);
    let natural = has_more_tool_calls
        || !state.deferred_steering.is_empty()
        || !state.follow_up_batch.is_empty();
    if natural {
        return Phase::AwaitingRequest { wake: None };
    }

    // agent 本应停止:follow-up 优先于显式 continue(pi :301);
    // 首条经 wake 注入,余量按 QueueMode 逐轮消费(collect_injectables)
    let follow_ups = state.receiver.drain_follow_up();
    if !follow_ups.is_empty() {
        state.follow_up_batch = follow_ups;
        let first = state.follow_up_batch.remove(0);
        return Phase::AwaitingRequest { wake: Some(Box::new(Wake::FollowUp(first))) };
    }
    if decision == Some(TurnDecision::Continue) {
        // 无自然请求时,用"仅上下文"的一轮兑现 continue(显式 wake,非补丁)
        return Phase::AwaitingRequest { wake: Some(Box::new(Wake::ExplicitContinue)) };
    }
    Phase::Done(RunStop::EndTurn)
}

async fn emit_message_events(sink: &Arc<dyn Subscriber>, message: &AgentMessage) {
    sink.on_event(&AgentEvent::MessageStart { message: Box::new(message.clone()), partial: None }).await;
    sink.on_event(&AgentEvent::MessageEnd { message: Box::new(message.clone()) }).await;
}

/// 流式处理(03 文档 §4):折叠转录 → 请求 → 事件转发 → 终态消息。
/// partial 不占转录末位(09 B5.2 决策):增量经类型化 `MessageDelta` 转发
/// (T2:thinking/toolCall 参数逐块可见),同时原地更新 `SharedPartial`
/// 快照读口供 UI 随帧取用;终态后 `MessageUpdate`(快照)+ `MessageEnd`。
/// 流式期间 `select!` 接收 steering(§10.5 推送式注入):收到即缓存到
/// `deferred`,当前 turn 结束后注入。
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
    receiver: &mut InjectionReceiver,
    deferred: &mut Vec<AgentMessage>,
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
    let shared_partial: SharedPartial = Arc::new(RwLock::new(AssistantMessage::pending(model)));
    let mut toolcall_json: HashMap<usize, String> = HashMap::new();
    let mut started = false;
    let mut final_message: Option<AssistantMessage> = None;
    let mut steering_closed = false;

    loop {
        // 推送式注入:流式期间收到 steering 即缓存(03 §10.5 select! 草图)
        let event = if steering_closed {
            stream.next().await
        } else {
            tokio::select! {
                event = stream.next() => event,
                message = receiver.recv_steering() => {
                    match message {
                        Some(message) => {
                            deferred.push(message);
                            continue;
                        }
                        None => {
                            // 通道关闭且为空:退出 select(防忙等)
                            steering_closed = true;
                            continue;
                        }
                    }
                }
            }
        };
        let Some(event) = event else { break };
        match event {
            AssistantMessageEvent::Start => {
                started = true;
                sink.on_event(&AgentEvent::MessageStart {
                    message: Box::new(AgentMessage::Assistant(Box::new(AssistantMessage::pending(model)))),
                    partial: Some(shared_partial.clone()),
                })
                .await;
            }
            AssistantMessageEvent::TextDelta { ref delta, .. } => {
                {
                    let mut partial = shared_partial.write().unwrap_or_else(|poisoned| poisoned.into_inner());
                    apply_event_to_partial(&mut partial, &mut toolcall_json, &event);
                }
                sink.on_event(&AgentEvent::MessageDelta {
                    delta: MessageDeltaPayload::Text { delta: delta.clone() },
                })
                .await;
            }
            AssistantMessageEvent::ThinkingDelta { ref delta, .. } => {
                {
                    let mut partial = shared_partial.write().unwrap_or_else(|poisoned| poisoned.into_inner());
                    apply_event_to_partial(&mut partial, &mut toolcall_json, &event);
                }
                sink.on_event(&AgentEvent::MessageDelta {
                    delta: MessageDeltaPayload::Thinking { delta: delta.clone() },
                })
                .await;
            }
            AssistantMessageEvent::ToolCallDelta { content_index, ref delta } => {
                {
                    let mut partial = shared_partial.write().unwrap_or_else(|poisoned| poisoned.into_inner());
                    apply_event_to_partial(&mut partial, &mut toolcall_json, &event);
                }
                sink.on_event(&AgentEvent::MessageDelta {
                    delta: MessageDeltaPayload::ToolCallArgs { content_index, delta: delta.clone() },
                })
                .await;
            }
            AssistantMessageEvent::Done(message) | AssistantMessageEvent::Error(message) => {
                if !started {
                    sink.on_event(&AgentEvent::MessageStart {
                        message: Box::new(AgentMessage::Assistant(Box::new(AssistantMessage::pending(model)))),
                        partial: Some(shared_partial.clone()),
                    })
                    .await;
                }
                sink.on_event(&AgentEvent::MessageUpdate { message: message.clone() }).await;
                sink.on_event(&AgentEvent::MessageEnd {
                    message: Box::new(AgentMessage::Assistant(message.clone())),
                })
                .await;
                final_message = Some(*message);
                break;
            }
            // *_start/*_end:权威内容随终态快照;这里仅更新 partial 读口
            AssistantMessageEvent::TextStart { .. }
            | AssistantMessageEvent::TextEnd { .. }
            | AssistantMessageEvent::ThinkingStart { .. }
            | AssistantMessageEvent::ThinkingEnd { .. }
            | AssistantMessageEvent::ToolCallStart { .. }
            | AssistantMessageEvent::ToolCallEnd { .. } => {
                let mut partial = shared_partial.write().unwrap_or_else(|poisoned| poisoned.into_inner());
                apply_event_to_partial(&mut partial, &mut toolcall_json, &event);
            }
        }
    }

    match final_message {
        Some(message) => message,
        // 流意外正常退出(契约要求终态事件;兜底防御)
        None => {
            let message = AssistantMessage::error(model, "stream ended without a terminal event", false);
            sink.on_event(&AgentEvent::MessageStart {
                message: Box::new(AgentMessage::Assistant(Box::new(AssistantMessage::pending(model)))),
                partial: Some(shared_partial.clone()),
            })
            .await;
            // 与终态路径事件序一致:update(快照) 先于 end
            sink.on_event(&AgentEvent::MessageUpdate { message: Box::new(message.clone()) }).await;
            sink.on_event(&AgentEvent::MessageEnd {
                message: Box::new(AgentMessage::Assistant(Box::new(message.clone()))),
            })
            .await;
            message
        }
    }
}

/// 把统一流事件应用到 partial 快照(T2:快照读口的写侧)。
/// text/thinking 从 `*_start` 空串随 `*_delta` 增长,`*_end` 权威定稿;
/// toolcall 参数从 `toolcall_delta` 尽力解析(与适配器修复解析同语义,
/// 这里仅供 UI 预览,终态以 provider 定稿为准)。
fn apply_event_to_partial(
    partial: &mut AssistantMessage,
    toolcall_json: &mut HashMap<usize, String>,
    event: &AssistantMessageEvent,
) {
    match event {
        AssistantMessageEvent::TextStart { content_index } => {
            ensure_block(partial, *content_index, || ContentBlock::text(String::new()));
        }
        AssistantMessageEvent::TextDelta { content_index, delta } => {
            if let Some(ContentBlock::Text { text, .. }) = partial.content.get_mut(*content_index) {
                text.push_str(delta);
            }
        }
        AssistantMessageEvent::TextEnd { content_index, content } => {
            if let Some(ContentBlock::Text { text, .. }) = partial.content.get_mut(*content_index) {
                *text = content.clone();
            }
        }
        AssistantMessageEvent::ThinkingStart { content_index } => {
            ensure_block(partial, *content_index, || ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: None,
                redacted: None,
            });
        }
        AssistantMessageEvent::ThinkingDelta { content_index, delta } => {
            if let Some(ContentBlock::Thinking { thinking, .. }) = partial.content.get_mut(*content_index) {
                thinking.push_str(delta);
            }
        }
        AssistantMessageEvent::ThinkingEnd { content_index, content } => {
            if let Some(ContentBlock::Thinking { thinking, .. }) = partial.content.get_mut(*content_index) {
                *thinking = content.clone();
            }
        }
        AssistantMessageEvent::ToolCallStart { content_index } => {
            ensure_block(partial, *content_index, || ContentBlock::ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: serde_json::json!({}),
            });
            toolcall_json.entry(*content_index).or_default().clear();
        }
        AssistantMessageEvent::ToolCallDelta { content_index, delta } => {
            let raw = toolcall_json.entry(*content_index).or_default();
            raw.push_str(delta);
            if let Some(ContentBlock::ToolCall { arguments, .. }) = partial.content.get_mut(*content_index) {
                *arguments = serde_json::from_str(raw)
                    .unwrap_or_else(|_| serde_json::Value::String(raw.clone()));
            }
        }
        AssistantMessageEvent::ToolCallEnd { content_index, tool_call } => {
            if partial.content.len() > *content_index {
                partial.content[*content_index] = tool_call.clone();
            } else {
                partial.content.push(tool_call.clone());
            }
            toolcall_json.remove(content_index);
        }
        _ => {}
    }
}

/// 确保 partial.content[index] 存在(适配器 content_index 稠密;防御性补位)。
/// 补位块是空 Text;若目标槽位仍是补位空 Text 则替换为新块。
fn ensure_block(partial: &mut AssistantMessage, index: usize, make: impl FnOnce() -> ContentBlock) {
    while partial.content.len() <= index {
        partial.content.push(ContentBlock::text(String::new()));
    }
    if partial.content[index] == ContentBlock::text(String::new()) {
        partial.content[index] = make();
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

/// 参数校验(T7,11 计划):`jsonschema` crate 完整 JSON Schema 校验,替换原
/// type/required/嵌套 properties 子集。schema 未声明(Value::Null / boolean true)
/// 不校验;schema 本身非法(compile 失败)fail-closed 返回 Err。错误语义不变:
/// Err(String) 由 prepare_call 转错误 ToolOutcome,不 panic、不改 Tool trait。
pub fn validate_arguments(schema: &serde_json::Value, args: &serde_json::Value) -> Result<(), String> {
    // 工具未声明 schema(或恒真 schema):无约束,不校验(既有测试钉住)
    if schema.is_null() || schema == &serde_json::Value::Bool(true) {
        return Ok(());
    }
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("invalid tool schema: {error}"))?;
    let mut errors = validator.iter_errors(args);
    if let Some(first) = errors.next() {
        return Err(first.to_string());
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

    #[test]
    fn argument_validation_rejects_enum_range_and_array_items() {
        // T7:此前子集实现放过的三类非法参数,现在执行前拦截
        let enum_schema = serde_json::json!({"type": "string", "enum": ["a", "b"]});
        assert!(validate_arguments(&enum_schema, &serde_json::json!("a")).is_ok());
        assert!(validate_arguments(&enum_schema, &serde_json::json!("c")).is_err());

        let minimum_schema = serde_json::json!({"type": "integer", "minimum": 1});
        assert!(validate_arguments(&minimum_schema, &serde_json::json!(1)).is_ok());
        assert!(validate_arguments(&minimum_schema, &serde_json::json!(0)).is_err());

        let array_schema =
            serde_json::json!({"type": "array", "items": {"type": "string"}});
        assert!(validate_arguments(&array_schema, &serde_json::json!(["x"])).is_ok());
        assert!(validate_arguments(&array_schema, &serde_json::json!(["x", 1])).is_err());
    }

    #[test]
    fn invalid_schema_fails_closed() {
        // T7:schema 本身非法 → Err(fail-closed),由调用方转错误 ToolOutcome
        let broken = serde_json::json!({"type": "not-a-json-schema-type"});
        assert!(validate_arguments(&broken, &serde_json::json!({})).is_err());
    }

    #[test]
    fn argument_validation_common_tool_schema_shapes() {
        // T7 坑位:内置 8 工具同形的 schema 形状必须照常通过(jsonschema 兼容性;
        // 真实 schema 的端到端断言在 rpi-tools 侧,依赖方向不允许反向引用)
        let schemas: Vec<serde_json::Value> = vec![
            serde_json::json!({"type": "object", "required": ["command"], "properties": {"command": {"type": "string"}, "timeout": {"type": "integer"}}}),
            serde_json::json!({"type": "object", "required": ["path", "edits"], "properties": {"path": {"type": "string"}, "edits": {"type": "array", "items": {"type": "object", "required": ["oldText", "newText"], "properties": {"oldText": {"type": "string"}, "newText": {"type": "string"}}}}}}),
            serde_json::json!({"type": "object", "required": ["path", "content"], "properties": {"path": {"type": "string"}, "content": {"type": "string"}}}),
        ];
        for schema in &schemas {
            assert!(validate_arguments(schema, &serde_json::json!({})).is_err(), "缺 required 应拦截: {schema}");
        }
        // 合法参数照常通过
        assert!(validate_arguments(&schemas[0], &serde_json::json!({"command": "ls"})).is_ok());
        assert!(validate_arguments(&schemas[1], &serde_json::json!({"path": "a", "edits": [{"oldText": "x", "newText": "y"}]})).is_ok());
        assert!(validate_arguments(&schemas[2], &serde_json::json!({"path": "a", "content": "b"})).is_ok());
    }

    #[test]
    fn injection_endpoints_track_depth() {
        let (sender, mut receiver) = create_injection_endpoints();
        sender.steer(AgentMessage::user("a"));
        sender.steer(AgentMessage::user("b"));
        sender.follow_up(AgentMessage::user("c"));
        assert_eq!(receiver.depth.steering(), 2);
        assert_eq!(receiver.depth.follow_up(), 1);
        let drained = receiver.drain_follow_up();
        assert_eq!(drained.len(), 1);
        receiver.clear();
        assert_eq!(receiver.depth.steering(), 0);
        assert_eq!(receiver.depth.follow_up(), 0);
    }
}
