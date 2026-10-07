//! latent-cli 库面:装配逻辑、四种模式与 mock MCP 扩展供 E2E 测试复用
//! (bin 仍在 main.rs)。
//!
//! 装配层与斜杠解析等公共面已下沉 `latent-runtime`(L2 装配层,与聊天网关
//! 共享);此处 re-export 保持既有 `latent::assembly::…` 路径兼容,E2E 与
//! 测试不破。

pub mod assembly;
pub mod mcp_mock;
pub mod modes;
