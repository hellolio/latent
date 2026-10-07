//! 四种运行模式(08 文档 §1):interactive/print/json/rpc —— 同一业务核
//! (`AgentSession`)的四种 I/O 壳,模式只订阅事件流 + 提供各自的
//! `ExtensionUi` 实现(接缝 #5)。

pub mod interactive;
pub mod json;
pub mod print_mode;
pub mod rpc;
pub mod slash;

/// json/rpc 共用的事件 → JSON 映射已下沉 `latent-runtime::events`
/// (聊天网关事件面同源);re-export 保持既有引用路径兼容。
pub use latent_runtime::events::session_event_to_json;
