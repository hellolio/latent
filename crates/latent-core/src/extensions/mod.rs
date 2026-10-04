//! 接缝 #4/#5(core ↔ 扩展、core ↔ mode):扩展注册面与 UI 抽象(07 文档)。
//!
//! 稳定边界是**事件干预语义 + `ExtensionUi` 抽象**,加载机制可替换(09 B4):
//! - 编译期注册(`Extension` trait,工具注册);
//! - 进程外 MCP 扩展(07 §8:B 方案,`ExtensionEventBus` + `McpConnection`,
//!   通讯复用 rmcp,埋点走 `latent/register` / `latent/event` 自定义方法)。
//!
//! 宿主与扩展的错误语义(07 §8.5):**跳过 + 收集诊断,绝不击穿宿主**。

mod event_bus;
mod mcp_host;
mod mcp_tool;

use std::sync::Arc;

use async_trait::async_trait;

use crate::session::CoreError;
use latent_agent::Tool;

use event_bus::record_diagnostic;

pub use event_bus::{
    create_diagnostics_sink, spawn_diagnostics_printer, DecisionOutcome, DiagnosticsSink,
    EventOutcome, EventPolicy, ExtensionDiagnostic, ExtensionEvent, ExtensionEventBus,
    ExtensionHooks, ExtensionRegistration, DEFAULT_EVENT_TIMEOUT_MS,
};
pub use mcp_host::{
    bridge_elicitation, connect_stdio, connect_transport, McpConnection, McpServerSpec,
};
pub use mcp_tool::{McpTool, McpToolMeta};

/// 装配便利工厂:连接扩展进程 + 建总线,返回(总线, 已连接扩展注册的工具)。
/// 连接失败的扩展记诊断跳过(07 §8.5),不阻断装配。
pub async fn create_extension_event_bus(
    specs: Vec<McpServerSpec>,
    ui: Arc<dyn ExtensionUi>,
    diagnostics: DiagnosticsSink,
) -> (Arc<ExtensionEventBus>, Vec<Arc<dyn Tool>>) {
    let mut connections = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    for spec in specs {
        let name = spec.name.clone().unwrap_or_else(|| {
            spec.command
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("mcp")
                .to_string()
        });
        // 装配期唯一性:重名扩展的工具前缀会冲突,记诊断跳过(07 §8.5)
        if !seen_names.insert(name.clone()) {
            record_diagnostic(
                &diagnostics,
                &name,
                "duplicate extension name, skipped".into(),
            );
            continue;
        }
        match connect_stdio(spec, ui.clone(), diagnostics.clone()).await {
            Ok(connection) => connections.push(connection),
            Err(message) => record_diagnostic(&diagnostics, &name, message),
        }
    }
    let bus = Arc::new(ExtensionEventBus::new(connections.clone(), diagnostics));
    let tools = connections
        .iter()
        .flat_map(|connection| connection.create_tools())
        .collect();
    (bus, tools)
}

/// 每个 mode 提供自己的实现:interactive 真 UI,print/json 为 no-op(pi 一比一)。
/// select/input/confirm 带默认 no-op 实现(07 §8.6 接缝 #5 修改):headless 下
/// confirm 放行、select/input 拒绝,print/json 模式零改动。
#[async_trait]
pub trait ExtensionUi: Send + Sync {
    async fn notify(&self, message: &str);

    /// 确认对话框(布尔);headless 默认放行。
    async fn confirm(&self, _message: &str) -> bool {
        true
    }

    /// 单选;返回选项下标,None = 用户取消。
    async fn select(&self, _message: &str, _options: &[String]) -> Option<usize> {
        None
    }

    /// 文本输入;None = 用户取消。
    async fn input(&self, _message: &str) -> Option<String> {
        None
    }
}

pub struct NoopUi;

#[async_trait]
impl ExtensionUi for NoopUi {
    async fn notify(&self, _message: &str) {}
}

/// 扩展拿到的注册面:装配期持有,运行期只读(09 A4)。
pub struct ExtensionApi<'a> {
    tools: &'a mut Vec<Arc<dyn Tool>>,
    ui: Arc<dyn ExtensionUi>,
}

impl<'a> ExtensionApi<'a> {
    pub fn new(tools: &'a mut Vec<Arc<dyn Tool>>, ui: Arc<dyn ExtensionUi>) -> Self {
        Self { tools, ui }
    }

    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn ui(&self) -> Arc<dyn ExtensionUi> {
        self.ui.clone()
    }
}

/// 编译期注册的扩展(09 B4 路线一);init 不得 panic,失败返回 Err
/// (init 失败 = 该扩展整个丢弃,注册的工具不留,07 §8.5)。
#[async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;

    async fn init(&self, api: &mut ExtensionApi<'_>) -> Result<(), CoreError>;
}

/// 装配期收集扩展;runner 按注册顺序执行、单 handler 错误不击穿宿主(07 文档)。
#[derive(Default)]
pub struct ExtensionRegistry {
    extensions: Vec<Arc<dyn Extension>>,
}

impl ExtensionRegistry {
    pub fn register(&mut self, extension: Arc<dyn Extension>) {
        self.extensions.push(extension);
    }

    pub fn all(&self) -> &[Arc<dyn Extension>] {
        &self.extensions
    }
}

/// core ↔ 扩展的运行期动作面(09 A2 接缝 5):扩展 runner 只依赖此接口,
/// 由 `AgentSession` 实现 —— runner 不知道 AgentSession 的存在。
#[async_trait]
pub trait ExtensionActions: Send + Sync {
    fn ui(&self) -> Arc<dyn ExtensionUi>;

    async fn notify(&self, message: &str) {
        self.ui().notify(message).await;
    }
}
