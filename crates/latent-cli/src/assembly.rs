//! 装配逻辑已下沉 `latent-runtime`(L2 装配层,与聊天网关共享);本模块
//! 仅 re-export 保持既有 `latent::assembly::…` 引用路径兼容。

pub use latent_runtime::assembly::*;
