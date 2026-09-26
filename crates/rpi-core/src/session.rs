//! `AgentSession`(04 文档 §2):所有模式共享的唯一业务层。编排 Agent、系统
//! 提示词 sections、激活工具集、steering/followUp 队列镜像、自动重试事件与
//! overflow 恢复;向 mode 暴露 `AgentSessionEvent` 流(UI 只消费这一层)。
//!
//! 会话持久化经 `SessionSink` trait 注入 —— rpi-session 是可选组件,由装配方
//! (rpi-cli)提供其适配实现;移除 rpi-session 后本 crate 照常编译。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use thiserror::Error;

use rpi_agent::{AgentError, AgentEvent, AgentMessage, LoopHooks, Subscriber, Tool};
use rpi_ai::{is_context_overflow, Model, Provider, StreamOptions};

use crate::extensions::{ExtensionActions, ExtensionDiagnostic, ExtensionRegistry, ExtensionUi};
use crate::retry::RetryHooks;
use crate::system_prompt::{
    build_system_prompt_sections, build_system_prompt_state, diff_system_prompt_sections,
    SystemPromptOptions, SystemPromptSections, SystemPromptState,
};
use rpi_agent::{RunStop, TurnLimits};

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("agent error: {0}")]
    Agent(#[from] AgentError),
    #[error("extension `{name}` failed: {message}")]
    Extension { name: String, message: String },
    #[error("system prompt error: {0}")]
    SystemPrompt(String),
}

/// 会话持久化接缝:core 只见 trait,具体实现(如 rpi-session 的 JSONL 树)
/// 由装配方注入(09 A4:持久会话由 SessionManager 权威所有)。
///
/// 除 `append` 外的方法都有默认空实现:装配方可按需支持对应 entry 类型
/// (transcript 统一:消息/usage/模型与思考级别变更都进 Session)。
#[async_trait]
pub trait SessionSink: Send + Sync {
    async fn append(&self, message: &AgentMessage) -> Result<(), String>;

