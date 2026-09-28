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
use crate::permission::{
    ApprovalDecision, ApprovalRequest, PermissionEngine, SessionMode,
};
use crate::retry::RetryHooks;
use crate::system_prompt::{
    build_system_prompt_sections, build_system_prompt_state, SystemPromptOptions,
    SystemPromptSections, SystemPromptState,
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

    /// 激活工具集变更(元数据 entry,不进模型上下文;恢复时按此重建激活集)
    async fn append_tool_set_change(&self, _tools: &[String]) -> Result<(), String> {
        Ok(())
    }

    /// 会话模式变更(元数据 entry,不进模型上下文;恢复时按此重建模式)
    async fn append_mode_change(&self, _mode: &str) -> Result<(), String> {
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

    /// 自动压缩阈值判定(06 文档 §3.1):估算当前上下文 token,超过
    /// contextWindow - reserveTokens 时返回 true。由装配方实现(core 不依赖
    /// rpi-session,估算基于 Session projection);默认 false = 不自动压缩。
    fn should_auto_compact(&self, _model: &Model, _messages: &[AgentMessage]) -> bool {
        false
    }
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
    /// 审批请求已弹出(hook 内广播;UI 据此绘制弹窗/压队列)
    ApprovalRequested {
        request: ApprovalRequest,
    },
    /// 审批已决策(含会话缓存命中自动批准的,decision 照发)
    ApprovalResolved {
        tool_call_id: String,
        decision: ApprovalDecision,
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
    /// 权限引擎(可选):未装配 = 无权限行为(现状退化,可拆卸判据)。
    /// `set_mode` 经此触达引擎切档;ApprovalHooks 持有同一 Arc。
    pub permission: Option<Arc<PermissionEngine>>,
    /// 模式提示词共享 cell(可选;ModeHooks 持有同一 Arc):apply_mode 写入
    /// 当前模式的请求级补充指令(如 Plan 节),每请求以 Developer 消息追加在
    /// 消息数组末尾 —— 系统提示词与工具数组随模式恒定(保 KV 缓存前缀)。
    /// None = 无模式提示词行为。
    pub mode_section_cell: Option<Arc<Mutex<Option<String>>>>,
}

/// 运行期可变状态(锁保护;无全局状态)。
struct SessionRuntime {
    system_prompt: SystemPromptState,
    sections: SystemPromptSections,
    /// 原始构建 options:工具集变更时在其上替换工具片段重建,
    /// 避免丢 custom_prompt/context_files/append 等用户配置
    system_prompt_options: SystemPromptOptions,
    active_tool_names: Vec<String>,
    /// 当前会话模式(与 PermissionEngine 同步;未装配引擎时也是唯一事实源)
    mode: SessionMode,
    /// 新会话默认模式(/new 与新建会话落盘用;装配期由 settings/CLI 决定)
    default_mode: SessionMode,
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
    /// 权限引擎(可选;见 AgentSessionConfig.permission)
    permission: Option<Arc<PermissionEngine>>,
    /// 模式提示词共享 cell(与 ModeHooks 共享;apply_mode 写)
    mode_section_cell: Option<Arc<Mutex<Option<String>>>>,
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
        permission: config.permission,
        extension_diagnostics,
        mode_section_cell: config.mode_section_cell,
        runtime: Mutex::new(SessionRuntime {
            system_prompt: state,
            system_prompt_options: options,
            sections,
            active_tool_names,
            mode: SessionMode::Plan,
            default_mode: SessionMode::Plan,
            overflow_recovery_attempted: false,
        }),
    })
}

