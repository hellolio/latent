//! 事件与订阅者(09 B2):`Vec<Arc<dyn Subscriber>>` 串行 await、按订阅顺序保序,
//! 不用 broadcast channel(无序且不背压)。
//!
//! 事件集合对齐 pi 的 10 种 AgentEvent(01 文档 §3.3):
//! `message_update` 仅 assistant 流式,携带快照;`tool_execution_end` 并行模式下
//! 按完成序,tool result 消息按源序(03 文档不变量 I4)。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use rpi_ai::AssistantMessage;

use crate::message::AgentMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// run 开始
    AgentStart,
    /// 每个 turn 开始
    TurnStart,
    /// assistant 消息 + 全部工具结果之后
    TurnEnd {
        message: Box<AssistantMessage>,
        tool_results: Vec<AgentMessage>,
    },
    /// system/user/assistant/toolResult 消息进入转录
    MessageStart { message: Box<AgentMessage> },
    /// 流式文本增量(便于 UI 直接追加;完整快照见 MessageUpdate)
    MessageDelta { delta: String },
    /// 仅 assistant 流式:当前 partial 消息快照
    MessageUpdate { message: Box<AssistantMessage> },
    /// 消息定稿(全部消息类型)
    MessageEnd { message: Box<AgentMessage> },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        partial: String,
    },
    /// 工具结算(并行模式下按完成序)
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        output: String,
        is_error: bool,
    },
    /// run 结束(最后事件);messages = 本次 run 新增的全部消息
    AgentEnd { messages: Vec<AgentMessage> },
}

/// 事件汇:core/工具/UI/持久化都是订阅者。实现不得 panic,失败自行记录。
#[async_trait]
pub trait Subscriber: Send + Sync {
    async fn on_event(&self, event: &AgentEvent);
}

pub type SharedSubscriber = Arc<dyn Subscriber>;
