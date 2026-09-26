//! cli 装配(07 §8.7 步骤 3):settings(`mcpServers`)→ spawn 扩展 → 装配
//! `ExtensionHooks`;诊断打 stderr。`Agent`/`AgentSession` 不感知 MCP —— MCP
//! 扩展只经两条接缝(Subscriber 事件汇 + LoopHooks 包装)注入业务核。
//!
//! `build_session` 是四种模式(print/json/rpc/interactive)共享的装配点
//! (08 文档:模式只是同一业务核的不同 I/O 壳);`run_session` 是 print
//! 模式旧行为的便捷封装,端到端测试复用。

use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use rpi_agent::{AgentEvent, RunStop};
use rpi_core::{
    create_agent_session, create_extension_event_bus, AgentSession, AgentSessionConfig,
    AgentSessionEvent, ExtensionHooks, ExtensionUi, McpServerSpec, NoopUi, SessionSharedSubscriber,
    SessionSink, SessionSubscriber, SystemPromptOptions,
};

/// 读取扩展进程声明:项目 `.rpi/settings.json` → 全局 `~/.rpi/settings.json`,
/// mcpServers 列表拼接(07 §8.7 步骤 3)。
pub fn load_mcp_server_specs() -> Vec<McpServerSpec> {
    let mut specs = Vec::new();
    let mut paths = Vec::new();
    if let Ok(current) = std::env::current_dir() {
        paths.push(current.join(".rpi/settings.json"));
    }
    if let Some(home) = dirs_home() {
        paths.push(home.join(".rpi/settings.json"));
    }
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<SettingsFile>(&text) {
            Ok(settings) => specs.extend(settings.mcp_servers.unwrap_or_default()),
            Err(error) => {
                eprintln!("[rpi] 扩展 settings 解析失败 {}: {error}", path.display());
            }
        }
    }
    specs
}

#[derive(serde::Deserialize)]
struct SettingsFile {
    #[serde(rename = "mcpServers", alias = "mcp_servers", default)]
    mcp_servers: Option<Vec<McpServerSpec>>,
    /// T10:shell 命令统一前缀(settings 提供,如 `commandPrefix: "timeout 300"`)
    #[serde(rename = "commandPrefix", alias = "command_prefix", default)]
    command_prefix: Option<String>,
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// T10:读取 shell 命令前缀,项目 settings 优先于全局 settings。
pub fn load_shell_command_prefix() -> Option<String> {
    let cwd = std::env::current_dir().ok();
    let home = dirs_home();
    shell_command_prefix_from(cwd.as_deref(), home.as_deref())
}

/// 纯函数面(可测):按 项目 → 全局 顺序读 `.rpi/settings.json` 的 commandPrefix,
/// 空白值跳过,首个非空生效。
fn shell_command_prefix_from(cwd: Option<&Path>, home: Option<&Path>) -> Option<String> {
    let mut paths = Vec::new();
    if let Some(cwd) = cwd {
        paths.push(cwd.join(".rpi/settings.json"));
    }
    if let Some(home) = home {
        paths.push(home.join(".rpi/settings.json"));
    }
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<SettingsFile>(&text) {
            Ok(settings) => {
                if let Some(prefix) = settings.command_prefix {
                    if !prefix.trim().is_empty() {
                        return Some(prefix);
                    }
                }
            }
            Err(_) => continue,
        }
    }
    None
}

/// 装配产物:四种模式共享的业务核句柄。`session_manager` 供 rpc 模式的
/// get_tree/get_entries/fork 命令查询会话树(rpi-session 是可选组件)。
pub struct BuiltSession {
    pub session: Arc<AgentSession>,
    pub session_manager: Option<Arc<rpi_session::SessionManager>>,
}

