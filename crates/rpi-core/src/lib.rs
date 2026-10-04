//! rpi-core —— 业务核:`AgentSession`、系统提示词 sections、模型解析、
//! 重试装饰、扩展注册面(04 文档)。
//!
//! 对外只暴露:工厂(`create_agent_session`/`create_model_resolver`/
//! `create_retrying_provider`/`create_session_retry_hooks`)、trait(`ExtensionUi`
//! /`ExtensionActions`/`Extension`/`SessionSink`/`SessionSubscriber`)、纯类型。

pub mod config;
pub mod extensions;
pub mod model;
pub mod permission;
pub mod retry;
pub mod session;
pub mod skills;
pub mod subagent;
pub mod system_prompt;

pub use config::{
    create_model_resolver_from_config, load_default_model_selection, load_theme_setting,
    preferred_models_path, upsert_models_json_entry, write_models_template_if_absent,
    NewModelEntry,
};
pub use extensions::{
    bridge_elicitation, connect_stdio, create_diagnostics_sink, create_extension_event_bus,
    spawn_diagnostics_printer, Extension, ExtensionActions, ExtensionApi, ExtensionDiagnostic,
    ExtensionEvent, ExtensionEventBus, ExtensionHooks, ExtensionRegistry, ExtensionUi,
    McpConnection, McpServerSpec, NoopUi,
};
pub use model::{create_model_resolver, default_model_for, ModelResolver};
pub use permission::{
    classify_tool, mode_baseline_tools, mode_section, normalize_command, policy_for_mode,
    ApprovalDecision, ApprovalHooks, ApprovalKey, ApprovalReason, ApprovalRequest, ApprovalUi,
    ApprovalRules, HeadlessApproval, HeadlessApprovalUi, PermissionEngine, SandboxConfig,
    SandboxPolicy, SessionMode, ToolRiskClass, Verdict,
};
pub use retry::{create_retrying_provider, RetryHooks};
pub use session::{
    create_agent_session, create_session_retry_hooks, AgentSession, AgentSessionConfig,
    AgentSessionEvent, ContextCompactor, CoreError, PromptOutcome, SessionSharedSubscriber,
    SessionSink, SessionSubscriber, create_session_persistence_subscriber,
};
pub use skills::{discover_skill_defs, parse_skill_def, LoadSkillDeps, LoadSkillTool, SkillDef};
pub use subagent::{
    discover_agent_defs, run_child, AgentDef, ChildOutcome, ChildSpec, RunGuard, RunStatus,
    ChildStore, ChildStoreFactory, SubagentDeps, SubagentRegistry, SubagentSessionFactory,
    SubagentTool, DEFAULT_TIMEOUT_MS, MAX_ACTIVE_ASYNC, MAX_OUTPUT_CHARS, MAX_RUN_HISTORY,
    MAX_SYNC_CONCURRENCY, TOOL_NAME,
};
pub use system_prompt::{
    build_system_prompt_sections, build_system_prompt_state, sections_to_text,
    split_prompt_and_rules, SystemPromptOptions, SystemPromptSections, SystemPromptState,
};
