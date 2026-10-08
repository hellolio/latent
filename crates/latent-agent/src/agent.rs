//! `Agent` 有状态薄壳(pi 的 agent.ts,03 文档 §8):队列、订阅、run 生命周期。
//!
//! 循环是纯函数式的(快照进、事件出),Agent 持有状态并作为事件 sink:
//! reducer 更新 `state`(message_end push,双份真相由"循环操作快照、公开状态
//! 事件驱动"消解),随后按订阅顺序串行 await 订阅者。steering/followUp 队列经
//! 内部 adapter 绑定进 `LoopHooks`(pi 的 createLoopConfig 等价物,09 A2 接缝 2)。

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use futures::FutureExt;
use latent_ai::{Model, Provider, StreamOptions, ThinkingLevel};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, SharedPartial, SharedSubscriber, Subscriber};
use crate::hooks::LoopHooks;
use crate::loop_::{
    create_injection_endpoints, run_agent_loop, AgentContext, InjectionReceiver, InjectionSender,
    LoopConfig, RunStop, TurnLimits, DEFAULT_TOOL_RESULT_MAX_CHARS,
};
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
pub struct AgentState {
    /// 基础系统提示词(请求级字段;session 不持久化能力规则,04 文档 §3.3)
    pub system: Option<String>,
    pub model: Option<Model>,
    /// None = 不请求思考(01 文档:ai 侧级别无 off)
    pub thinking_level: Option<ThinkingLevel>,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn Tool>>,
    /// 流式中的 partial 消息快照(message_end 后清空)
    pub streaming_message: Option<latent_ai::AssistantMessage>,
    /// "到目前为止"的 partial 读口(T2):循环原地更新,UI 随帧取用
    pub streaming_partial: Option<SharedPartial>,
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
    #[error("{0}")]
    Other(String),
}

/// 用户 hooks 的透传绑定(pi 的 createLoopConfig;steering/follow-up 走
/// mpsc 注入通道,不再经钩子)。
/// `agent` 是 Weak 回指:转录被 `auto_compact_context` 整体替换时同步
/// reducer 状态(下次 run 的种子;MessageEnd 只 append,无法表达替换)。
struct AgentHookAdapter {
    inner: Arc<dyn LoopHooks>,
    agent: Weak<Agent>,
}

#[async_trait]
impl LoopHooks for AgentHookAdapter {
    fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<latent_ai::Message> {
        self.inner.convert_to_llm(msgs)
    }