/// 会话存储策略:`Memory` 纯内存(测试);`New` 在目录下新建
/// `<session-id>.jsonl`;`Resume` 打开既有 JSONL 续聊(`--continue`)。
#[derive(Debug, Clone)]
pub enum SessionStore {
    Memory,
    New { dir: std::path::PathBuf },
    Resume { file: std::path::PathBuf },
}

pub struct BuildOptions {
    pub provider: Arc<dyn rpi_ai::Provider>,
    pub model: rpi_ai::Model,
    /// 模式各自的 `ExtensionUi` 实现(print/json 为 no-op,rpc 为反向通道,
    /// interactive 为 TUI 真实现 —— 接缝 #5)
    pub ui: Arc<dyn ExtensionUi>,
    pub extension_specs: Vec<McpServerSpec>,
    /// T10:spawn 前命令改写钩子(可接扩展管线;缺省 None = 不改写)。
    /// commandPrefix 来自 settings(`load_shell_command_prefix`),不经本字段。
    pub spawn_hook: Option<Arc<dyn rpi_tools::ShellSpawnHook>>,
    /// 会话存储策略(CLI 默认文件持久化,`--continue` 续聊)
    pub session_store: SessionStore,
}

/// T9:PI_* 会话环境快照闭包。装配期建共享 cell(`Weak<AgentSession>`),
/// session 建好后回填;工具执行时按需读取,无 session(空 cell)返回空 =
/// 现状行为。禁用可变全局状态(policy §2):经工具配置参数注入。
fn session_env_fn(
    session_cell: Arc<Mutex<Weak<AgentSession>>>,
    session_manager: Arc<rpi_session::SessionManager>,
) -> rpi_tools::SessionEnvFn {
    Arc::new(move || {
        let Some(session) = session_cell.lock().unwrap().upgrade() else {
            return Vec::new();
        };
        let snapshot = session.agent().state_snapshot();
        let mut env: Vec<(String, String)> = Vec::new();
        if let Some(model) = &snapshot.model {
            env.push(("PI_PROVIDER".into(), model.provider.clone()));
            env.push(("PI_MODEL".into(), model.id.clone()));
        }
        if let Some(level) = snapshot.thinking_level {
            env.push((
                "PI_REASONING_LEVEL".into(),
                thinking_level_name(level).into(),
            ));
        }
        env.push((
            "PI_SESSION_ID".into(),
            session_manager.session_id().to_string(),
        ));
        if let Some(path) = session_manager.file_path() {
            env.push(("PI_SESSION_FILE".into(), path.display().to_string()));
        }
        env
    })
}

fn thinking_level_name(level: rpi_ai::ThinkingLevel) -> &'static str {
    match level {
        rpi_ai::ThinkingLevel::Minimal => "minimal",
        rpi_ai::ThinkingLevel::Low => "low",
        rpi_ai::ThinkingLevel::Medium => "medium",
        rpi_ai::ThinkingLevel::High => "high",
        rpi_ai::ThinkingLevel::Xhigh => "xhigh",
        rpi_ai::ThinkingLevel::Max => "max",
    }
}

/// session 投影设置态 → ThinkingLevel("off" = None)。`/thinking` 命令复用。
pub fn parse_thinking_level(name: &str) -> Option<rpi_ai::ThinkingLevel> {
    match name {
        "minimal" => Some(rpi_ai::ThinkingLevel::Minimal),
        "low" => Some(rpi_ai::ThinkingLevel::Low),
        "medium" => Some(rpi_ai::ThinkingLevel::Medium),
        "high" => Some(rpi_ai::ThinkingLevel::High),
        "xhigh" => Some(rpi_ai::ThinkingLevel::Xhigh),
        "max" => Some(rpi_ai::ThinkingLevel::Max),
        _ => None,
    }
}

