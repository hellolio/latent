//! 控制面方法表(§4.12;命名照抄上游 `core-descriptors.ts`)。
//! 一方法一异步函数,`dispatch` 按名路由。

pub mod chat;
pub mod pairing;
pub mod sessions;
pub mod system;

use std::sync::Arc;

use serde_json::Value;

use crate::auto_reply::Gateway;

/// 方法执行上下文。
pub struct MethodCtx {
    pub gateway: Arc<Gateway>,
    /// 已解析的 auth token(连接握手后可用)
    pub connected: bool,
}

/// 方法分派(未知方法 → Err)。
pub async fn dispatch(gateway: &Arc<Gateway>, method: &str, params: &Value) -> Result<Value, String> {
    match method {
        "health" => system::health(gateway).await,
        "status" => system::status(gateway).await,
        "chat.send" => chat::send(gateway, params).await,
        "chat.abort" => chat::abort(gateway, params).await,
        "chat.history" => chat::history(gateway, params).await,
        "sessions.list" => sessions::list(gateway).await,
        "sessions.reset" => sessions::reset(gateway, params).await,
        "channels.status" => system::channels_status(gateway, params).await,
        "pairing.list" => pairing::list(gateway).await,
        "pairing.approve" => pairing::approve(gateway, params).await,
        "config.get" => system::config_get(gateway).await,
        other => Err(format!("未知方法: {other}")),
    }
}
