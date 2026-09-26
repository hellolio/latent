//! `Agent` 有状态薄壳(pi 的 agent.ts,03 文档 §8):队列、订阅、run 生命周期。
//!
//! 循环是纯函数式的(快照进、事件出),Agent 持有状态并作为事件 sink:
//! reducer 更新 `state`(message_end push,双份真相由"循环操作快照、公开状态
//! 事件驱动"消解),随后按订阅顺序串行 await 订阅者。steering/followUp 队列经
//! 内部 adapter 绑定进 `LoopHooks`(pi 的 createLoopConfig 等价物,09 A2 接缝 2)。

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use futures::FutureExt;
use rpi_ai::{Model, Provider, StreamOptions, ThinkingLevel};
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, SharedSubscriber, Subscriber};
use crate::hooks::LoopHooks;
use crate::loop_::{run_agent_loop, AgentContext, LoopConfig, RunStop, TurnLimits};
use crate::message::AgentMessage;
use crate::tool::Tool;

/// 队列抽取模式(01 文档 QueueMode);Agent 默认两者都是 one-at-a-time(pi :247-248)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueueMode {
    #[default]
    All,
    OneAtATime,
}

#[derive(Default)]
struct Queues {
    steering: Vec<AgentMessage>,
    follow_up: Vec<AgentMessage>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl Queues {
    fn drain_steering(&mut self) -> Vec<AgentMessage> {
        match self.steering_mode {
            QueueMode::All => std::mem::take(&mut self.steering),
            QueueMode::OneAtATime => self.steering.drain(..1.min(self.steering.len())).collect(),
        }
    }

    fn drain_follow_up(&mut self) -> Vec<AgentMessage> {
        match self.follow_up_mode {
            QueueMode::All => std::mem::take(&mut self.follow_up),
            QueueMode::OneAtATime => self.follow_up.drain(..1.min(self.follow_up.len())).collect(),
        }
    }

    fn drain_all(&mut self) -> (Vec<AgentMessage>, Vec<AgentMessage>) {
        (std::mem::take(&mut self.steering), std::mem::take(&mut self.follow_up))
    }
}

#[derive(Default)]
pub struct AgentState {
    /// 基础系统提示词(转录重放状态由 core 的 sections 机制维护,04 文档 §3.3)
    pub system: Option<String>,
    pub model: Option<Model>,
    /// None = 不请求思考(01 文档:ai 侧级别无 off)
    pub thinking_level: Option<ThinkingLevel>,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn Tool>>,
    /// 流式中的 partial 消息快照(message_end 后清空)
    pub streaming_message: Option<rpi_ai::AssistantMessage>,
    pub pending_tool_calls: HashSet<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent is already running; use steer/follow_up or wait for idle")]
    AlreadyRunning,
    #[error("no model installed")]
    NoModel,
    #[error("nothing to continue: queue is empty and last message is not user/toolResult")]
    NothingToContinue,
    #[error("run panicked")]
    Panicked,
}

/// 队列 drain 与用户 hooks 的绑定(pi 的 createLoopConfig;09 B3:
/// "steering_messages() 默认实现即 self.steering.drain()")。
struct AgentHookAdapter {
    inner: Arc<dyn LoopHooks>,
    queues: Arc<Mutex<Queues>>,
    /// `continue()` 预载的注入消息(pi 的 skipInitialSteeringPoll 场景)
    preload: Mutex<Vec<AgentMessage>>,
}

impl AgentHookAdapter {
    fn take_preload(&self) -> Vec<AgentMessage> {
        std::mem::take(&mut *self.preload.lock().unwrap())
    }

    fn set_preload(&self, messages: Vec<AgentMessage>) {
        *self.preload.lock().unwrap() = messages;
    }
}

#[async_trait]
impl LoopHooks for AgentHookAdapter {
    fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
        self.inner.convert_to_llm(msgs)
    }

    async fn transform_context(&self, msgs: Vec<AgentMessage>) -> Vec<AgentMessage> {
        self.inner.transform_context(msgs).await
    }

    async fn get_api_key(&self, provider: &str) -> Option<String> {
        self.inner.get_api_key(provider).await
    }

    async fn prepare_request(
        &self,
        model: &Model,
        thinking: Option<ThinkingLevel>,
    ) -> Option<crate::hooks::RequestUpdate> {
        self.inner.prepare_request(model, thinking).await
    }

    async fn prepare_next_turn(
        &self,
        ctx: crate::hooks::TurnCtx,
    ) -> Option<crate::hooks::TurnUpdate> {
        self.inner.prepare_next_turn(ctx).await
    }

    async fn finish_turn(&self, ctx: crate::hooks::TurnCtx) -> Option<crate::hooks::TurnDecision> {
        self.inner.finish_turn(ctx).await
    }

