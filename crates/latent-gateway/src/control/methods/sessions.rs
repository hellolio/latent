//! `sessions.list` / `sessions.reset`。

use serde_json::{json, Value};
use std::sync::Arc;

use crate::auto_reply::Gateway;

/// 列出活跃会话(session key + 模型 + 消息数)。
pub async fn list(gateway: &Arc<Gateway>) -> Result<Value, String> {
    let mut sessions = Vec::new();
    for key in gateway.registry.keys().await {
        let detail = match gateway.registry.get(&key).await {
            Some(session) => {
                let snapshot = session.built.session.agent().state_snapshot();
                json!({
                    "key": key,
                    "messageCount": snapshot.message_count,
                    "isStreaming": snapshot.is_streaming,
                    "model": snapshot.model.as_ref().map(|m| m.id.clone()),
                })
            }
            None => json!({ "key": key }),
        };
        sessions.push(detail);
    }
    Ok(json!({ "sessions": sessions }))
}

/// 重置(= /new):abort 后台运行 + 新建会话文件。
pub async fn reset(gateway: &Arc<Gateway>, params: &Value) -> Result<Value, String> {
    let session_key = params
        .get("sessionKey")
        .and_then(Value::as_str)
        .ok_or("缺少 sessionKey")?;
    let Some(session) = gateway.registry.get(session_key).await else {
        return Err(format!("会话不存在: {session_key}"));
    };
    if let Some(registry) = &session.built.subagent_registry {
        registry.abort_all();
    }
    session.built.session.abort();
    session.built.session.wait_idle().await;
    match latent_runtime::assembly::switch_new_session(
        &session.built.session,
        &session.built.manager_holder,
        None,
    )
    .await
    {
        Ok(path) => {
            if let Some(path) = path {
                gateway
                    .state
                    .set_session_file(session_key, &path, crate::state::now_ms());
                let _ = gateway.state.save();
            }
            gateway
                .rebuild_session(session_key)
                .await
                .map(|_| json!({ "reset": true }))
        }
        Err(error) => Err(format!("重置失败: {error}")),
    }
}