/// 共享装配:扩展连接失败不阻断(诊断打 stderr,07 §8.5)。
pub async fn build_session(options: BuildOptions) -> Result<BuiltSession, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let BuildOptions {
        provider,
        model,
        ui,
        extension_specs,
        spawn_hook,
        session_store,
    } = options;

    // 扩展:settings → spawn → 总线;连接失败 = 诊断 + 跳过(绝不击穿宿主)
    let diagnostics = rpi_core::create_diagnostics_sink();
    let (bus, extension_tools) =
        create_extension_event_bus(extension_specs, ui.clone(), diagnostics.clone()).await;
    for diagnostic in diagnostics.lock().unwrap().iter() {
        eprintln!(
            "[rpi][extension:{}] {}",
            diagnostic.extension, diagnostic.message
        );
    }

    // 运行期诊断可见:后台 drain 打 stderr(07 §8.5 面向 mode 可见)
    rpi_core::spawn_diagnostics_printer(diagnostics.clone());

    // 决策类埋点:ExtensionHooks 包装内层 hooks(先问扩展再透传)
    let hooks: Arc<dyn rpi_agent::LoopHooks> = if bus.is_empty() {
        Arc::new(rpi_agent::PassthroughHooks)
    } else {
        Arc::new(ExtensionHooks::new(
            Arc::new(rpi_agent::PassthroughHooks),
            bus.clone(),
        ))
    };

    // 会话树管理器先于工具装配创建:T9 的 PI_* 环境闭包需要读 session id/file。
    // 默认文件持久化(`~/.rpi/sessions/<session-id>.jsonl`),Memory 仅测试用
    let session_manager: Arc<rpi_session::SessionManager> = match &session_store {
        SessionStore::Memory => rpi_session::create_session(None::<String>)
            .map_err(|e| e.to_string())?
            .into(),
        SessionStore::New { dir } => {
            rpi_session::create_session_in_dir(dir, &cwd.display().to_string(), None)
                .map_err(|e| e.to_string())?
                .into()
        }
        SessionStore::Resume { file } => rpi_session::create_session(Some(file))
            .map_err(|e| e.to_string())?
            .into(),
    };

    // transcript 统一:resume 时从 Session projection 回填初始转录(source of
    // truth → context);设置态(thinking level)一并恢复
    let (seed_messages, seed_thinking_level) = match &session_store {
        SessionStore::Resume { .. } => {
            let context = rpi_session::build_session_context(
                &session_manager.branch_entries(),
                session_manager.get_leaf_id().as_deref(),
            );
            (context.messages, context.thinking_level)
        }
        _ => (Vec::new(), "off".to_string()),
    };

    // T9/T10:shell 工具装配选项 —— PI_* 会话环境 + settings 命令前缀 + spawn 钩子
    let session_cell: Arc<Mutex<Weak<AgentSession>>> = Arc::new(Mutex::new(Weak::new()));
    let shell = rpi_tools::ShellSpawnOptions {
        session_env: Some(session_env_fn(
            session_cell.clone(),
            session_manager.clone(),
        )),
        command_prefix: load_shell_command_prefix(),
        spawn_hook,
    };

    // 内置工具 + 扩展注册工具(McpTool,名字带扩展前缀)
    let mut tools = rpi_tools::create_tools_at_with_shell(&cwd, shell)
        .all()
        .to_vec();
    tools.extend(extension_tools);

    // 重试装饰器(pi 的 retryAssistantCall 注入点)+ AutoRetry 事件面
    let subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_hooks = rpi_core::create_session_retry_hooks(subscribers.clone());
    let provider = rpi_core::create_retrying_provider(
        provider,
        rpi_ai::RetryPolicy::default(),
        Some(retry_hooks),
    );

    let compactor: Arc<dyn rpi_core::ContextCompactor> = Arc::new(SessionCompactor {
        manager: session_manager.clone(),
        provider: provider.clone(),
        settings: rpi_session::CompactionSettings::default(),
    });
    let session = Arc::new(
        create_agent_session(AgentSessionConfig {
            provider,
            model,
            hooks,
            ui,
            extensions: rpi_core::ExtensionRegistry::default(),
            tools,
            active_tool_names: None,
            system_prompt: SystemPromptOptions {
                cwd: Some(cwd.display().to_string()),
                ..Default::default()
            },
            limits: rpi_agent::TurnLimits::default(),
            stream_options: Default::default(),
            session_sink: Some(Arc::new(SessionManagerSink(session_manager.clone()))),
            seed_messages,
            compactor: Some(compactor),
            subscribers: Some(subscribers.clone()),
        })
        .await
        .map_err(|e| e.to_string())?,
    );

    // resume 设置态恢复:直接回填 Agent 状态(不再落 thinking_level_change entry)
    if let Some(level) = parse_thinking_level(&seed_thinking_level) {
        session.agent().set_thinking_level(Some(level));
    }

    // T9:session 建好后回填共享 cell,PI_* 环境闭包此后可按需快照
    *session_cell.lock().unwrap() = Arc::downgrade(&session);

    // 装配期诊断(编译期扩展 init 失败跳过等)
    for diagnostic in session.extension_diagnostics() {
        eprintln!(
            "[rpi][extension:{}] {}",
            diagnostic.extension, diagnostic.message
        );
    }

    // 观察类埋点:总线作为订阅者挂上两条事件汇(07 §8.3 接入方式)
    if !bus.is_empty() {
        session
            .agent()
            .subscribe(bus.clone() as Arc<dyn rpi_agent::Subscriber>);
        session.subscribe(bus.clone() as SessionSharedSubscriber);
    }

    Ok(BuiltSession {
        session,
        session_manager: Some(session_manager),
    })
}