    async fn steering_messages(&self) -> Vec<AgentMessage> {
        let mut messages = self.take_preload();
        messages.extend(self.queues.lock().unwrap().drain_steering());
        messages
    }

    async fn follow_up_messages(&self) -> Vec<AgentMessage> {
        self.queues.lock().unwrap().drain_follow_up()
    }

    async fn before_tool_call(&self, ctx: crate::hooks::ToolCallCtx) -> Option<crate::hooks::ToolBlock> {
        self.inner.before_tool_call(ctx).await
    }

    async fn after_tool_call(&self, ctx: crate::hooks::ToolResultCtx) -> Option<crate::hooks::ToolPatch> {
        self.inner.after_tool_call(ctx).await
    }

    fn tool_execution(&self) -> crate::tool::ToolExecution {
        self.inner.tool_execution()
    }
}

pub struct Agent {
    state: Mutex<AgentState>,
    queues: Arc<Mutex<Queues>>,
    subscribers: Mutex<Vec<SharedSubscriber>>,
    provider: Arc<dyn Provider>,
    adapter: Arc<AgentHookAdapter>,
    limits: Mutex<TurnLimits>,
    stream_options: Mutex<StreamOptions>,
    streaming: AtomicBool,
    cancel: Mutex<CancellationToken>,
    self_weak: Weak<Agent>,
}

/// 工厂:上游只认 `Arc<Agent>`,实现细节不出厂(方针文档 §2 规则 1)。
pub fn create_agent(provider: Arc<dyn Provider>, hooks: Arc<dyn LoopHooks>) -> Arc<Agent> {
    Arc::new_cyclic(|weak| Agent::new(provider, hooks, weak.clone()))
}

impl Agent {
    fn new(provider: Arc<dyn Provider>, hooks: Arc<dyn LoopHooks>, self_weak: Weak<Agent>) -> Self {
        let queues = Arc::new(Mutex::new(Queues {
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            ..Queues::default()
        }));
        let adapter = Arc::new(AgentHookAdapter {
            inner: hooks.clone(),
            queues: queues.clone(),
            preload: Mutex::new(Vec::new()),
        });
        Self {
            state: Mutex::new(AgentState::default()),
            queues,
            subscribers: Mutex::new(Vec::new()),
            provider,
            adapter,
            limits: Mutex::new(TurnLimits::default()),
            stream_options: Mutex::new(StreamOptions::default()),
            streaming: AtomicBool::new(false),
            cancel: Mutex::new(CancellationToken::new()),
            self_weak,
        }
    }

    pub fn subscribe(&self, subscriber: SharedSubscriber) {
        self.subscribers.lock().unwrap().push(subscriber);
    }

    pub fn set_model(&self, model: Model) {
        self.state.lock().unwrap().model = Some(model);
    }

    pub fn set_thinking_level(&self, level: Option<ThinkingLevel>) {
        self.state.lock().unwrap().thinking_level = level;
    }

    pub fn set_system_prompt(&self, system: Option<String>) {
        self.state.lock().unwrap().system = system;
    }

