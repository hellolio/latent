//! AgentMessage(01 文档 §1/§1.5):agent 侧消息联合,serde 形态与 pi JSONL 兼容。
//!
//! 封闭 enum + `Custom` 逃生口(09 B2)。转录只保留**状态类**消息(用户/助手/
//! 工具结果等);系统提示词与工具 schema 属"能力规则",永远经请求级字段
//! (`Context.system` / `tools`)动态下发,不以消息形式进转录或 session。
//! 四种自定义消息由 `convert_to_llm` 折叠为 LLM 消息。

use serde::{Deserialize, Serialize};

use rpi_ai::{AssistantMessage, ContentBlock, Usage};

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    #[serde(rename_all = "camelCase")]
    User {
        #[serde(default)]
        content: String,
        #[serde(default)]
        timestamp: i64,
    },
    Assistant(Box<AssistantMessage>),
    #[serde(rename = "toolResult", rename_all = "camelCase")]
    ToolResult {
        tool_call_id: String,
        #[serde(default)]
        tool_name: String,
        #[serde(default)]
        content: Vec<ContentBlock>,
        /// 结构化 UI/日志数据,不发给 LLM(01 文档)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        /// 工具自身消耗(如子 LLM 调用),不进主上下文核算
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        timestamp: i64,
    },
    /// `!` 前缀的裸 shell 执行记录(convert_to_llm → user 文本;01 文档 §1.5)
    #[serde(rename_all = "camelCase")]
    BashExecution {
        command: String,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default)]
        timestamp: i64,
    },
    /// 树导航时的分支摘要(convert_to_llm → 带 XML 包裹的 user)
    #[serde(rename_all = "camelCase")]
    BranchSummary {
        summary: String,
        #[serde(default)]
        timestamp: i64,
    },
    /// 压缩摘要(convert_to_llm → 带 XML 包裹的 user)
    #[serde(rename_all = "camelCase")]
    CompactionSummary {
        summary: String,
        #[serde(default)]
        timestamp: i64,
    },
    /// 扩展/自定义消息逃生口;`kind` 做判别,details 边界统一 `serde_json::Value`(09 B5.4)
    Custom(CustomMessage),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub kind: String,
    pub data: serde_json::Value,
}

impl AgentMessage {
    pub fn user(text: impl Into<String>) -> Self {
        AgentMessage::User { content: text.into(), timestamp: now_ms() }
    }

    /// 工具结果消息(文本内容便捷构造)。
    pub fn tool_result_text(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        text: impl Into<String>,
        is_error: bool,
    ) -> Self {
        AgentMessage::ToolResult {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::text(text)],
            details: None,
            usage: None,
            is_error,
            timestamp: now_ms(),
        }
    }

    /// toolResult 消息的拼接文本(UI 展示用)。
    pub fn tool_result_content(&self) -> Option<String> {
        match self {
            AgentMessage::ToolResult { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|block| block.as_text())
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        }
    }

    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            AgentMessage::Assistant(assistant) => Some(assistant),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::ContentBlock;

    /// P0 回归:AgentMessage 的 role 判别符与 pi JSONL 兼容。
    #[test]
    fn agent_message_roundtrip_matches_pi_roles() {
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let mut assistant = rpi_ai::AssistantMessage::pending(&model);
        assistant.content = vec![ContentBlock::text("hello")];
        assistant.stop_reason = rpi_ai::StopReason::Stop;

        let messages = vec![
            AgentMessage::user("q"),
            AgentMessage::Assistant(Box::new(assistant)),
            AgentMessage::tool_result_text("t1", "read", "r", false),
            AgentMessage::BashExecution { command: "ls".into(), output: "out".into(), exit_code: Some(0), timestamp: 0 },
            AgentMessage::BranchSummary { summary: "s".into(), timestamp: 0 },
            AgentMessage::CompactionSummary { summary: "c".into(), timestamp: 0 },
        ];
        for message in messages {
            let value = serde_json::to_value(&message).unwrap();
            let back: AgentMessage = serde_json::from_value(value).unwrap();
            assert_eq!(back, message);
        }
        let assistant_value = serde_json::to_value(AgentMessage::Assistant(Box::new({
            let mut m = rpi_ai::AssistantMessage::pending(&model);
            m.stop_reason = rpi_ai::StopReason::Stop;
            m
        })))
        .unwrap();
        assert_eq!(assistant_value["role"], "assistant");
        assert_eq!(
            serde_json::to_value(AgentMessage::tool_result_text("t", "read", "", false)).unwrap()["role"],
            "toolResult"
        );
    }
}
