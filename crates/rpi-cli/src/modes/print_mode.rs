//! print 模式(08 文档 §1):跑一个 prompt,流式输出最终回复到 stdout;
//! 非 TTY 自动进入(main 侧判定)。

use std::sync::Arc;

use rpi_agent::RunStop;
use rpi_core::McpServerSpec;

use crate::assembly::{
    build_session, run_session, BuildOptions, SessionRequest, SessionSettings, SessionStore,
};

/// print 模式入口:装配(NoopUi,headless 无交互)→ prompt → 流式打印。
pub async fn run_print_mode(
    provider: Arc<dyn rpi_ai::Provider>,
    model: rpi_ai::Model,
    prompt: String,
    extension_specs: Vec<McpServerSpec>,
    session_store: SessionStore,
    settings: SessionSettings,
    session_mode_override: Option<rpi_core::SessionMode>,
) -> Result<RunStop, String> {
    run_session(SessionRequest {
        provider,
        model,
        prompt,
        extension_specs,
        extra_subscriber: None,
        session_store,
        settings,
        session_mode_override,
    })
    .await
}

/// 供其他模式复用的装配(不跑 prompt)。`approval_ui` 由调用方按模式提供
/// (interactive=TuiApprovalUi,rpc=RpcApprovalUi,json=HeadlessApprovalUi)。
pub async fn build_bare_session(
    provider: Arc<dyn rpi_ai::Provider>,
    model: rpi_ai::Model,
    ui: Arc<dyn rpi_core::ExtensionUi>,
    extension_specs: Vec<McpServerSpec>,
    session_store: SessionStore,
    settings: SessionSettings,
    approval_ui: Option<Arc<dyn rpi_core::ApprovalUi>>,
    rpc_approval: Option<Arc<crate::modes::rpc::RpcApprovalUi>>,
    session_mode_override: Option<rpi_core::SessionMode>,
) -> Result<crate::assembly::BuiltSession, String> {
    build_session(BuildOptions {
        provider,
        model,
        ui,
        extension_specs,
        spawn_hook: None,
        session_store,
        context_snapshot: Some(settings.context_snapshot),
        active_tools: settings.active_tools,
        tool_result_max_chars: settings.tool_result_max_chars,
        compaction: settings.compaction,
        session_mode: session_mode_override,
        default_session_mode: settings.session_mode,
        sandbox: settings.sandbox,
        approval: settings.approval,
        subagent_async_approval: settings.subagent_async_approval,
        approval_ui,
        rpc_approval,
    })
    .await
}