    pub fn install_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        self.state.lock().unwrap().tools = tools;
    }

    pub fn set_limits(&self, limits: TurnLimits) {
        *self.limits.lock().unwrap() = limits;
    }

    pub fn set_stream_options(&self, options: StreamOptions) {
        *self.stream_options.lock().unwrap() = options;
    }

    /// 队列模式(默认 both one-at-a-time;pi :247-248)
    pub fn set_queue_modes(&self, steering: QueueMode, follow_up: QueueMode) {
        let mut queues = self.queues.lock().unwrap();
        queues.steering_mode = steering;
        queues.follow_up_mode = follow_up;
    }

    /// 状态快照仅供 UI/装配读取;订阅者应消费事件而非共享引用(09 B2)
    pub fn state_snapshot(&self) -> AgentStateSnapshot {
        let state = self.state.lock().unwrap();
        AgentStateSnapshot {
            model: state.model.clone(),
            thinking_level: state.thinking_level,
            message_count: state.messages.len(),
            tool_count: state.tools.len(),
            pending_tool_calls: state.pending_tool_calls.len(),
            error_message: state.error_message.clone(),
            is_streaming: self.streaming.load(Ordering::SeqCst),
        }
    }

    /// 消息转录只读视图(投影/调试用;写路径只有 reducer)
    pub fn messages(&self) -> Vec<AgentMessage> {
        self.state.lock().unwrap().messages.clone()
    }

    /// 丢弃转录最后一条消息(overflow 恢复:移除被中断 turn 的错误 assistant 消息,
    /// 使 continue 的"最后一条为 user/toolResult"前置条件成立)。
    pub fn drop_last_message(&self) -> Option<AgentMessage> {
        let mut state = self.state.lock().unwrap();
        state.messages.pop()
    }

    /// 只保留末尾 keep_last 条消息(保留打头的 system baseline);
    /// overflow 恢复时压缩上下文用。
    pub fn trim_oldest_messages(&self, keep_last: usize) {
        let mut state = self.state.lock().unwrap();
        let len = state.messages.len();
        if len <= keep_last {
            return;
        }
        let start = match state.messages.first() {
            Some(AgentMessage::System { .. }) if len - 1 > keep_last => 1,
            _ => 0,
        };
        state.messages.drain(start..len.saturating_sub(keep_last));
    }

    pub fn steer(&self, msg: AgentMessage) {
        self.queues.lock().unwrap().steering.push(msg);
    }

    pub fn follow_up(&self, msg: AgentMessage) {
        self.queues.lock().unwrap().follow_up.push(msg);
    }

    pub fn has_queued_messages(&self) -> bool {
        let queues = self.queues.lock().unwrap();
        !queues.steering.is_empty() || !queues.follow_up.is_empty()
    }

    /// 队列深度(steering 优先展示,pi :330-333 peekQueuedMessages)
    pub fn queue_depths(&self) -> (usize, usize) {
        let queues = self.queues.lock().unwrap();
        (queues.steering.len(), queues.follow_up.len())
    }

    pub fn clear_all_queues(&self) {
        let mut queues = self.queues.lock().unwrap();
        queues.steering.clear();
        queues.follow_up.clear();
        self.adapter.set_preload(Vec::new());
    }

    pub fn abort(&self) {
        self.cancel.lock().unwrap().cancel();
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming.load(Ordering::SeqCst)
    }

    /// 等 run 结束(订阅者在每个事件内串行完成,agent_end 返回时结算已发生)
    pub async fn wait_idle(&self) {
        while self.streaming.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// 新 prompt:activeRun 存在时报错(pi 的 "用 steer/followUp 或等待")。
    pub async fn prompt(&self, text: impl Into<String>) -> Result<RunStop, AgentError> {
        self.prompt_messages(vec![AgentMessage::user(text)]).await
    }

    /// string | AgentMessage[] 双形态入口(pi 的 prompt input)。
    pub async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> Result<RunStop, AgentError> {
        if messages.is_empty() {
            return Err(AgentError::NothingToContinue);
        }
        self.run_with_lifecycle(messages).await
    }

    /// 续跑(pi :384-411):最后一条是 assistant 时先耗 steering 队列、再耗
    /// follow-up,都为空则报错;否则纯续跑(最后一条 user/toolResult = 重试)。
    pub async fn continue_run(&self) -> Result<RunStop, AgentError> {
        let last_is_assistant = self
            .state
            .lock()
            .unwrap()
            .messages
            .last()
            .map(|m| matches!(m, AgentMessage::Assistant(_)))
            .unwrap_or(false);
        if last_is_assistant {
            let (steering, follow_up) = self.queues.lock().unwrap().drain_all();
            if steering.is_empty() && follow_up.is_empty() {
                return Err(AgentError::NothingToContinue);
            }
            let mut preload = steering;
            preload.extend(follow_up);
            self.adapter.set_preload(preload);
        }
        let result = self.run_with_lifecycle(Vec::new()).await;
        // 仅在 run 实际启动后清理 preload;NoModel 等早退时保留,供重试使用
        if result.is_ok() {
            self.adapter.set_preload(Vec::new());
        }
        result
    }

    /// reset(03 §8.1):保留重放后的首条 system 消息作 baseline;run 存在时报错。
    pub fn reset(&self) -> Result<(), AgentError> {
        if self.is_streaming() {
            return Err(AgentError::AlreadyRunning);
        }
        let mut state = self.state.lock().unwrap();
        let baseline = match state.messages.first() {
            Some(message @ AgentMessage::System { .. }) => Some(message.clone()),
            _ => None,
        };
        state.messages.clear();
        if let Some(baseline) = baseline {
            state.messages.push(baseline);
        }
        state.streaming_message = None;
        state.pending_tool_calls.clear();
        state.error_message = None;
        drop(state);
        self.clear_all_queues();
        Ok(())
    }

    /// run 生命周期(pi 的 runWithLifecycle):isStreaming=true → 循环 → 复位;
    /// panic 兜底 = handleRunFailure(合成 error assistant 消息,03 §8.3)。
    async fn run_with_lifecycle(&self, prompts: Vec<AgentMessage>) -> Result<RunStop, AgentError> {
        if self
            .streaming
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(AgentError::AlreadyRunning);
        }

        let cancel = {
            let mut cancel = self.cancel.lock().unwrap();
            if cancel.is_cancelled() {
                *cancel = CancellationToken::new();
            }
            cancel.clone()
        };

        let (system, model, thinking, tools, transcript, limits, stream_options) = {
            let state = self.state.lock().unwrap();
            (
                state.system.clone(),
                state.model.clone(),
                state.thinking_level,
                state.tools.clone(),
                state.messages.clone(),
                *self.limits.lock().unwrap(),
                self.stream_options.lock().unwrap().clone(),
            )
        };
        let Some(model) = model else {
            self.streaming.store(false, Ordering::SeqCst);
            return Err(AgentError::NoModel);
        };

        // pi 的 createContextSnapshot:拿快照进、发事件出,无可变全局(09 A4)
        let context = AgentContext { system, messages: transcript, tools };
        let config = LoopConfig { model, thinking, limits, stream_options };
        let sink: Arc<dyn Subscriber> = match self.self_weak.upgrade() {
            Some(this) => this,
            None => {
                self.streaming.store(false, Ordering::SeqCst);
                return Err(AgentError::Panicked);
            }
        };

        let future = run_agent_loop(
            prompts,
            context,
            self.adapter.clone(),
            config,
            self.provider.clone(),
            sink,
            cancel,
        );
        let result = AssertUnwindSafe(future).catch_unwind().await;

        self.streaming.store(false, Ordering::SeqCst);

        match result {
            Ok(output) => {
                if let RunStop::Error(message) = &output.stop {
                    self.state.lock().unwrap().error_message = Some(message.clone());
                }
                Ok(output.stop)
            }
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                self.handle_run_failure(&message).await;
                Err(AgentError::Panicked)
            }
        }
    }

    /// handleRunFailure(03 §8.3):合成 stopReason=error 的 assistant 消息,
    /// 依次发 message_start → message_end → turn_end → agent_end
    /// (agent_end.messages 只含这一条)。
    async fn handle_run_failure(&self, message: &str) {
        let model = self
            .state
            .lock()
            .unwrap()
            .model
            .clone()
            .unwrap_or_else(|| rpi_ai::Model::minimal("unknown", "unknown", "unknown"));
        self.state.lock().unwrap().error_message = Some(message.to_string());
        let assistant = rpi_ai::AssistantMessage::error(&model, message, false);
        let entry = AgentMessage::Assistant(Box::new(assistant.clone()));
        let start = AgentEvent::MessageStart { message: Box::new(entry.clone()) };
        let end = AgentEvent::MessageEnd { message: Box::new(entry.clone()) };
        let turn_end = AgentEvent::TurnEnd { message: Box::new(assistant), tool_results: Vec::new() };
        let agent_end = AgentEvent::AgentEnd { messages: vec![entry] };
        self.on_event(&start).await;
        self.on_event(&end).await;
        self.on_event(&turn_end).await;
        self.on_event(&agent_end).await;
    }

    /// 事件 reducer(03 §8.4):状态唯一写路径。
    fn reduce(&self, event: &AgentEvent) {
        let mut state = self.state.lock().unwrap();
        match event {
            AgentEvent::MessageStart { message } => {
                if let AgentMessage::Assistant(assistant) = &**message {
                    state.streaming_message = Some((**assistant).clone());
                }
            }
            AgentEvent::MessageUpdate { message } => {
                state.streaming_message = Some((**message).clone());
            }
            AgentEvent::MessageEnd { message } => {
                state.streaming_message = None;
                state.messages.push((**message).clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                state.pending_tool_calls.insert(tool_call_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                state.pending_tool_calls.remove(tool_call_id);
            }
            AgentEvent::TurnEnd { message, .. } => {
                // 每 turn 覆盖:成功 turn 清除上一轮的错误(03 §8.4)
                state.error_message = message.error_message.clone();
            }
            AgentEvent::AgentEnd { .. } => {
                state.streaming_message = None;
            }
            _ => {}
        }
    }

    /// 仅供测试/装配检查;订阅者应消费事件而非查询状态
    pub fn last_assistant_content(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .messages
            .iter()
            .rev()
            .find_map(|msg| msg.as_assistant().map(|a| a.text_content()))
    }
}

#[derive(Debug, Clone)]
pub struct AgentStateSnapshot {
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub message_count: usize,
    pub tool_count: usize,
    pub pending_tool_calls: usize,
    pub error_message: Option<String>,
    pub is_streaming: bool,
}

#[async_trait]
impl Subscriber for Agent {
    async fn on_event(&self, event: &AgentEvent) {
        // reducer 先行,随后按订阅顺序串行 await(09 B2;结算前 listener 全部完成)
        self.reduce(event);
        let subscribers = self.subscribers.lock().unwrap().clone();
        for subscriber in subscribers {
            subscriber.on_event(event).await;
        }
    }
}
