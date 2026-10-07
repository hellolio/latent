//! latent-gateway(L3 聊天网关引擎):常驻 daemon 拥有所有聊天渠道连接与
//! 所有 agent 会话,暴露 WebSocket 控制面。行为语义对齐 OpenClaw 上游
//! (见 GATEWAY_PLAN §4)。
//!
//! 依赖 L2 装配层(latent-runtime)与 L1 纯聊天域(latent-channel);
//! 具体渠道类型只在 `channels::create_channel` 出现(feature 门控)。

pub mod agents;
pub mod approval;
pub mod auto_reply;
pub mod channels;
pub mod config;
pub mod control;
pub mod daemon;
pub mod pairing;
pub mod routing;
pub mod state;