/// print 模式便捷封装(旧行为,端到端测试复用):装配 + 跑一轮 prompt,
/// 流式增量直接打 stdout。`extra_subscriber` 供测试断言事件。
pub struct SessionRequest {
    pub provider: Arc<dyn rpi_ai::Provider>,
    pub model: rpi_ai::Model,
    pub prompt: String,
    pub extension_specs: Vec<McpServerSpec>,
    pub extra_subscriber: Option<SessionSharedSubscriber>,
    pub session_store: SessionStore,
}

pub async fn run_session(request: SessionRequest) -> Result<RunStop, String> {
    let built = build_session(BuildOptions {
        provider: request.provider,
        model: request.model,
        ui: Arc::new(NoopUi),
        extension_specs: request.extension_specs,
        spawn_hook: None,
        session_store: request.session_store,
    })
    .await?;

    if let Some(subscriber) = request.extra_subscriber {
        built.session.subscribe(subscriber);
    }
    built
        .session
        .subscribe(Arc::new(PrintSubscriber::default()));

    let outcome = built
        .session
        .prompt(request.prompt)
        .await
        .map_err(|e| e.to_string())?;
    built.session.wait_idle().await;
    Ok(outcome.stop())
}

/// rpi-core 的 `SessionSink` 适配器:把可选组件 rpi-session 注入业务核。
/// 拆卸 rpi-session 时删除本结构体即可,core 与其余 crate 不受影响。
/// transcript 统一:消息 / usage / 模型与思考级别变更全部落盘。
struct SessionManagerSink(Arc<rpi_session::SessionManager>);

