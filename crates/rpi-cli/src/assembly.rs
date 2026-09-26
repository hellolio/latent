//! cli 装配(07 §8.7 步骤 3):settings(`mcpServers`)→ spawn 扩展 → 装配
//! `ExtensionHooks`;诊断打 stderr。`Agent`/`AgentSession` 不感知 MCP —— MCP
//! 扩展只经两条接缝(Subscriber 事件汇 + LoopHooks 包装)注入业务核。
//!
//! `build_session` 是四种模式(print/json/rpc/interactive)共享的装配点
//! (08 文档:模式只是同一业务核的不同 I/O 壳);`run_session` 是 print
//! 模式旧行为的便捷封装,端到端测试复用。

use std::sync::{Arc, Mutex};

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
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// 装配产物:四种模式共享的业务核句柄。`session_manager` 供 rpc 模式的
/// get_tree/get_entries/fork 命令查询会话树(rpi-session 是可选组件)。
pub struct BuiltSession {
    pub session: Arc<AgentSession>,
    pub session_manager: Option<Arc<rpi_session::SessionManager>>,
}

pub struct BuildOptions {
    pub provider: Arc<dyn rpi_ai::Provider>,
    pub model: rpi_ai::Model,
    /// 模式各自的 `ExtensionUi` 实现(print/json 为 no-op,rpc 为反向通道,
    /// interactive 为 TUI 真实现 —— 接缝 #5)
    pub ui: Arc<dyn ExtensionUi>,
    pub extension_specs: Vec<McpServerSpec>,
}

/// 共享装配:扩展连接失败不阻断(诊断打 stderr,07 §8.5)。
pub async fn build_session(options: BuildOptions) -> Result<BuiltSession, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let BuildOptions { provider, model, ui, extension_specs } = options;

    // 扩展:settings → spawn → 总线;连接失败 = 诊断 + 跳过(绝不击穿宿主)
    let diagnostics = rpi_core::create_diagnostics_sink();
    let (bus, extension_tools) =
        create_extension_event_bus(extension_specs, ui.clone(), diagnostics.clone()).await;
    for diagnostic in diagnostics.lock().unwrap().iter() {
        eprintln!("[rpi][extension:{}] {}", diagnostic.extension, diagnostic.message);
    }

    // 运行期诊断可见:后台 drain 打 stderr(07 §8.5 面向 mode 可见)
    rpi_core::spawn_diagnostics_printer(diagnostics.clone());

    // 决策类埋点:ExtensionHooks 包装内层 hooks(先问扩展再透传)
    let hooks: Arc<dyn rpi_agent::LoopHooks> = if bus.is_empty() {
        Arc::new(rpi_agent::PassthroughHooks)
    } else {
        Arc::new(ExtensionHooks::new(Arc::new(rpi_agent::PassthroughHooks), bus.clone()))
    };

    // 内置工具 + 扩展注册工具(McpTool,名字带扩展前缀)
    let mut tools = rpi_tools::create_tools_at(&cwd).all().to_vec();
    tools.extend(extension_tools);

    // 重试装饰器(pi 的 retryAssistantCall 注入点)+ AutoRetry 事件面
    let subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_hooks = rpi_core::create_session_retry_hooks(subscribers.clone());
    let provider = rpi_core::create_retrying_provider(provider, rpi_ai::RetryPolicy::default(), Some(retry_hooks));

    let session_manager: Arc<rpi_session::SessionManager> =
        rpi_session::create_session(None::<String>).map_err(|e| e.to_string())?.into();

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
            subscribers: Some(subscribers.clone()),
        })
        .await
        .map_err(|e| e.to_string())?,
    );

    // 装配期诊断(编译期扩展 init 失败跳过等)
    for diagnostic in session.extension_diagnostics() {
        eprintln!("[rpi][extension:{}] {}", diagnostic.extension, diagnostic.message);
    }

    // 观察类埋点:总线作为订阅者挂上两条事件汇(07 §8.3 接入方式)
    if !bus.is_empty() {
        session.agent().subscribe(bus.clone() as Arc<dyn rpi_agent::Subscriber>);
        session.subscribe(bus.clone() as SessionSharedSubscriber);
    }

    Ok(BuiltSession { session, session_manager: Some(session_manager) })
}

/// print 模式便捷封装(旧行为,端到端测试复用):装配 + 跑一轮 prompt,
/// 流式增量直接打 stdout。`extra_subscriber` 供测试断言事件。
pub struct SessionRequest {
    pub provider: Arc<dyn rpi_ai::Provider>,
    pub model: rpi_ai::Model,
    pub prompt: String,
    pub extension_specs: Vec<McpServerSpec>,
    pub extra_subscriber: Option<SessionSharedSubscriber>,
}

pub async fn run_session(request: SessionRequest) -> Result<RunStop, String> {
    let built = build_session(BuildOptions {
        provider: request.provider,
        model: request.model,
        ui: Arc::new(NoopUi),
        extension_specs: request.extension_specs,
    })
    .await?;

    if let Some(subscriber) = request.extra_subscriber {
        built.session.subscribe(subscriber);
    }
    built.session.subscribe(Arc::new(PrintSubscriber::default()));

    let stop = built.session.prompt(request.prompt).await.map_err(|e| e.to_string())?;
    built.session.wait_idle().await;
    Ok(stop)
}

/// rpi-core 的 `SessionSink` 适配器:把可选组件 rpi-session 注入业务核。
/// 拆卸 rpi-session 时删除本结构体即可,core 与其余 crate 不受影响。
struct SessionManagerSink(Arc<rpi_session::SessionManager>);

#[async_trait]
impl SessionSink for SessionManagerSink {
    async fn append(&self, message: &rpi_agent::AgentMessage) -> Result<(), String> {
        self.0
            .append_message(message.clone())
            .map(|_| ())
            .map_err(|e| e.to_string())
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
                AgentEvent::MessageDelta { delta } => {
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
            AgentSessionEvent::AutoRetryStart { attempt, delay_ms, reason } => {
                eprintln!("[retry #{attempt} in {delay_ms}ms] {reason}");
            }
            AgentSessionEvent::AutoRetryEnd { .. } => {}
        }
    }
}
