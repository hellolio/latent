//! latent-runtime(L2 装配层):业务核的共享装配点与模式无关的解析面。
//!
//! 依赖 L1/L2 的全部 latent crate,不依赖 latent-tui / latent-cli / 聊天栈
//! (latent-channel / latent-gateway)—— 依赖方向见 AGENTS.md,严格单向。
//! 四种运行模式(print/json/rpc/interactive)与聊天网关(latent-gateway)
//! 都经 `assembly::build_session` 装配出各自的 `AgentSession`。

pub mod assembly;
pub mod bootstrap;
pub mod events;
pub mod slash;

/// 聊天栈统一依赖面(P1-14):latent-gateway 按计划只依赖
/// latent-runtime + latent-channel,所需跨层类型经此处 re-export
/// (照 latent-cli 的路径兼容层做法,保持"装配类型只从 runtime 出"
/// 的层纪律)。
pub mod facade {
    pub use latent_ai::{Model, Provider, ThinkingLevel};
    pub use latent_agent::{AgentEvent, AgentMessage, MessageDeltaPayload};
    pub use latent_core::{
        ApprovalDecision, ApprovalReason, ApprovalRequest, ApprovalUi, AgentSession,
        AgentSessionEvent, McpServerSpec, NoopUi, SessionMode, SessionSharedSubscriber,
        SessionSubscriber, ToolRiskClass, create_model_resolver_from_config, latent_dir,
    };
    pub use latent_session::Entry;
}
