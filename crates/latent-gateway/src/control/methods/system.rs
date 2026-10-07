//! `health` / `status` / `channels.status` / `config.get`(redact 凭据)。

use serde_json::{json, Value};
use std::sync::Arc;

use crate::auto_reply::Gateway;

/// 存活探针。
pub async fn health(gateway: &Arc<Gateway>) -> Result<Value, String> {
    Ok(json!({
        "ok": true,
        "uptimeMs": gateway.started_at.elapsed().as_millis() as u64,
    }))
}

/// 运行态(uptime、渠道状态、会话数)。
pub async fn status(gateway: &Arc<Gateway>) -> Result<Value, String> {
    let channels = gateway.channels.status_snapshot().await;
    let channel_values: Vec<Value> = channels
        .iter()
        .map(|(id, status, account)| {
            json!({
                "id": id,
                "status": status,
                "accountId": account,
            })
        })
        .collect();
    Ok(json!({
        "uptimeMs": gateway.started_at.elapsed().as_millis() as u64,
        "channels": channel_values,
        "sessions": gateway.registry.len().await,
        "pendingApprovals": gateway.approval.pending_ids().await,
    }))
}

/// 渠道连接状态(`--probe` 深探活:逐渠道问 handle.status())。
pub async fn channels_status(gateway: &Arc<Gateway>, params: &Value) -> Result<Value, String> {
    let probe = params
        .get("probe")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let snapshot = gateway.channels.status_snapshot().await;
    let mut channels = Vec::new();
    for (id, status, account) in snapshot {
        let mut entry = json!({ "id": id, "status": status, "accountId": account });
        if probe {
            if let Some(handle) = gateway.channels.handle(&id).await {
                let deep = handle.status().await;
                entry["probe"] = serde_json::to_value(&deep).unwrap_or(Value::Null);
            }
        }
        channels.push(entry);
    }
    Ok(json!({ "channels": channels }))
}

/// 读当前生效配置(redact 凭据:token/secret/token 类字段值替换)。
pub async fn config_get(gateway: &Arc<Gateway>) -> Result<Value, String> {
    let value = serde_json::to_value(&gateway.config).map_err(|e| e.to_string())?;
    Ok(redact(value))
}

const REDACT_KEYS: &[&str] = &["token", "secret", "accessToken", "botToken", "apiKey"];

fn redact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    if REDACT_KEYS.contains(&key.as_str()) && value.is_string() {
                        (key, Value::String("[redacted]".into()))
                    } else {
                        (key, redact(value))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        other => other,
    }
}
