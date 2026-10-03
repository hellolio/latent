//! 接缝 #2(宿主 ↔ 循环):`LoopHooks` trait(09 B3 草图)。
//!
//! `convert_to_llm` 是唯一必填方法,其余全部带默认空实现;返回空值/None 是合法
//! 语义,钩子不得 panic(方针文档 §2)。`after_tool_call` 的浅覆盖为**逐字段
//! 显式 Patch**(Some 才覆盖,03 文档 §10.2.6 的语义模糊点由此消除)。
//! steering/follow-up 注入不走钩子:T5 起为 mpsc 推送通道(03 §10.5),
//! 轮询制 `steering_messages`/`follow_up_messages` 已移除(接缝变更见
//! policy §3 登记表)。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rpi_ai::{Model, ThinkingLevel};

use crate::message::AgentMessage;
use crate::tool::ToolExecution;

/// turn 收尾上下文(assistant 消息 + 全部工具结果 + 本次 run 新增消息)。
#[derive(Debug, Clone)]
pub struct TurnCtx {
    pub message: Box<rpi_ai::AssistantMessage>,
    pub tool_results: Vec<AgentMessage>,
    pub new_messages: Vec<AgentMessage>,
}

/// finishTurn 的决策(pi 的 {action:"continue"|"end"})。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDecision {
    Continue,
    End,
}

/// prepareRequest 可替换的字段(消息变换走 transform_context)。
#[derive(Debug, Clone, Default)]
pub struct RequestUpdate {
    pub model: Option<Model>,
    pub thinking_level: Option<Option<ThinkingLevel>>,
}

/// prepareNextTurn 可追加的消息与可替换的字段。
#[derive(Debug, Clone, Default)]
pub struct TurnUpdate {
    pub messages: Option<Vec<AgentMessage>>,
    pub model: Option<Model>,
    pub thinking_level: Option<Option<ThinkingLevel>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallCtx {
    pub tool_call_id: String,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultCtx {
    pub tool_call_id: String,
    pub name: String,
    pub output: String,
    pub details: serde_json::Value,
    pub is_error: bool,
    pub terminate: bool,
}

/// beforeToolCall 的干预决策。两用(07 §8.6 接缝修改):
/// - `block = true`:拦截执行,`reason` 进错误 tool result,`terminate` 参与
///   "批全部 terminate 提前结束"(03 §5.5);
/// - `block = false` 且 `args = Some(..)`:改参后继续执行(对齐 pi 原地改
///   `event.input`;字段缺省 None = 不改)。`args` 为 wire 形态,扩展经 MCP
///   返回同名结构(policy §3 变更登记)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolBlock {
    #[serde(default)]
    pub block: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

/// afterToolCall 的逐字段浅覆盖(无深合并;None = 保持原值)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl ToolPatch {
    /// 是否含任何覆盖字段(扩展聚合后判断"是否有干预")。
    pub fn is_empty(&self) -> bool {
        self.output.is_none()
            && self.details.is_none()
            && self.is_error.is_none()
            && self.terminate.is_none()
    }
}

#[async_trait]
pub trait LoopHooks: Send + Sync {
    /// 必填:AgentMessage 世界 → LLM 世界唯一桥(pi 的 convertToLlm,01 文档)。
    /// 契约:不得 panic;不可转换的消息直接过滤。
    fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message>;

    /// 每次请求前对 AgentMessage 级转录做变换(剪枝/注入),不得 panic。
    async fn transform_context(&self, msgs: Vec<AgentMessage>) -> Vec<AgentMessage> {
        msgs
    }

    /// 按 provider 动态取 key(OAuth 过期 token);不得 panic。
    async fn get_api_key(&self, _provider: &str) -> Option<String> {
        None
    }

    /// 每次请求前(含第一次)调用;可替换 model/thinkingLevel。
    async fn prepare_request(
        &self,
        _model: &Model,
        _thinking: Option<ThinkingLevel>,
    ) -> Option<RequestUpdate> {
        None
    }

    /// turn 结束、下一 turn 前;可追加 messages(如 compaction)。
    async fn prepare_next_turn(&self, _ctx: TurnCtx) -> Option<TurnUpdate> {
        None
    }

    /// turn 结束、turn_end 事件前;可返回 continue/end 决策。
    async fn finish_turn(&self, _ctx: TurnCtx) -> Option<TurnDecision> {
        None
    }

    /// 参数校验后调用;Some(block) 拦截执行。
    async fn before_tool_call(&self, _ctx: ToolCallCtx) -> Option<ToolBlock> {
        None
    }

    /// 工具结算前调用;逐字段浅覆盖结果。
    async fn after_tool_call(&self, _ctx: ToolResultCtx) -> Option<ToolPatch> {
        None
    }

    /// 批执行默认模式(逐工具可用 `Tool::execution_mode` 覆盖)。
    fn tool_execution(&self) -> ToolExecution {
        ToolExecution::Parallel
    }
}

/// 默认 hooks:AgentMessage → LLM 消息的折叠(`Custom` 不进上下文)。
/// core/模式在其上覆盖需要的钩子;cli --mock 也直接用它跑通全链路。
pub struct PassthroughHooks;

#[async_trait]
impl LoopHooks for PassthroughHooks {
    fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
        use rpi_ai::Message;
        msgs.iter()
            .filter_map(|msg| match msg {
                AgentMessage::User { content, timestamp } => Some(Message::User {
                    content: rpi_ai::UserContent::Text(content.clone()),
                    timestamp: *timestamp,
                }),
                AgentMessage::Assistant(assistant) => Some(Message::Assistant(assistant.clone())),
                AgentMessage::ToolResult {
                    tool_call_id,
                    tool_name,
                    content,
                    details,
                    usage: _,
                    is_error,
                    timestamp,
                } => Some(Message::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    content: content.clone(),
                    details: details.clone(),
                    is_error: *is_error,
                    timestamp: *timestamp,
                }),
                AgentMessage::BashExecution {
                    command,
                    output,
                    exit_code,
                    timestamp: _,
                } => {
                    let exit = exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into());
                    Some(Message::user_text(format!(
                        "<bash_execution command=\"{}\" exit_code=\"{}\">\n{}\n</bash_execution>",
                        command.replace('"', "&quot;"),
                        exit,
                        output
                    )))
                }
                AgentMessage::BranchSummary {
                    summary,
                    timestamp: _,
                } => Some(Message::user_text(format!(
                    "<branch_summary>\n{}\n</branch_summary>",
                    summary
                ))),
                AgentMessage::CompactionSummary {
                    summary,
                    timestamp: _,
                } => Some(Message::user_text(format!(
                    "<compaction_summary>\n{}\n</compaction_summary>",
                    summary
                ))),
                AgentMessage::ModeSection { content, timestamp: _ } => {
                    Some(Message::developer(content.clone()))
                }
                AgentMessage::Custom(_) => None,
            })
            .collect()
    }
}
