//! `pairing.list` / `pairing.approve`(daemon 单一写者落 state.json;
//! `latent-gateway pairing` 子命令的薄客户端目标,§4.10)。

use serde_json::{json, Value};
use std::sync::Arc;

use crate::auto_reply::Gateway;
use crate::state::now_ms;

/// 列出未决配对请求。
pub async fn list(gateway: &Arc<Gateway>) -> Result<Value, String> {
    let pending = gateway.pairing.list_pending();
    let mut entries = Vec::new();
    for (channel, requests) in pending {
        for request in requests {
            entries.push(json!({
                "channel": channel,
                "code": request.code,
                "userId": request.user_id,
                "expiresAt": request.expires_at,
                "expired": request.expires_at <= now_ms(),
            }));
        }
    }
    Ok(json!({ "pending": entries }))
}

/// 批准配对(只授 DM 访问,永不授群访问)。
pub async fn approve(gateway: &Arc<Gateway>, params: &Value) -> Result<Value, String> {
    let channel = params
        .get("channel")
        .and_then(Value::as_str)
        .ok_or("缺少 channel")?;
    let code = params
        .get("code")
        .and_then(Value::as_str)
        .ok_or("缺少 code")?;
    let user_id = gateway.pairing.approve(channel, code, now_ms())?;
    Ok(json!({ "approved": true, "channel": channel, "userId": user_id }))
}
