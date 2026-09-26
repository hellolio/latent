//! 工具集声明(03 文档 §6 declareToolChanges,不变量 I6):`context.tools` 是
//! 运行时可执行集;转录 system 消息声明"模型可见集"。每次注入消息前,重放全部
//! system 消息求当前可见集,与可执行集求差,把增量编码为注入消息中的 system 消息
//! —— 重放后模型可见工具集恒等于 `context.tools`。

use std::sync::Arc;

use rpi_ai::{Tool as DeclaredTool, ToolReference};

use crate::message::AgentMessage;
use crate::tool::Tool;

/// 重放 AgentMessage 转录中的全部 system 消息,得到当前声明的工具集
/// (tools_removed 先删、tools_added 后加,保持首次声明序)。
pub fn declared_tools(messages: &[AgentMessage]) -> Vec<DeclaredTool> {
    let mut tools: Vec<DeclaredTool> = Vec::new();
    for message in messages {
        if let AgentMessage::System {
            tools_added,
            tools_removed,
            ..
        } = message
        {
            for removed in tools_removed {
                tools.retain(|tool| tool.name != removed.name);
            }
            for added in tools_added {
                match tools.iter_mut().find(|tool| tool.name == added.name) {
                    Some(existing) => *existing = added.clone(),
                    None => tools.push(added.clone()),
                }
            }
        }
    }
    tools
}

fn to_declaration(tool: &Arc<dyn Tool>) -> DeclaredTool {
    DeclaredTool::new(tool.name(), tool.description(), tool.schema())
}

/// 计算注入消息的工具集增量并编码进 pending 消息(03 文档 §6):
/// - pending 中已有 system 消息 → 其工具字段替换为计算出的增量;
/// - 否则新建 system 消息,插到第一个非 system 的 pending 消息之前;
/// - 无变化则原样返回。
pub fn declare_tool_changes(
    transcript: &[AgentMessage],
    tools: &[Arc<dyn Tool>],
    pending: Vec<AgentMessage>,
) -> Vec<AgentMessage> {
    let desired: Vec<DeclaredTool> = tools.iter().map(to_declaration).collect();
    let current = declared_tools(transcript);

    let mut added: Vec<DeclaredTool> = Vec::new();
    for tool in &desired {
        match current.iter().find(|c| c.name == tool.name) {
            Some(existing) if existing == tool => {}
            _ => added.push(tool.clone()),
        }
    }
    let mut removed: Vec<ToolReference> = current
        .iter()
        .filter(|c| !desired.iter().any(|d| d.name == c.name))
        .map(|c| ToolReference {
            name: c.name.clone(),
        })
        .collect();

    if added.is_empty() && removed.is_empty() {
        return pending;
    }

    let mut out = pending;
    let mut replaced = false;
    for message in out.iter_mut() {
        if let AgentMessage::System {
            tools_added,
            tools_removed,
            ..
        } = message
        {
            if !replaced {
                // 首个 system 消息:工具字段替换为计算出的增量
                *tools_added = std::mem::take(&mut added);
                *tools_removed = std::mem::take(&mut removed);
                replaced = true;
            } else {
                // 其余 system 消息:工具字段清零,避免重放过度应用破坏 I6
                tools_added.clear();
                tools_removed.clear();
            }
        }
    }
    if replaced {
        return out;
    }
    let position = out
        .iter()
        .position(|msg| !matches!(msg, AgentMessage::System { .. }))
        .unwrap_or(out.len());
    let declaration = AgentMessage::System {
        content: String::new(),
        sections: Default::default(),
        tools_added: added,
        tools_removed: removed,
        timestamp: crate::message::now_ms(),
    };
    out.insert(position, declaration);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolCall, ToolError, ToolOutput, ToolUpdater};
    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    fn fake_tool(name: &str) -> Arc<dyn Tool> {
        struct Fake(String);
        #[async_trait]
        impl Tool for Fake {
            fn name(&self) -> &str {
                &self.0
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(
                &self,
                _call: ToolCall,
                _cancel: CancellationToken,
                _updater: &dyn ToolUpdater,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::text("ok"))
            }
        }
        Arc::new(Fake(name.to_string()))
    }

    fn system_with_tools(added: &[&str], removed: &[&str]) -> AgentMessage {
        AgentMessage::System {
            content: String::new(),
            sections: Default::default(),
            tools_added: added
                .iter()
                .map(|n| DeclaredTool::new(*n, "", serde_json::json!({"type": "object"})))
                .collect(),
            tools_removed: removed
                .iter()
                .map(|n| ToolReference {
                    name: n.to_string(),
                })
                .collect(),
            timestamp: 0,
        }
    }

    /// I6:注入后重放,模型可见集恒等于 context.tools。
    #[test]
    fn replay_after_declaration_equals_context_tools() {
        let transcript = vec![system_with_tools(&["read"], &[])];
        let tools = vec![fake_tool("bash"), fake_tool("read")];
        let pending = vec![AgentMessage::user("hi")];
        let injected = declare_tool_changes(&transcript, &tools, pending);
        // 注入消息插在 user 之前
        assert!(matches!(injected[0], AgentMessage::System { .. }));
        assert_eq!(injected.len(), 2);
        // I6:合并 transcript + 注入结果后重放,可见集 == context.tools
        let mut full = transcript.clone();
        full.extend(injected);
        let replay = declared_tools(&full);
        let mut names: Vec<String> = replay.iter().map(|t| t.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["bash", "read"]);
    }

    #[test]
    fn no_change_returns_pending_unchanged() {
        let transcript = vec![system_with_tools(&["read"], &[])];
        let tools = vec![fake_tool("read")];
        let pending = vec![AgentMessage::user("hi")];
        let injected = declare_tool_changes(&transcript, &tools, pending);
        assert_eq!(injected.len(), 1);
        assert!(matches!(&injected[0], AgentMessage::User { .. }));
    }

    #[test]
    fn pending_system_message_fields_are_replaced() {
        let transcript = vec![system_with_tools(&["read"], &[])];
        let tools = vec![fake_tool("bash")];
        let pending = vec![AgentMessage::system("追加指令"), AgentMessage::user("hi")];
        let injected = declare_tool_changes(&transcript, &tools, pending);
        match &injected[0] {
            AgentMessage::System {
                content,
                tools_added,
                tools_removed,
                ..
            } => {
                // 既有 system 消息:content 保留,工具字段替换为增量
                assert_eq!(content, "追加指令");
                assert_eq!(tools_added.len(), 1);
                assert_eq!(tools_added[0].name, "bash");
                assert_eq!(tools_removed.len(), 1);
                assert_eq!(tools_removed[0].name, "read");
            }
            other => panic!("expected system message, got {other:?}"),
        }
        assert_eq!(injected.len(), 2);
    }

    #[test]
    fn changed_declaration_replaces_in_place() {
        // 同名工具 schema 变化 → 视为 added(重放后可见集更新)
        struct V2;
        #[async_trait]
        impl Tool for V2 {
            fn name(&self) -> &str {
                "read"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {"v": {"type": "integer"}}})
            }
            async fn execute(
                &self,
                _call: ToolCall,
                _cancel: CancellationToken,
                _updater: &dyn ToolUpdater,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::text("ok"))
            }
        }
        let transcript = vec![system_with_tools(&["read"], &[])];
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(V2)];
        let injected = declare_tool_changes(&transcript, &tools, vec![AgentMessage::user("hi")]);
        assert!(matches!(injected[0], AgentMessage::System { .. }));
    }
}
