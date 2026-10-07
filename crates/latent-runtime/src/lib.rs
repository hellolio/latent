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