#[async_trait]
impl SessionSink for SessionManagerSink {
    async fn append(&self, message: &rpi_agent::AgentMessage) -> Result<(), String> {
        self.0
            .append_message(message.clone())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn append_model_change(&self, provider: &str, model_id: &str) -> Result<(), String> {
        self.0
            .append_model_change(provider, model_id)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn append_thinking_level_change(&self, level: &str) -> Result<(), String> {
        self.0
            .append_thinking_level_change(level)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn append_usage(
        &self,
        kind: &str,
        provider: &str,
        model: &str,
        usage: rpi_ai::Usage,
    ) -> Result<(), String> {
        self.0
            .append_usage(kind, provider, model, usage, None)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// 统一 compaction(06 文档):provider 支撑的 Summarizer + SessionCompactor
// ---------------------------------------------------------------------------

/// 用当前会话的 provider/model 发摘要请求(SUMMARIZATION_PROMPT 管线)。
struct ProviderSummarizer {
    provider: Arc<dyn rpi_ai::Provider>,
    model: rpi_ai::Model,
}

#[async_trait]
impl rpi_session::Summarizer for ProviderSummarizer {
    async fn summarize(
        &self,
        request: &rpi_session::SummarizationRequest,
    ) -> Result<rpi_session::SummarizationResponse, String> {
        // 占位 user 承载序列化对话,摘要指令拼接其后
        let mut messages = request.messages.clone();
        match messages.last_mut() {
            Some(rpi_agent::AgentMessage::User { content, .. }) => {
                content.push_str("\n\n");
                content.push_str(&request.instruction);
            }
            _ => messages.push(rpi_agent::AgentMessage::user(&request.instruction)),
        }
        use rpi_agent::LoopHooks as _;
        let llm_messages = rpi_agent::PassthroughHooks.convert_to_llm(&messages);
        let mut stream = self
            .provider
            .stream(
                &self.model,
                rpi_ai::TranscriptContext {
                    messages: llm_messages,
                },
                Default::default(),
            )
            .await;
        use futures::StreamExt;
        let mut result: Option<rpi_session::SummarizationResponse> = None;
        while let Some(event) = stream.next().await {
            match event {
                rpi_ai::AssistantMessageEvent::Done(assistant) => {
                    result = Some(rpi_session::SummarizationResponse {
                        summary: assistant.text_content(),
                        stop_reason: assistant.stop_reason,
                        error_message: assistant.error_message.clone(),
                        usage: Some(assistant.usage),
                    });
                }
                rpi_ai::AssistantMessageEvent::Error(error) => {
                    return Err(error
                        .error_message
                        .unwrap_or_else(|| "summarizer error".into()));
                }
                _ => {}
            }
        }
        result.ok_or_else(|| "summarizer stream ended without a message".to_string())
    }
}

/// overflow 恢复与 manual /compact 共用的压缩实现:
/// 移除触发溢出的错误 assistant(ContextEdit)→ run_compaction →
/// Compaction entry 落盘(原始历史保留)→ Session projection 回填上下文。
struct SessionCompactor {
    manager: Arc<rpi_session::SessionManager>,
    provider: Arc<dyn rpi_ai::Provider>,
    settings: rpi_session::CompactionSettings,
}

#[async_trait]
impl rpi_core::ContextCompactor for SessionCompactor {
    async fn compact(&self, model: &rpi_ai::Model) -> Result<Vec<rpi_agent::AgentMessage>, String> {
        // 末尾是 overflow 错误 assistant 时剔除出上下文(append-only ContextEdit;
        // 同时满足 continue_run"最后一条非 assistant"的前置条件)
        let entries = self.manager.branch_entries();
        if let Some(rpi_session::Entry::Message {
            id,
            message: rpi_agent::AgentMessage::Assistant(assistant),
            ..
        }) = entries.last()
        {
            if matches!(assistant.stop_reason, rpi_ai::StopReason::Error) {
                self.manager
                    .append_context_edit(id, None)
                    .map_err(|e| e.to_string())?;
            }
        }

        let entries = self.manager.branch_entries();
        let summarizer = ProviderSummarizer {
            provider: self.provider.clone(),
            model: model.clone(),
        };
        if let Some(outcome) =
            rpi_session::run_compaction(&entries, &self.settings, &summarizer).await?
        {
            self.manager
                .append_compaction(
                    outcome.summary,
                    outcome.first_kept_entry_id,
                    outcome.tokens_before,
                    Some(outcome.details),
                    outcome.usage,
                    false,
                    outcome.system_message,
                )
                .map_err(|e| e.to_string())?;
        }
        // 压缩后上下文一律从 Session projection 重建(source of truth)
        Ok(self.manager.projection().messages)
    }
}

/// mode 侧订阅者(接缝 #5):流式增量直接打印,工具调用打印一行状态。
#[derive(Default)]
pub struct PrintSubscriber {
    streamed: Mutex<String>,
}

#[async_trait]
impl SessionSubscriber for PrintSubscriber {
    async fn on_session_event(&self, event: &AgentSessionEvent) {
        match event {
            AgentSessionEvent::Agent(agent_event) => match agent_event {
                AgentEvent::MessageDelta {
                    delta: rpi_agent::MessageDeltaPayload::Text { delta },
                } => {
                    // T2:print 模式只上文本增量(thinking/参数增量不上 stdout)
                    use std::io::Write;
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                    self.streamed.lock().unwrap().push_str(delta);
                }
                AgentEvent::MessageEnd { message } => {
                    if matches!(**message, rpi_agent::AgentMessage::Assistant(_)) {
                        println!();
                    }
                }
                AgentEvent::ToolExecutionStart { tool_name, .. } => {
                    println!("[tool] {tool_name} …");
                }
                _ => {}
            },
            AgentSessionEvent::AgentSettled => {}
            AgentSessionEvent::QueueUpdate { .. } => {}
            AgentSessionEvent::AutoRetryStart {
                attempt,
                delay_ms,
                reason,
            } => {
                eprintln!("[retry #{attempt} in {delay_ms}ms] {reason}");
            }
            AgentSessionEvent::AutoRetryEnd { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rpi_prefix_test_{}_{}",
                tag,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
        fn write_settings(&self, text: &str) {
            std::fs::create_dir_all(self.0.join(".rpi")).unwrap();
            std::fs::write(self.0.join(".rpi/settings.json"), text).unwrap();
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn command_prefix_from_project_settings() {
        let project = TempDir::new("project");
        project.write_settings(r#"{"commandPrefix": "timeout 300"}"#);
        assert_eq!(
            shell_command_prefix_from(Some(&project.0), None),
            Some("timeout 300".into()),
            "T10:settings commandPrefix 应进入装配通道"
        );
    }

    #[test]
    fn project_settings_win_over_global_and_blank_is_skipped() {
        let project = TempDir::new("prio");
        let global = TempDir::new("prio_global");
        global.write_settings(r#"{"commandPrefix": "global-prefix"}"#);
        assert_eq!(
            shell_command_prefix_from(Some(&project.0), Some(&global.0)),
            Some("global-prefix".into()),
            "项目无 settings 时回退全局"
        );
        project.write_settings(r#"{"commandPrefix": "   "}"#);
        assert_eq!(
            shell_command_prefix_from(Some(&project.0), Some(&global.0)),
            Some("global-prefix".into()),
            "项目空白前缀跳过,继续看全局"
        );
        project.write_settings(r#"{"commandPrefix": "project-prefix"}"#);
        assert_eq!(
            shell_command_prefix_from(Some(&project.0), Some(&global.0)),
            Some("project-prefix".into()),
            "项目非空前缀优先于全局"
        );
    }

    #[test]
    fn missing_settings_yields_none() {
        let project = TempDir::new("none");
        assert_eq!(shell_command_prefix_from(Some(&project.0), None), None);
    }

    // ---- transcript 统一:sink 扩展 entry + 统一 compaction ----

    #[tokio::test]
    async fn sink_persists_model_thinking_usage_entries() {
        let manager: Arc<rpi_session::SessionManager> =
            rpi_session::create_session(None::<String>).unwrap().into();
        let sink = SessionManagerSink(manager.clone());
        sink.append(&rpi_agent::AgentMessage::user("hi"))
            .await
            .unwrap();
        sink.append_model_change("openai", "gpt-5").await.unwrap();
        sink.append_thinking_level_change("high").await.unwrap();
        sink.append_usage("message", "openai", "gpt-5", rpi_ai::Usage::default())
            .await
            .unwrap();
        let kinds: Vec<String> = manager
            .entries()
            .iter()
            .map(|entry| match entry {
                rpi_session::Entry::Message { .. } => "message".into(),
                rpi_session::Entry::ModelChange { .. } => "model_change".into(),
                rpi_session::Entry::ThinkingLevelChange { .. } => "thinking_level".into(),
                rpi_session::Entry::Usage { .. } => "usage".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["message", "model_change", "thinking_level", "usage"]
        );
    }

    #[tokio::test]
    async fn session_compactor_appends_compaction_and_rebuilds_projection() {
        use rpi_agent::AgentMessage;

        let manager: Arc<rpi_session::SessionManager> =
            rpi_session::create_session(None::<String>).unwrap().into();
        // transcript:System 声明 → user → assistant(正常)→ assistant(overflow 错误)
        manager
            .append_message(AgentMessage::System {
                content: String::new(),
                sections: Default::default(),
                tools_added: Vec::new(),
                tools_removed: Vec::new(),
                timestamp: 0,
            })
            .unwrap();
        manager
            .append_message(AgentMessage::user("第一轮问题"))
            .unwrap();
        let m = rpi_ai::Model::minimal("m", "mock", "mock");
        let mut ok = rpi_ai::AssistantMessage::pending(&m);
        ok.content = vec![rpi_ai::ContentBlock::text("回答")];
        ok.stop_reason = rpi_ai::StopReason::Stop;
        ok.usage.total_tokens = 100;
        manager
            .append_message(AgentMessage::Assistant(Box::new(ok)))
            .unwrap();
        let mut error = rpi_ai::AssistantMessage::pending(&m);
        error.stop_reason = rpi_ai::StopReason::Error;
        error.error_message = Some("prompt is too long: 2000 tokens > 100 maximum".into());
        manager
            .append_message(AgentMessage::Assistant(Box::new(error)))
            .unwrap();

        let compactor = SessionCompactor {
            manager: manager.clone(),
            provider: rpi_ai::create_mock_provider("## Goal\n摘要内容"),
            settings: rpi_session::CompactionSettings {
                enabled: true,
                reserve_tokens: 0,
                keep_recent_tokens: 0,
            },
        };
        use rpi_core::ContextCompactor as _;
        let messages = compactor.compact(&m).await.unwrap();

        // 原始历史保留(append-only):4 条消息 entry 都在
        let entries = manager.entries();
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(e, rpi_session::Entry::Message { .. }))
                .count(),
            4,
            "原始消息不删除"
        );
        // Compaction + ContextEdit(剔除错误 assistant)entry 已落盘
        assert!(entries
            .iter()
            .any(|e| matches!(e, rpi_session::Entry::Compaction { .. })));
        assert!(entries
            .iter()
            .any(|e| matches!(e, rpi_session::Entry::ContextEdit { .. })));
        // 压缩后上下文来自 projection:system 快照 + 摘要,被摘要消息不再出现
        // 压缩后上下文:system 快照 + 摘要 + 保留的近期回复;
        // 错误 assistant(ContextEdit 剔除)与被摘要的 user 不再出现
        assert!(matches!(
            messages.first(),
            Some(AgentMessage::System { .. })
        ));
        assert!(messages
            .iter()
            .any(|m| matches!(m, AgentMessage::CompactionSummary { .. })));
        assert!(!messages
            .iter()
            .any(|m| matches!(m, AgentMessage::User { .. })));
        assert!(
            !messages.iter().any(|m| matches!(m,
                AgentMessage::Assistant(a) if matches!(a.stop_reason, rpi_ai::StopReason::Error))),
            "触发溢出的错误 assistant 应被 ContextEdit 剔除"
        );
    }
}
