//! `chat.send` / `chat.abort` / `chat.history`。
//!
//! **chat.send 的 sender 身份由服务端固定为 operator(params 无 sender
//! 字段)**:不匹配 `ownerAllowFrom`、不受 dmPolicy 放行 —— 控制面调用者
//! 不能伪造渠道身份冒充 owner 应答审批/切 full-access(§5.4)。

use serde_json::{json, Value};
use std::sync::Arc;

use crate::auto_reply::Gateway;

/// 投递一条消息 —— **与渠道入站走同一 dispatch 管线**(sessionKey 直指
/// 目标会话;带幂等键,进程内 Map + 过期)。
pub async fn send(gateway: &Arc<Gateway>, params: &Value) -> Result<Value, String> {
    let session_key = params
        .get("sessionKey")
        .and_then(Value::as_str)
        .ok_or("缺少 sessionKey")?
        .to_string();
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or("缺少 text")?
        .to_string();
    // params 携带 sender 字段直接拒绝(身份由服务端固定,§5.4)
    if params.get("sender").is_some() {
        return Err("chat.send 不接受 sender 字段(身份由服务端固定为 operator)".into());
    }
    let idempotency_key = params
        .get("idempotencyKey")
        .and_then(Value::as_str)
        .map(str::to_string);
    gateway
        .inject_operator_message(session_key, text, idempotency_key)
        .await
        .map(|state| json!({ "state": state }))
}

/// 中止指定会话活跃 run(带 runId 则精确取消 —— MVP 会话级取消)。
pub async fn abort(gateway: &Arc<Gateway>, params: &Value) -> Result<Value, String> {
    let session_key = params
        .get("sessionKey")
        .and_then(Value::as_str)
        .ok_or("缺少 sessionKey")?;
    let session = gateway.registry.get(session_key).await;
    match session {
        Some(session) => {
            session.built.session.abort();
            Ok(json!({ "aborted": true }))
        }
        None => Err(format!("会话不存在: {session_key}")),
    }
}

/// 拉指定会话最近消息(转录尾部)。
pub async fn history(gateway: &Arc<Gateway>, params: &Value) -> Result<Value, String> {
    let session_key = params
        .get("sessionKey")
        .and_then(Value::as_str)
        .ok_or("缺少 sessionKey")?;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 500) as usize;
    let session = gateway
        .registry
        .get(session_key)
        .await
        .ok_or_else(|| format!("会话不存在: {session_key}"))?;
    let Some(manager) = &session.built.session_manager else {
        return Ok(json!({ "messages": [] }));
    };
    let entries = manager.branch_entries();
    let messages: Vec<Value> = entries
        .iter()
        .rev()
        .filter_map(|entry| match entry {
            latent_runtime::facade::Entry::Message { id, message, .. } => {
                serde_json::to_value(message).ok().map(|value| {
                    json!({ "id": id, "message": value })
                })
            }
            _ => None,
        })
        .take(limit)
        .collect();
    Ok(json!({ "messages": messages }))
}