    fn tool_call_guard_enabled(&self) -> bool {
        self.inner.tool_call_guard_enabled()
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

    async fn auto_compact_context(
        &self,
        model: &Model,
        messages: &[AgentMessage],
    ) -> Option<Vec<AgentMessage>> {
        let compacted = self.inner.auto_compact_context(model, messages).await?;
        // 转录替换同步到 Agent 状态(唯一写路径 reducer 之外的例外:压缩替换
        // 不是事件可表达的消息追加;此刻无并发,循环单任务内同步执行)
        if let Some(agent) = self.agent.upgrade() {
            agent.state.lock().unwrap().messages = compacted.clone();
        }
        Some(compacted)
    }

    async fn before_tool_call(
        &self,
        ctx: crate::hooks::ToolCallCtx,
    ) -> Option<crate::hooks::ToolBlock> {
        self.inner.before_tool_call(ctx).await
    }

    async fn after_tool_call(
        &self,
        ctx: crate::hooks::ToolResultCtx,
    ) -> Option<crate::hooks::ToolPatch> {
        self.inner.after_tool_call(ctx).await
    }

    fn tool_execution(&self) -> crate::tool::ToolExecution {
        self.inner.tool_execution()
    }
}

pub struct Agent {
    state: Mutex<AgentState>,
    /// 注入通道发送端(steer/follow_up 任意时刻可 push;panic 恢复时可整体重建)
    sender: Mutex<InjectionSender>,
    /// 深度镜像读口(与 sender/receiver 共享 Arc;run 期间也可读)
    depth: Arc<crate::loop_::InjectionDepth>,
    /// 注入通道接收端(run 进行中被移交给循环)
    receiver: Mutex<Option<InjectionReceiver>>,
    /// 批量模式(pi :247-248 默认两者都 one-at-a-time),随 LoopConfig 进循环
    steering_mode: Mutex<QueueMode>,
    follow_up_mode: Mutex<QueueMode>,
    subscribers: Mutex<Vec<SharedSubscriber>>,
    provider: Arc<dyn Provider>,
    adapter: Arc<AgentHookAdapter>,
    limits: Mutex<TurnLimits>,
    /// 超长 tool result 进转录前的字符上限(Current Turn 层裁剪)
    tool_result_max_chars: Mutex<usize>,
    /// 图片占位符开关(settings `blockImages`):发送前把 Image 块替换为文本占位符
    block_images: Mutex<bool>,
    stream_options: Mutex<StreamOptions>,
    /// streaming 标志(T8):watch 化,`wait_idle` 经 receiver 等待、零轮询;
    /// Sender 加锁串行化"检查-置位"(替代 AtomicBool 的 compare_exchange)
    streaming: Mutex<watch::Sender<bool>>,
    streaming_rx: watch::Receiver<bool>,
    cancel: Mutex<CancellationToken>,
    self_weak: Weak<Agent>,
}

/// 工厂:上游只认 `Arc<Agent>`,实现细节不出厂(方针文档 §2 规则 1)。
pub fn create_agent(provider: Arc<dyn Provider>, hooks: Arc<dyn LoopHooks>) -> Arc<Agent> {
    Arc::new_cyclic(|weak| Agent::new(provider, hooks, weak.clone()))
}

impl Agent {
    fn new(provider: Arc<dyn Provider>, hooks: Arc<dyn LoopHooks>, self_weak: Weak<Agent>) -> Self {
        let (sender, receiver) = create_injection_endpoints();
        let depth = sender.depth();
        let adapter = Arc::new(AgentHookAdapter {
            inner: hooks.clone(),
            agent: self_weak.clone(),
        });
        let (streaming, streaming_rx) = watch::channel(false);
        Self {
            state: Mutex::new(AgentState::default()),
            depth,
            sender: Mutex::new(sender),
            receiver: Mutex::new(Some(receiver)),
            steering_mode: Mutex::new(QueueMode::OneAtATime),
            follow_up_mode: Mutex::new(QueueMode::OneAtATime),
            subscribers: Mutex::new(Vec::new()),
            provider,
            adapter,
            limits: Mutex::new(TurnLimits::default()),
            tool_result_max_chars: Mutex::new(DEFAULT_TOOL_RESULT_MAX_CHARS),
            block_images: Mutex::new(false),
            stream_options: Mutex::new(StreamOptions::default()),
            streaming: Mutex::new(streaming),
            streaming_rx,
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

    /// 超长 tool result 的字符上限(0 = 不裁剪)。
    pub fn set_tool_result_max_chars(&self, max_chars: usize) {
        *self.tool_result_max_chars.lock().unwrap() = max_chars;
    }

    /// 图片占位符开关(settings `blockImages`):true = 发送前把转录里的
    /// Image 块替换为文本占位符。
    pub fn set_block_images(&self, block: bool) {
        *self.block_images.lock().unwrap() = block;
    }

    pub fn set_stream_options(&self, options: StreamOptions) {
        *self.stream_options.lock().unwrap() = options;
    }

    /// 队列模式(默认 both one-at-a-time;pi :247-248)。
    /// steering 模式随 LoopConfig 进循环;follow-up 在停止点整批整流。
    pub fn set_queue_modes(&self, steering: QueueMode, follow_up: QueueMode) {
        *self.steering_mode.lock().unwrap() = steering;
        *self.follow_up_mode.lock().unwrap() = follow_up;
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
            is_streaming: *self.streaming.lock().unwrap().borrow(),
        }
    }

    /// 消息转录只读视图(投影/调试用;写路径只有 reducer)
    pub fn messages(&self) -> Vec<AgentMessage> {
        self.state.lock().unwrap().messages.clone()
    }

    /// 整体替换转录(session 恢复 / compaction 投影回填;run 期间报错)。
    /// 不产生事件:回填内容已存在于 Session,重放会造成重复持久化。
    pub fn set_messages(&self, messages: Vec<AgentMessage>) -> Result<(), AgentError> {
        if self.is_streaming() {
            return Err(AgentError::AlreadyRunning);
        }
        let mut state = self.state.lock().unwrap();
        state.streaming_message = None;
        state.pending_tool_calls.clear();
        state.messages = messages;
        Ok(())
    }

    /// 丢弃转录最后一条消息(overflow 恢复:移除被中断 turn 的错误 assistant 消息,
    /// 使 continue 的"最后一条为 user/toolResult"前置条件成立)。
    pub fn drop_last_message(&self) -> Option<AgentMessage> {
        let mut state = self.state.lock().unwrap();
        state.messages.pop()
    }

    /// 只保留末尾 keep_last 条消息;overflow 恢复时压缩上下文用。
    pub fn trim_oldest_messages(&self, keep_last: usize) {
        let mut state = self.state.lock().unwrap();
        let len = state.messages.len();
        if len <= keep_last {
            return;
        }
        let removable = len - keep_last;
        state.messages.drain(..removable);
    }

    /// steer:推送进 mpsc 注入通道(03 §10.5;run 期间也可调用,循环在
    /// 流式期间 select! 接收)。
    pub fn steer(&self, msg: AgentMessage) {
        self.sender.lock().unwrap().steer(msg);
    }

    /// followUp:推送进 mpsc 注入通道(agent 本应停止时整流)。
    pub fn follow_up(&self, msg: AgentMessage) {
        self.sender.lock().unwrap().follow_up(msg);
    }

    pub fn has_queued_messages(&self) -> bool {
        self.queue_depths() != (0, 0)
    }

    /// 队列深度(发送递增、循环消费递减的镜像计数;run 期间也可读)
    pub fn queue_depths(&self) -> (usize, usize) {
        (self.depth.steering(), self.depth.follow_up())
    }

    pub fn clear_all_queues(&self) {
        if let Some(receiver) = &mut *self.receiver.lock().unwrap() {
            receiver.clear();
        }
    }

    pub fn abort(&self) {
        self.cancel.lock().unwrap().cancel();
    }

    pub fn is_streaming(&self) -> bool {
        *self.streaming.lock().unwrap().borrow()
    }

    /// 等 run 结束(T8 去轮询):watch 的 `borrow_and_update + changed` 等待,
    /// 无忙等、无丢失唤醒(04 文档踩坑③)。
    pub async fn wait_idle(&self) {
        let mut rx = self.streaming_rx.clone();
        while *rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                // Sender 随 Agent 存活,正常不会走这里;兜底防自旋
                break;
            }
        }
    }

