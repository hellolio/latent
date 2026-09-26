//! AgentMessage(01 文档 §1/§1.5):agent 侧消息联合,serde 形态与 pi JSONL 兼容。
//!
//! 封闭 enum + `Custom` 逃生口(09 B2);System 携带 sections/toolsAdded/toolsRemoved
//! 控制面(declareToolChanges 依赖),四种自定义消息由 `convert_to_llm` 折叠为 LLM 消息。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use rpi_ai::{AssistantMessage, ContentBlock, Tool, ToolReference, Usage};

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
    System {
        /// 首条 = 基础 prompt;后续 = 追加指令(01 文档 §1.1)
        #[serde(default)]
        content: String,
        /// 命名节 patch:字符串 = 替换,None = 删除
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        sections: BTreeMap<String, Option<String>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools_added: Vec<Tool>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools_removed: Vec<ToolReference>,
        #[serde(default)]
        timestamp: i64,
    },
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

    pub fn system(text: impl Into<String>) -> Self {
        AgentMessage::System {
            content: text.into(),
            sections: BTreeMap::new(),
            tools_added: Vec::new(),
            tools_removed: Vec::new(),
            timestamp: now_ms(),
        }
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
            AgentMessage::system("sys"),
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

    /// system 消息的控制面字段(toolsAdded/toolsRemoved/sections)roundtrip。
    #[test]
    fn system_message_control_plane_roundtrip() {
        let mut sections = BTreeMap::new();
        sections.insert("style".to_string(), Some("terse".to_string()));
        let message = AgentMessage::System {
            content: String::new(),
            sections,
            tools_added: vec![Tool::new("read", "d", serde_json::json!({"type": "object"}))],
            tools_removed: vec![ToolReference { name: "bash".into() }],
            timestamp: 7,
        };
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "system");
        assert_eq!(value["toolsAdded"][0]["name"], "read");
        assert_eq!(value["toolsRemoved"][0]["name"], "bash");
        let back: AgentMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back, message);
    }
}
