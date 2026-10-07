//! 控制面事件(上游 `GATEWAY_EVENTS` 的子集,命名照抄):
//! `agent`(run 进度/最终回复)、`chat`(入站回执)、`channels`(状态变化)、
//! `health`、`shutdown`。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum GatewayEvent {
    /// 入站回执
    Chat {
        platform: String,
        chat_type: String,
        conversation_id: String,
        sender: String,
        text: String,
    },
    /// run 进度/最终回复(operator 会话与监控订阅)
    Agent {
        session_key: String,
        kind: String,
        text: String,
    },
    /// 渠道状态变化
    Channels {
        channel: String,
        status: String,
    },
    Health {
        uptime_ms: u64,
    },
    Shutdown,
}