/// 子 agent 转录持久化订阅者(裸 `rpi_agent::Agent` 用,SessionBridge 的
/// 持久化半边):MessageEnd → sink.append + usage entry —— 子会话 JSONL
/// 条目格式与主会话完全一致。
pub fn create_session_persistence_subscriber(
    sink: Arc<dyn SessionSink>,
) -> rpi_agent::SharedSubscriber {
    Arc::new(SessionBridge {
        subscribers: Arc::new(Mutex::new(Vec::new())),
        sink: Some(sink),
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
        let run = self.agent.prompt_messages(vec![AgentMessage::user(text.clone())]);
        match self.run_with_recovery(run).await {
            Ok(stop) => Ok(PromptOutcome::Started(stop)),
            // TOCTOU:is_streaming 检查后并发 prompt 抢先启动了 run →
            // 按流式语义转 steer(reviewer P2)
            Err(CoreError::Agent(AgentError::AlreadyRunning)) => {
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

    /// 切换会话模式(13 文档 §8.4,模式切换的唯一入口):工具集 + 系统提示词
    /// mode 节 + 引擎切档 + entry 落盘,一次完成。落盘内联 await(理由同
    /// set_model:spawn 异步写会让 entry 排在后续消息之后)。
    pub async fn set_mode(&self, mode: SessionMode) -> Result<(), CoreError> {
        self.apply_mode(mode, true).await
    }

    /// 恢复场景的 mode 应用(不落 entry;resume 回填用)。
    pub async fn apply_mode_without_persist(&self, mode: SessionMode) -> Result<(), CoreError> {
        self.apply_mode(mode, false).await
    }

    /// 新会话默认模式(/new 落盘用)。
    pub fn default_mode(&self) -> SessionMode {
        self.runtime.lock().unwrap().default_mode
    }

    pub fn set_default_mode(&self, mode: SessionMode) {
        self.runtime.lock().unwrap().default_mode = mode;
    }

    /// 当前会话模式(footer 状态栏/交互展示用)。
    pub fn mode(&self) -> SessionMode {
        self.runtime.lock().unwrap().mode
    }

    /// 当前系统提示词 sections(诊断/测试用;模式节已迁出,不再含 `<mode>`)。
    pub fn system_prompt_sections(&self) -> SystemPromptSections {
        self.runtime.lock().unwrap().sections.clone()
    }

    async fn apply_mode(&self, mode: SessionMode, persist: bool) -> Result<(), CoreError> {
        // 模式切换三件事:写模式提示词 cell、引擎切档、entry 落盘。
        // 系统提示词与激活工具集**不随模式变化**(tools 数组恒定保 KV 缓存
        // 前缀命中);Plan 的只读约束由权限引擎在运行时强制(bash 只读检查 /
        // write Deny / 沙箱 ReadOnly 包装),拒绝原因进转录模型可自行换路径。
        self.runtime.lock().unwrap().mode = mode;
        if let Some(cell) = &self.mode_section_cell {
            *cell.lock().unwrap() = crate::permission::mode_section(mode);
        }

        // 引擎切档(清审批缓存)
        if let Some(engine) = &self.permission {
            engine.set_mode(mode);
        }
        if persist {
            if let Some(sink) = &self.session_sink {
                if let Err(error) = sink.append_mode_change(mode.as_str()).await {
                    eprintln!("[rpi] session sink mode change append failed: {error}");
                }
            }
        }
        Ok(())
    }

    /// 切换激活工具集:重建系统提示词 sections(请求级字段)+ 换可执行工具集,
    /// 并落一条 tool_set_change 元数据 entry(恢复时按此重建激活集)。
    /// session 不持久化任何提示词文本 —— 工具 schema 每次请求动态下发。
    pub async fn set_active_tools_by_name(&self, names: &[String]) -> Result<(), CoreError> {
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
            if !forced {
                runtime.system_prompt = SystemPromptState::Sections(new_sections.clone());
                self.agent
                    .set_system_prompt(Some(runtime.system_prompt.to_text()));
            }
            runtime.sections = new_sections;
        }
        self.agent.install_tools(active_tools);
        if let Some(sink) = &self.session_sink {
            if let Err(error) = sink.append_tool_set_change(names).await {
                eprintln!("[rpi] session sink tool set change append failed: {error}");
            }
        }
        Ok(())
    }

    /// 切模型(pi setModel):Agent 状态 + model_change entry 同步落盘。
    /// 落盘内联 await:与 SessionBridge 的消息 append 保持文件内顺序,
    /// spawn 异步写会让 model_change 排在后续 message 之后(恢复时投影出错模型)。
    pub async fn set_model(&self, model: Model) {
        self.agent.set_model(model.clone());
        if let Some(sink) = &self.session_sink {
            if let Err(error) = sink
                .append_model_change(&model.provider, &model.id)
                .await
            {
                eprintln!("[rpi] session sink model change append failed: {error}");
            }
        }
    }

    /// 思考级别(pi setThinkingLevel):Agent 状态 + thinking_level_change entry。
    /// 落盘内联 await,理由同 set_model。
    pub async fn set_thinking_level(&self, level: Option<rpi_ai::ThinkingLevel>) {
        self.agent.set_thinking_level(level);
        if let (Some(sink), Some(level)) = (&self.session_sink, level) {
            if let Err(error) = sink.append_thinking_level_change(level.as_str()).await {
                eprintln!("[rpi] session sink thinking level append failed: {error}");
            }
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
                self.maybe_auto_compact().await;
                return retry_stop.map_err(CoreError::from);
            }
        }
        if let RunStop::EndTurn = &stop {
            self.maybe_auto_compact().await;
        }
        Ok(stop)
    }

    /// 自动压缩(06 文档 §3.1):run 成功结束后按阈值检查,超限则立即压缩,
    /// 下一次 prompt 从压缩后上下文开始 —— 避免下一轮请求直接 overflow,
    /// 白耗一次失败调用(阈值判定由装配方注入,见 ContextCompactor)。
    async fn maybe_auto_compact(&self) {
        let Some(compactor) = self.compactor.as_ref() else {
            return;
        };
        let Some(model) = self.agent.state_snapshot().model else {
            return;
        };
        let messages = self.agent.messages();
        if !compactor.should_auto_compact(&model, &messages) {
            return;
        }
        self.broadcast(&AgentSessionEvent::AutoRetryStart {
            attempt: 1,
            delay_ms: 0,
            reason: "context threshold reached: auto-compacting".into(),
        })
        .await;
        let result = match compactor.compact(&model).await {
            Ok(messages) => self
                .agent
                .set_messages(messages)
                .map_err(|e| e.to_string())
                .map(|_| ()),
            Err(error) => Err(error),
        };
        self.broadcast(&AgentSessionEvent::AutoRetryEnd {
            success: result.is_ok(),
            reason: "auto compaction".into(),
        })
        .await;
        if let Err(error) = result {
            eprintln!("[rpi] auto compaction failed: {error}");
        }
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
