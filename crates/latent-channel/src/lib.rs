//! latent-channel(L1 纯聊天域):跨渠道统一消息模型 + 渠道插件接缝 +
//! 宿主层机制(防抖/分段/@判定/typing)+ 渠道适配器(feature 门控)。
//!
//! **零内部依赖**(不 import 任何 latent-* crate)—— 这条是硬规则:渠道
//! crate 之间互不可见,业务栈也看不到具体渠道,全部经 `ChannelPlugin` trait
//! 交互。行为语义对齐 OpenClaw 上游(见 GATEWAY_PLAN §2/§6)。

pub mod chunk;
pub mod credential;
pub mod debounce;
pub mod error;
pub mod mention_gating;
pub mod plugin;
pub mod typing;
pub mod types;

#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "qq")]
pub mod qq;
#[cfg(feature = "telegram")]
pub mod telegram;
#[cfg(feature = "wecom")]
pub mod wecom;
