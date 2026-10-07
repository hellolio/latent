//! 控制面薄客户端(`latent-gateway pairing` 等子命令与集成测试共用):
//! connect → 请求 → 读响应。token 读 `$LATENT_GATEWAY_TOKEN` /
//! gateway.json(daemon 单一写者落 state.json,禁止 CLI 直写)。

use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use futures::{SinkExt, StreamExt};

pub struct ControlClient {
    sink: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    stream: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    next_id: u64,
}

/// 默认控制面地址(与 gateway.json 默认一致)。
pub const DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:18789/ws";

impl ControlClient {
    /// 连接 + 握手(connect 首帧;token 校验失败 = Err)。
    pub async fn connect(endpoint: &str, token: &str) -> Result<Self, String> {
        let (stream, _) = tokio_tungstenite::connect_async(endpoint)
            .await
            .map_err(|error| format!("控制面连接失败 {endpoint}: {error}"))?;
        let (mut sink, mut stream) = stream.split();
        let connect = json!({
            "type": "req",
            "id": 1,
            "method": "connect",
            "params": {
                "client": { "id": "latent-gateway-cli", "version": env!("CARGO_PKG_VERSION") },
                "role": "operator",
                "scopes": [],
                "auth": { "token": token },
            },
        });
        sink.send(Message::text(connect.to_string()))
            .await
            .map_err(|error| format!("connect 发送失败: {error}"))?;
        // 读到 hello-ok 为止
        loop {
            let message = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
                .await
                .map_err(|_| "connect 应答超时".to_string())?
                .ok_or("连接被服务端关闭")?
                .map_err(|error| format!("connect 应答读取失败: {error}"))?;
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Close(_) => return Err("连接被服务端关闭(认证失败?)".into()),
                _ => continue,
            };
            let value: Value =
                serde_json::from_str(&text).map_err(|error| format!("帧解析失败: {error}"))?;
            if value.get("type").and_then(Value::as_str) == Some("hello-ok") {
                return Ok(ControlClient {
                    sink,
                    stream,
                    next_id: 2,
                });
            }
            if value.get("ok").and_then(Value::as_bool) == Some(false) {
                return Err(format!(
                    "connect 被拒: {}",
                    value.get("error").and_then(Value::as_str).unwrap_or("?")
                ));
            }
        }
    }

    /// 单次请求(请求/响应按 id 关联)。
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({ "type": "req", "id": id, "method": method, "params": params });
        self.sink
            .send(Message::text(frame.to_string()))
            .await
            .map_err(|error| format!("请求发送失败: {error}"))?;
        loop {
            let message = tokio::time::timeout(std::time::Duration::from_secs(30), self.stream.next())
                .await
                .map_err(|_| "请求应答超时".to_string())?
                .ok_or("连接被服务端关闭")?
                .map_err(|error| format!("应答读取失败: {error}"))?;
            let text = match message {
                Message::Text(text) => text.to_string(),
                _ => continue,
            };
            let value: Value =
                serde_json::from_str(&text).map_err(|error| format!("帧解析失败: {error}"))?;
            if value.get("id").and_then(Value::as_u64) == Some(id)
                && value.get("type").and_then(Value::as_str) == Some("res")
            {
                return if value.get("ok").and_then(Value::as_bool) == Some(true) {
                    Ok(value.get("payload").cloned().unwrap_or(Value::Null))
                } else {
                    Err(value
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("未知错误")
                        .to_string())
                };
            }
            // 事件帧忽略(薄客户端不订阅)
        }
    }
}

/// 解析控制面 token:显式参数 > `$LATENT_GATEWAY_TOKEN`。
pub fn resolve_client_token(explicit: Option<&str>) -> Result<String, String> {
    if let Some(token) = explicit {
        if !token.trim().is_empty() {
            return Ok(token.to_string());
        }
    }
    std::env::var("LATENT_GATEWAY_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "缺少控制面 token(设 $LATENT_GATEWAY_TOKEN 或 gateway.json 的 gateway.auth.token)".into()
        })
}
