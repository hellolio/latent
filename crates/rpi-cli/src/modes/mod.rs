//! 四种运行模式(08 文档 §1):interactive/print/json/rpc —— 同一业务核
//! (`AgentSession`)的四种 I/O 壳,模式只订阅事件流 + 提供各自的
//! `ExtensionUi` 实现(接缝 #5)。

pub mod interactive;
pub mod json;
pub mod print_mode;
pub mod rpc;
pub mod slash;

use rpi_agent::AgentEvent;
use rpi_core::AgentSessionEvent;
use serde_json::{json, Value};

/// `AgentSessionEvent` → JSON(08 文档 `JsonAgentSessionEvent`):**剥离流式
/// partial**(message delta/update 与工具输出增量不上线),工具开始事件附
/// id/toolName。json 与 rpc 模式共用同一映射(与 TUI 消费同源事件)。
pub fn session_event_to_json(event: &AgentSessionEvent) -> Option<Value> {
    match event {
        AgentSessionEvent::Agent(agent_event) => agent_event_to_json(agent_event),
        AgentSessionEvent::AgentSettled => Some(json!({ "type": "agent_settled" })),
        AgentSessionEvent::QueueUpdate {
            steering,
            follow_up,
        } => Some(json!({
            "type": "queue_update",
            "steering": steering,
            "follow_up": follow_up,
        })),
        AgentSessionEvent::AutoRetryStart {
            attempt,
            delay_ms,
            reason,
        } => Some(json!({
            "type": "auto_retry_start",
            "attempt": attempt,
            "delay_ms": delay_ms,
            "reason": reason,
        })),
        AgentSessionEvent::AutoRetryEnd { success, reason } => Some(json!({
            "type": "auto_retry_end",
            "success": success,
            "reason": reason,
        })),
        AgentSessionEvent::ApprovalRequested { request } => {
            // 审批载荷整体序列化(ApprovalRequest 自带 serde 形态)
            match serde_json::to_value(request) {
                Ok(request) => Some(json!({
                    "type": "approval_requested",
                    "request": request,
                })),
                Err(_) => None,
            }
        }
        AgentSessionEvent::ApprovalResolved {
            tool_call_id,
            decision,
        } => Some(json!({
            "type": "approval_resolved",
            "toolCallId": tool_call_id,
            "decision": decision,
        })),
    }
}

fn agent_event_to_json(event: &AgentEvent) -> Option<Value> {
    match event {
        AgentEvent::AgentStart => Some(json!({ "type": "agent_start" })),
        AgentEvent::TurnStart => Some(json!({ "type": "turn_start" })),
        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => Some(json!({
            "type": "turn_end",
            "message": message,
            "tool_results": tool_results,
        })),
        AgentEvent::MessageStart { message, .. } => {
            Some(json!({ "type": "message_start", "message": message }))
        }
        // 流式 partial:剥离(08 文档 json 模式语义)
        AgentEvent::MessageDelta { .. } | AgentEvent::MessageUpdate { .. } => None,
        AgentEvent::MessageEnd { message } => {
            Some(json!({ "type": "message_end", "message": message }))
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Some(json!({
            "type": "toolcall_start",
            "id": tool_call_id,
            "toolName": tool_name,
            "args": args,
        })),
        // 工具输出增量也是流式 partial:剥离
        AgentEvent::ToolExecutionUpdate { .. } => None,
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            output,
            is_error,
        } => Some(json!({
            "type": "toolcall_end",
            "id": tool_call_id,
            "toolName": tool_name,
            "output": output,
            "isError": is_error,
        })),
        AgentEvent::AgentEnd { messages } => {
            Some(json!({ "type": "agent_end", "messages": messages }))
        }
    }
}
