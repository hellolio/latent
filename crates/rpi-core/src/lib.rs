//! rpi-core —— 业务核:`AgentSession`、系统提示词 sections、模型解析、
//! 重试装饰、扩展注册面(04 文档)。
//!
//! 对外只暴露:工厂(`create_agent_session`/`create_model_resolver`/
//! `create_retrying_provider`/`create_session_retry_hooks`)、trait(`ExtensionUi`
//! /`ExtensionActions`/`Extension`/`SessionSink`/`SessionSubscriber`)、纯类型。

pub mod config;
pub mod extensions;
pub mod model;
pub mod retry;
pub mod session;
pub mod system_prompt;

pub use config::{create_model_resolver_from_config, load_default_model_selection};
pub use extensions::{
    bridge_elicitation, connect_stdio, create_diagnostics_sink, create_extension_event_bus,
    spawn_diagnostics_printer,
    Extension, ExtensionActions, ExtensionApi, ExtensionDiagnostic, ExtensionEvent,
    ExtensionEventBus, ExtensionHooks, ExtensionRegistry, ExtensionUi, McpConnection,
    McpServerSpec, NoopUi,
};
pub use model::{create_model_resolver, default_model_for, ModelResolver};
pub use retry::{create_retrying_provider, RetryHooks};
pub use session::{
    create_agent_session, create_session_retry_hooks, AgentSession, AgentSessionConfig,
    AgentSessionEvent, ContextCompactor, CoreError, PromptOutcome, SessionSharedSubscriber,
    SessionSink, SessionSubscriber,
};
pub use system_prompt::{
    build_system_prompt_sections, build_system_prompt_state, diff_system_prompt_sections,
    sections_to_text, SystemPromptOptions, SystemPromptSections, SystemPromptState,
};