    /// 新 prompt:activeRun 存在时报错(pi 的 "用 steer/followUp 或等待")。
    pub async fn prompt(&self, text: impl Into<String>) -> Result<RunStop, AgentError> {
        self.prompt_messages(vec![AgentMessage::user(text)]).await
    }

    /// string | AgentMessage[] 双形态入口(pi 的 prompt input)。
    pub async fn prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<RunStop, AgentError> {
        if messages.is_empty() {
            return Err(AgentError::NothingToContinue);
        }
        self.run_with_lifecycle(messages).await
    }

    /// 续跑(pi :384-411):最后一条是 assistant 时要求注入通道非空
    /// (steering/follow-up 由循环在边界/停止点整流);否则纯续跑
    /// (最后一条 user/toolResult = 重试)。
    pub async fn continue_run(&self) -> Result<RunStop, AgentError> {
        let last_is_assistant = self
            .state
            .lock()
            .unwrap()
            .messages
            .last()
            .map(|m| matches!(m, AgentMessage::Assistant(_)))
            .unwrap_or(false);
        if last_is_assistant && !self.has_queued_messages() {
            return Err(AgentError::NothingToContinue);
        }
        self.run_with_lifecycle(Vec::new()).await
    }

    /// reset(03 §8.1):清空转录与队列;run 存在时报错。
    pub fn reset(&self) -> Result<(), AgentError> {
        if self.is_streaming() {
            return Err(AgentError::AlreadyRunning);
        }
        let mut state = self.state.lock().unwrap();
        state.messages.clear();
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
        // 检查-置位在锁内串行(替代 compare_exchange;watch Sender 加锁)
        {
            let streaming = self.streaming.lock().unwrap();
            if *streaming.borrow() {
                return Err(AgentError::AlreadyRunning);
            }
            let _ = streaming.send(true);
        }

        let cancel = {
            let mut cancel = self.cancel.lock().unwrap();
            if cancel.is_cancelled() {
                *cancel = CancellationToken::new();
            }
            cancel.clone()
        };

        let (system, model, thinking, tools, transcript, limits, tool_result_max_chars, block_images, stream_options, steering_mode, follow_up_mode) = {
            let state = self.state.lock().unwrap();
            (
                state.system.clone(),
                state.model.clone(),
                state.thinking_level,
                state.tools.clone(),
                state.messages.clone(),
                *self.limits.lock().unwrap(),
                *self.tool_result_max_chars.lock().unwrap(),
                *self.block_images.lock().unwrap(),
                self.stream_options.lock().unwrap().clone(),
                *self.steering_mode.lock().unwrap(),
                *self.follow_up_mode.lock().unwrap(),
            )
        };
        let Some(model) = model else {
            let _ = self.streaming.lock().unwrap().send(false);
            return Err(AgentError::NoModel);
        };

        // 注入接收端移交给循环;run 结束后放回(期间 steer 走发送端缓冲)
        let receiver = self
            .receiver
            .lock()
            .unwrap()
            .take()
            .expect("injection receiver available outside run");

        // pi 的 createContextSnapshot:拿快照进、发事件出,无可变全局(09 A4)
        let context = AgentContext {
            system,
            messages: transcript,
            tools,
        };
        let config = LoopConfig {
            model,
            thinking,
            limits,
            stream_options,
            steering_mode,
            follow_up_mode,
            tool_result_max_chars,
            block_images,
        };
        let sink: Arc<dyn Subscriber> = match self.self_weak.upgrade() {
            Some(this) => this,
            None => {
                let _ = self.streaming.lock().unwrap().send(false);
                return Err(AgentError::Panicked);
            }
        };

        // 接收端移交给循环,run 结束后归还(未消费的注入消息保留到下一 run / clear)
        let future = run_agent_loop(
            prompts,
            context,
            self.adapter.clone(),
            config,
            self.provider.clone(),
            sink,
            cancel,
            receiver,
        );
        let (result, receiver) = match AssertUnwindSafe(future).catch_unwind().await {
            Ok((output, receiver)) => {
                // 异常终止路径:已取出未消费的注入消息放回通道(不静默丢失;
                // 计数仍算在这些消息上,故用 requeue_* 不增计数)
                let sender = self.sender.lock().unwrap();
                let (requeued_steering, requeued_follow_up) = (
                    output.requeued_steering.clone(),
                    output.requeued_follow_up.clone(),
                );
                for message in requeued_steering {
                    sender.requeue_steering(message);
                }
                for message in requeued_follow_up {
                    sender.requeue_follow_up(message);
                }
                (Ok(output), receiver)
            }
            Err(panic) => {
                // 循环 panic 时接收端随 future 丢弃:重建端点(宿主下一次 steer
                // 从干净状态开始,与 handleRunFailure 的合成兜底同级)
                let (sender, fresh) = create_injection_endpoints();
                *self.sender.lock().unwrap() = sender;
                (Err(panic), fresh)
            }
        };

        // 接收端放回
        *self.receiver.lock().unwrap() = Some(receiver);
        let _ = self.streaming.lock().unwrap().send(false);

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
            .unwrap_or_else(|| latent_ai::Model::minimal("unknown", "unknown", "unknown"));
        self.state.lock().unwrap().error_message = Some(message.to_string());
        let assistant = latent_ai::AssistantMessage::error(&model, message, false);
        let entry = AgentMessage::Assistant(Box::new(assistant.clone()));
        let start = AgentEvent::MessageStart {
            message: Box::new(entry.clone()),
            partial: None,
            started_at_ms: None,
        };
        let end = AgentEvent::MessageEnd {
            message: Box::new(entry.clone()),
        };
        let turn_end = AgentEvent::TurnEnd {
            message: Box::new(assistant),
            tool_results: Vec::new(),
        };
        let agent_end = AgentEvent::AgentEnd {
            messages: vec![entry],
        };
        self.on_event(&start).await;
        self.on_event(&end).await;
        self.on_event(&turn_end).await;
        self.on_event(&agent_end).await;
    }

    /// 事件 reducer(03 §8.4):状态唯一写路径。
    fn reduce(&self, event: &AgentEvent) {
        let mut state = self.state.lock().unwrap();
        match event {
            AgentEvent::MessageStart { message, partial, .. } => {
                state.streaming_partial = partial.clone();
                if let AgentMessage::Assistant(assistant) = &**message {
                    state.streaming_message = Some((**assistant).clone());
                }
            }
            AgentEvent::MessageUpdate { message } => {
                state.streaming_message = Some((**message).clone());
            }
            AgentEvent::MessageEnd { message } => {
                state.streaming_message = None;
                state.streaming_partial = None;
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
                state.streaming_partial = None;
            }
            _ => {}
        }
    }

    /// "到目前为止"的 partial 消息(T2 快照读口;UI 随帧读取,clone 一次)。
    pub fn partial_message(&self) -> Option<latent_ai::AssistantMessage> {
        let state = self.state.lock().unwrap();
        state
            .streaming_partial
            .as_ref()
            .and_then(|partial| partial.read().ok().map(|p| p.clone()))
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