    async fn append_model_change(&self, _provider: &str, _model_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn append_thinking_level_change(&self, _level: &str) -> Result<(), String> {
        Ok(())
    }

    async fn append_usage(
        &self,
        _kind: &str,
        _provider: &str,
        _model: &str,
        _usage: rpi_ai::Usage,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// 上下文压缩统一入口(overflow 恢复与 manual /compact 共用;06 文档):
/// 实现方负责 run compaction → Session.append(Compaction)(原始历史保留)
/// → 返回压缩后的上下文消息(Session projection 结果)。传入当前模型供
/// 摘要 LLM 调用使用。
#[async_trait]
pub trait ContextCompactor: Send + Sync {
    async fn compact(&self, model: &Model) -> Result<Vec<AgentMessage>, String>;
}

/// mode 侧事件(04 文档 §2.3 AgentSessionEvent 的 M4 子集):10 种 agent 事件
/// 之上追加 settled/queue/retry 面,UI 只消费这一层。
#[derive(Debug, Clone)]
pub enum AgentSessionEvent {
    Agent(AgentEvent),
    /// agent_end 之后(run 结算完成)
    AgentSettled,
    QueueUpdate {
        steering: usize,
        follow_up: usize,
    },
    AutoRetryStart {
        attempt: u32,
        delay_ms: u64,
        reason: String,
    },
    AutoRetryEnd {
        success: bool,
        reason: String,
    },
}

/// session 事件订阅者(mode/持久化/扩展 UI)。
#[async_trait]
pub trait SessionSubscriber: Send + Sync {
    async fn on_session_event(&self, event: &AgentSessionEvent);
}

pub type SessionSharedSubscriber = Arc<dyn SessionSubscriber>;

#[derive(Debug, Clone, Default)]
pub struct SessionRuntimeState {
    pub steering_depth: usize,
    pub follow_up_depth: usize,
}

pub struct AgentSessionConfig {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub hooks: Arc<dyn LoopHooks>,
    pub ui: Arc<dyn ExtensionUi>,
    pub extensions: ExtensionRegistry,
    /// 内置工具 + 扩展注册工具的**全量候选集**(激活子集经 active_tool_names 过滤)
    pub tools: Vec<Arc<dyn Tool>>,
    /// 激活工具名;默认全量(pi 的 initialActiveToolNames 默认 read/bash/edit/write)
    pub active_tool_names: Option<Vec<String>>,
    pub system_prompt: SystemPromptOptions,
    pub limits: TurnLimits,
    pub stream_options: StreamOptions,
    pub session_sink: Option<Arc<dyn SessionSink>>,
    /// 恢复/压缩接缝:从 Session 投影回填的初始转录(--continue 语义)
    pub seed_messages: Vec<AgentMessage>,
    /// 统一 compaction(overflow 恢复与 manual /compact 共用;装配方提供)
    pub compactor: Option<Arc<dyn ContextCompactor>>,
    /// 外部预建的订阅者列表(可选):装配方需要把同一列表交给多个事件源
    /// (如 SessionBridge 与 retry hooks)时传入;缺省内部新建。
    pub subscribers: Option<Arc<Mutex<Vec<SessionSharedSubscriber>>>>,
}

/// 运行期可变状态(锁保护;无全局状态)。
struct SessionRuntime {
    system_prompt: SystemPromptState,
    sections: SystemPromptSections,
    /// 原始构建 options:工具集变更时在其上替换工具片段重建,
    /// 避免丢 custom_prompt/context_files/append 等用户配置
    system_prompt_options: SystemPromptOptions,
    active_tool_names: Vec<String>,
    /// 待注入的系统提示词 patch(扩展/工具集变更产生,prompt 时进转录)
    pending_system_messages: Vec<AgentMessage>,
    /// 每次 run 只尝试一次 overflow 恢复(04 文档 _overflowRecoveryAttempted)
    overflow_recovery_attempted: bool,
}

/// prompt 结果(T8,04 文档踩坑②):流式中按 streamingBehavior 转 steer 时返回
/// `Enqueued`,调用方不再需要以 `is_streaming()` 二次区分。
#[derive(Debug, Clone)]
pub enum PromptOutcome {
    /// 本次调用启动了一个新 run,携带 run 终止原因
    Started(RunStop),
    /// agent 正在流式,消息已入队(steering)
    Enqueued,
}

impl PromptOutcome {
    /// 兼容旧调用形态的便捷取值:Enqueued 视作 EndTurn(旧语义)。
    pub fn stop(self) -> RunStop {
        match self {
            PromptOutcome::Started(stop) => stop,
            PromptOutcome::Enqueued => RunStop::EndTurn,
        }
    }
}

/// pi AgentSession 的 Rust 版:持有 `Agent`,向 mode 暴露 prompt/steer/事件流。
pub struct AgentSession {
    agent: Arc<rpi_agent::Agent>,
    ui: Arc<dyn ExtensionUi>,
    tools_all: Vec<Arc<dyn Tool>>,
    subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>>,
    /// transcript 统一:模型/思考级别/usage 变更经同一 sink 落盘
    session_sink: Option<Arc<dyn SessionSink>>,
    compactor: Option<Arc<dyn ContextCompactor>>,
    runtime: Mutex<SessionRuntime>,
    /// 装配期收集的扩展诊断(init 失败跳过等,07 §8.5),面向 mode 可见。
    extension_diagnostics: Vec<ExtensionDiagnostic>,
}

/// 装配工厂(方针文档 §2 规则 1):完成"扩展 init → 工具集合并 → 系统提示词
/// sections → Agent 组装 → 持久化订阅"的组装顺序;上游只认 trait。
///
/// 扩展装配语义(07 §8.5):**逐扩展独立工具缓冲**,init 失败 = 跳过该扩展 +
/// 收集诊断,已注册工具整体丢弃、session 照常创建(pi loader 的
/// continue + warning);一个坏的扩展拖不死整体。
pub async fn create_agent_session(config: AgentSessionConfig) -> Result<AgentSession, CoreError> {
    let mut tools = config.tools;
    let mut extension_diagnostics: Vec<ExtensionDiagnostic> = Vec::new();
    for extension in config.extensions.all() {
        let mut buffer: Vec<Arc<dyn Tool>> = Vec::new();
        let mut api = crate::extensions::ExtensionApi::new(&mut buffer, config.ui.clone());
        match extension.init(&mut api).await {
            Ok(()) => tools.append(&mut buffer),
            Err(error) => extension_diagnostics.push(ExtensionDiagnostic {
                extension: extension.name().to_string(),
                message: error.to_string(),
            }),
        }
    }

    // 激活工具子集(默认全量)
    let active_tool_names = config
        .active_tool_names
        .unwrap_or_else(|| tools.iter().map(|tool| tool.name().to_string()).collect());
    let active_tools: Vec<Arc<dyn Tool>> = tools
        .iter()
        .filter(|tool| active_tool_names.iter().any(|name| name == tool.name()))
        .cloned()
        .collect();

    // 系统提示词 sections(工具片段来自激活工具)
    let mut options = config.system_prompt;
    options
        .tool_snippets
        .extend(active_tools.iter().filter_map(|tool| tool.prompt_snippet()));
    options.tool_guidelines.extend(
        active_tools
            .iter()
            .flat_map(|tool| tool.prompt_guidelines()),
    );
    let state = build_system_prompt_state(&options)?;
    let sections = match &state {
        SystemPromptState::Sections(sections) => sections.clone(),
        SystemPromptState::Forced(_) => SystemPromptSections::default(),
    };

    let agent = rpi_agent::create_agent(config.provider, config.hooks);
    agent.set_model(config.model.clone());
    agent.set_system_prompt(Some(state.to_text()));
    agent.install_tools(active_tools);
    agent.set_limits(config.limits);
    agent.set_stream_options(config.stream_options);
    // transcript 统一:--continue 场景从 Session projection 回填初始转录
    if !config.seed_messages.is_empty() {
        agent
            .set_messages(config.seed_messages)
            .map_err(CoreError::Agent)?;
    }

    let subscribers = config
        .subscribers
        .clone()
        .unwrap_or_else(|| Arc::new(Mutex::new(Vec::new())));
    agent.subscribe(Arc::new(SessionBridge {
        subscribers: subscribers.clone(),
        sink: config.session_sink.clone(),
    }));

    Ok(AgentSession {
        agent,
        ui: config.ui,
        tools_all: tools,
        subscribers,
        session_sink: config.session_sink,
        compactor: config.compactor,
        extension_diagnostics,
        runtime: Mutex::new(SessionRuntime {
            system_prompt: state,
            system_prompt_options: options,
            sections,
            active_tool_names,
            pending_system_messages: Vec::new(),
            overflow_recovery_attempted: false,
        }),
    })
}

/// agent 事件 → session 事件 + 持久化 + AgentSettled 的翻译层
/// (pi 的 _installAgent* 三个安装器的 M4 等价物;Agent 不知道扩展/持久化存在)。
struct SessionBridge {
    subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>>,
    sink: Option<Arc<dyn SessionSink>>,
}

impl SessionBridge {
    async fn broadcast(&self, event: &AgentSessionEvent) {
        let subscribers = self.subscribers.lock().unwrap().clone();
        for subscriber in subscribers {
            subscriber.on_session_event(event).await;
        }
    }
}

#[async_trait]
impl Subscriber for SessionBridge {
    async fn on_event(&self, event: &AgentEvent) {
        // 持久化:message_end = 消息定稿,逐条追加(append-only)
        if let (AgentEvent::MessageEnd { message }, Some(sink)) = (event, &self.sink) {
            if let Err(error) = sink.append(message).await {
                eprintln!("[rpi] session sink append failed: {error}");
            }
            // usage 一致:assistant 定稿即记录 usage entry(不进模型上下文)
            if let AgentMessage::Assistant(assistant) = message.as_ref() {
                if let Err(error) = sink
                    .append_usage(
                        "message",
                        &assistant.provider,
                        &assistant.model,
                        assistant.usage,
                    )
                    .await
                {
                    eprintln!("[rpi] session sink usage append failed: {error}");
                }
            }
        }
        self.broadcast(&AgentSessionEvent::Agent(event.clone()))
            .await;
        if matches!(event, AgentEvent::AgentEnd { .. }) {
            self.broadcast(&AgentSessionEvent::AgentSettled).await;
        }
    }
}

/// session 侧 RetryHooks → AutoRetryStart/End 事件(重试装饰器经此上报)。
struct SessionRetryHooks {
    subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>>,
}

impl RetryHooks for SessionRetryHooks {
    fn on_retry_scheduled(&self, attempt: u32, _max_attempts: u32, delay_ms: u64, error: &str) {
        self.spawn_event(AgentSessionEvent::AutoRetryStart {
            attempt,
            delay_ms,
            reason: error.to_string(),
        });
    }

    fn on_retry_finished(&self, success: bool, _attempt: u32, _final_error: Option<&str>) {
        self.spawn_event(AgentSessionEvent::AutoRetryEnd {
            success,
            reason: "provider retry".into(),
        });
    }
}

impl SessionRetryHooks {
    /// 同步回调 → 异步订阅者:事件通知允许 spawn 转发(不参与结算语义)
    fn spawn_event(&self, event: AgentSessionEvent) {
        let subscribers = self.subscribers.lock().unwrap().clone();
        tokio::spawn(async move {
            for subscriber in subscribers {
                subscriber.on_session_event(&event).await;
            }
        });
    }
}

/// 工厂:把 session 的 AutoRetry 事件面接到重试装饰器上(装配方在创建 provider
/// 时使用,见 cli 装配示例)。
pub fn create_session_retry_hooks(
    subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>>,
) -> Arc<dyn RetryHooks> {
    Arc::new(SessionRetryHooks { subscribers })
}

impl AgentSession {
    pub fn agent(&self) -> &Arc<rpi_agent::Agent> {
        &self.agent
    }

    pub fn ui(&self) -> &Arc<dyn ExtensionUi> {
        &self.ui
    }

    /// 装配期扩展诊断(07 §8.5):init 失败跳过 + 收集,mode 可见(cli 打 stderr)。
    pub fn extension_diagnostics(&self) -> &[ExtensionDiagnostic] {
        &self.extension_diagnostics
    }

    /// 订阅 session 事件流(接缝 #5:mode 只消费这一层)。
    pub fn subscribe(&self, subscriber: SessionSharedSubscriber) {
        self.subscribers.lock().unwrap().push(subscriber);
    }

    /// prompt(pi @1606):流式中默认转 steer(可再暴露 streamingBehavior 配置)。
    /// 返回 `PromptOutcome`(T8):Started 携带 run 终止原因;Enqueued = 已入队。
    pub async fn prompt(&self, text: impl Into<String>) -> Result<PromptOutcome, CoreError> {
        if self.agent.is_streaming() {
            self.steer(text).await;
            return Ok(PromptOutcome::Enqueued);
        }
        let text = text.into();
        let system_messages =
            std::mem::take(&mut self.runtime.lock().unwrap().pending_system_messages);
        let mut messages = system_messages.clone();
        messages.push(AgentMessage::user(text.clone()));
        let run = self.agent.prompt_messages(messages);
        match self.run_with_recovery(run).await {
            Ok(stop) => Ok(PromptOutcome::Started(stop)),
            // TOCTOU:is_streaming 检查后并发 prompt 抢先启动了 run →
            // 按流式语义转 steer(reviewer P2);系统 patch 退回待注入队列,
            // 随下一次成功启动的 prompt 进转录
            Err(CoreError::Agent(AgentError::AlreadyRunning)) => {
                self.runtime.lock().unwrap().pending_system_messages = system_messages;
                self.steer(text).await;
                Ok(PromptOutcome::Enqueued)
            }
            Err(error) => Err(error),
        }
    }

    /// steer(pi @1860):push 进 agent 队列 + session 深度镜像 → QueueUpdate。
    pub async fn steer(&self, text: impl Into<String>) {
        self.agent.steer(AgentMessage::user(text));
        self.emit_queue_update().await;
    }

    /// followUp(pi @1872)。
    pub async fn follow_up(&self, text: impl Into<String>) {
        self.agent.follow_up(AgentMessage::user(text));
        self.emit_queue_update().await;
    }

    pub fn queue_depths(&self) -> (usize, usize) {
        self.agent.queue_depths()
    }

    pub fn abort(&self) {
        self.agent.abort();
    }

    pub async fn wait_idle(&self) {
        self.agent.wait_idle().await;
    }

    /// `!` bash 执行记录(pi 的 bash 透传):BashExecution 消息进转录
    /// (内存 + 持久化);convert_to_llm 语义使其在下一轮进入模型上下文,
    /// 本调用不触发模型 run。run 期间拒绝(转录变更限制)。
    pub async fn record_bash_execution(
        &self,
        command: String,
        output: String,
        exit_code: Option<i32>,
    ) -> Result<(), CoreError> {
        let message = AgentMessage::BashExecution {
            command: command.clone(),
            output: output.clone(),
            exit_code,
            timestamp: rpi_agent::now_ms(),
        };
        let mut messages = self.agent.messages();
        messages.push(message.clone());
        self.agent.set_messages(messages).map_err(CoreError::from)?;
        if let Some(sink) = &self.session_sink {
            sink.append(&message).await.map_err(|error| {
                CoreError::SystemPrompt(format!("session sink append: {error}"))
            })?;
        }
        Ok(())
    }

    /// 切换激活工具集(pi @1279):工具集变更由循环的 declareToolChanges
    /// 公告给模型;系统提示词 tools/rules 节随之 diff 出一条 system patch。
    pub fn set_active_tools_by_name(&self, names: &[String]) -> Result<(), CoreError> {
        let known: Vec<String> = self
            .tools_all
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        for name in names {
            if !known.contains(name) {
                return Err(CoreError::SystemPrompt(format!("unknown tool `{name}`")));
            }
        }
        let active_tools: Vec<Arc<dyn Tool>> = self
            .tools_all
            .iter()
            .filter(|tool| names.iter().any(|name| name == tool.name()))
            .cloned()
            .collect();

        {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.active_tool_names = names.to_vec();
            // 在原始 options 上替换工具片段(保留 custom_prompt/context_files/
            // append/prompt_guidelines 等全部用户配置);Forced 整 prompt 不参与重建
            let mut options = runtime.system_prompt_options.clone();
            options.tool_snippets = active_tools
                .iter()
                .filter_map(|tool| tool.prompt_snippet())
                .collect();
            options.tool_guidelines = active_tools
                .iter()
                .flat_map(|tool| tool.prompt_guidelines())
                .collect();
            // Forced 整 prompt:提示词不随工具集变化,只换工具集本身
            let forced = matches!(runtime.system_prompt, SystemPromptState::Forced(_));
            let new_sections = build_system_prompt_sections(&options)?;
            let patch = if forced {
                Default::default()
            } else {
                diff_system_prompt_sections(&runtime.sections, &new_sections)
            };
            runtime.sections = new_sections.clone();
            if !patch.is_empty() {
                runtime.system_prompt = SystemPromptState::Sections(new_sections);
                self.agent
                    .set_system_prompt(Some(runtime.system_prompt.to_text()));
                runtime.pending_system_messages.push(AgentMessage::System {
                    content: String::new(),
                    sections: patch,
                    tools_added: Vec::new(),
                    tools_removed: Vec::new(),
                    timestamp: rpi_agent::now_ms(),
                });
            }
        }
        self.agent.install_tools(active_tools);
        Ok(())
    }

    /// 切模型(pi setModel):Agent 状态 + model_change entry 同步落盘。
    pub fn set_model(&self, model: Model) {
        self.agent.set_model(model.clone());
        if let Some(sink) = &self.session_sink {
            let sink = sink.clone();
            let provider = model.provider.clone();
            let model_id = model.id.clone();
            tokio::spawn(async move {
                if let Err(error) = sink.append_model_change(&provider, &model_id).await {
                    eprintln!("[rpi] session sink model change append failed: {error}");
                }
            });
        }
    }

    /// 思考级别(pi setThinkingLevel):Agent 状态 + thinking_level_change entry。
    pub fn set_thinking_level(&self, level: Option<rpi_ai::ThinkingLevel>) {
        self.agent.set_thinking_level(level);
        if let (Some(sink), Some(level)) = (&self.session_sink, level) {
            let sink = sink.clone();
            let name = level.as_str().to_string();
            tokio::spawn(async move {
                if let Err(error) = sink.append_thinking_level_change(&name).await {
                    eprintln!("[rpi] session sink thinking level append failed: {error}");
                }
            });
        }
    }

    /// manual compact:与 overflow 恢复共用同一 ContextCompactor(06 文档)。
    /// 返回压缩后的上下文消息数。
    pub async fn compact(&self) -> Result<usize, String> {
        let compactor = self.compactor.as_ref().ok_or("compaction 未装配")?;
        let model = self
            .agent
            .state_snapshot()
            .model
            .ok_or("compaction 需要 model")?;
        let messages = compactor.compact(&model).await?;
        let count = messages.len();
        self.agent
            .set_messages(messages)
            .map_err(|e| e.to_string())?;
        Ok(count)
    }

    /// 更新系统提示词 options:diff 出 sections patch,prompt 时进转录。
    pub fn update_system_prompt(&self, options: SystemPromptOptions) -> Result<(), CoreError> {
        let new_sections = build_system_prompt_sections(&options)?;
        let mut runtime = self.runtime.lock().unwrap();
        let patch = diff_system_prompt_sections(&runtime.sections, &new_sections);
        runtime.sections = new_sections.clone();
        if !patch.is_empty() {
            runtime.system_prompt = SystemPromptState::Sections(new_sections);
            self.agent
                .set_system_prompt(Some(runtime.system_prompt.to_text()));
            runtime.pending_system_messages.push(AgentMessage::System {
                content: String::new(),
                sections: patch,
                tools_added: Vec::new(),
                tools_removed: Vec::new(),
                timestamp: rpi_agent::now_ms(),
            });
        }
        Ok(())
    }

    async fn emit_queue_update(&self) {
        let (steering, follow_up) = self.agent.queue_depths();
        let subscribers = self.subscribers.lock().unwrap().clone();
        let event = AgentSessionEvent::QueueUpdate {
            steering,
            follow_up,
        };
        for subscriber in subscribers {
            subscriber.on_session_event(&event).await;
        }
    }

    /// run + overflow 恢复(04 文档 §2.5;06 文档统一 compaction):context
    /// overflow → 统一 compaction(摘要 + Compaction entry 落盘)→ 投影回填
    /// → continue 重放;每次 run 只尝试一次。
    async fn run_with_recovery<F>(&self, run: F) -> Result<RunStop, CoreError>
    where
        F: std::future::Future<Output = Result<RunStop, AgentError>>,
    {
        {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.overflow_recovery_attempted = false;
        }
        let stop = run.await?;

        if let RunStop::Error(_) = &stop {
            let needs_recovery = {
                let runtime = self.runtime.lock().unwrap();
                !runtime.overflow_recovery_attempted
                    && self
                        .agent
                        .messages()
                        .last()
                        .and_then(|message| message.as_assistant().cloned())
                        .map(|assistant| {
                            let context_window = self
                                .agent()
                                .state_snapshot()
                                .model
                                .as_ref()
                                .map(|m| m.context_window);
                            is_context_overflow(&assistant, context_window)
                        })
                        .unwrap_or(false)
            };
            if needs_recovery {
                self.runtime.lock().unwrap().overflow_recovery_attempted = true;
                self.broadcast(&AgentSessionEvent::AutoRetryStart {
                    attempt: 1,
                    delay_ms: 0,
                    reason: "context overflow: compacting transcript and retrying".into(),
                })
                .await;
                // 统一入口:compaction(摘要 + Compaction entry)+ 投影回填;
                // 未装配 compactor(纯内存测试)时退化为内存裁剪(无 session 可分裂)
                let retry_stop = match &self.compactor {
                    Some(compactor) => match self.agent.state_snapshot().model {
                        Some(model) => match compactor.compact(&model).await {
                            Ok(messages) => match self.agent.set_messages(messages) {
                                Ok(()) => self.agent.continue_run().await,
                                Err(error) => Err(error),
                            },
                            Err(error) => Err(AgentError::Other(error)),
                        },
                        None => Err(AgentError::NoModel),
                    },
                    None => {
                        let message_count = self.agent.messages().len();
                        self.agent.drop_last_message();
                        self.agent.trim_oldest_messages(message_count / 2 + 1);
                        self.agent.continue_run().await
                    }
                };
                self.broadcast(&AgentSessionEvent::AutoRetryEnd {
                    success: matches!(retry_stop, Ok(RunStop::EndTurn)),
                    reason: "overflow recovery".into(),
                })
                .await;
                return retry_stop.map_err(CoreError::from);
            }
        }
        Ok(stop)
    }

    async fn broadcast(&self, event: &AgentSessionEvent) {
        let subscribers = self.subscribers.lock().unwrap().clone();
        for subscriber in subscribers {
            subscriber.on_session_event(event).await;
        }
    }
}

/// core ↔ 扩展的运行期动作面:runner 只依赖此接口,不 import 具体类型(09 A2 接缝 5)。
#[async_trait]
impl ExtensionActions for AgentSession {
    fn ui(&self) -> Arc<dyn ExtensionUi> {
        self.ui.clone()
    }
}
