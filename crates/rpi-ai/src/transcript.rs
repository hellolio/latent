//! 转录折叠与重放(pi 的 utils/transcript.ts,02 文档 §1.1)。
//!
//! `normalize_context` 是 `TranscriptContext` 的唯一常规构造入口:提示词与工具
//! 折叠为首条 system 消息,provider 代码只见转录,不见散装字段。

use crate::types::{Context, Message, Tool, ToolReference, TranscriptContext};
use std::collections::BTreeMap;

/// 为提示词与工具集构造首条 system 消息;两者皆空时返回 None(空转录保持为空)。
pub fn create_initial_system_message(
    system_prompt: Option<&str>,
    tools: &[Tool],
) -> Option<Message> {
    let has_prompt = system_prompt.map(|p| !p.is_empty()).unwrap_or(false);
    if !has_prompt && tools.is_empty() {
        return None;
    }
    Some(Message::System {
        content: system_prompt.unwrap_or("").to_string(),
        sections: Default::default(),
        tools_added: tools.to_vec(),
        tools_removed: Vec::new(),
        timestamp: 0,
    })
}

/// 把 `Context.system_prompt` 与 `Context.tools` 折叠成首条 system 消息。
pub fn normalize_context(context: Context) -> TranscriptContext {
    let initial = create_initial_system_message(context.system_prompt.as_deref(), &context.tools);
    let mut messages = Vec::with_capacity(context.messages.len() + 1);
    if let Some(initial) = initial {
        messages.push(initial);
    }
    messages.extend(context.messages);
    TranscriptContext { messages }
}

fn as_system(message: &Message) -> Option<&Message> {
    match message {
        Message::System { .. } => Some(message),
        _ => None,
    }
}

/// 首条 system 消息(若转录以它开头)。
pub fn get_initial_system_message(messages: &[Message]) -> Option<&Message> {
    messages.first().and_then(as_system)
}

/// 去掉首条 system 消息(用于把提示词放在消息列表之外的 API)。
pub fn without_initial_system_message(messages: &[Message]) -> Vec<Message> {
    if get_initial_system_message(messages).is_some() {
        messages[1..].to_vec()
    } else {
        messages.to_vec()
    }
}

/// 按顺序重放全部 system 消息后可用的工具集(tools_removed 先删、tools_added 后加)。
pub fn get_current_tools(messages: &[Message]) -> Vec<Tool> {
    // 保持首次声明序(pi 的 Map 插入序):重声明原位替换,移除即删除
    let mut tools: Vec<Tool> = Vec::new();
    for message in messages {
        if let Message::System {
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

/// 系统消息的完整提示词文本:content + 各命名节(02 文档;pi 的 getSystemMessageText)。
pub fn get_system_message_text(message: &Message) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Message::System {
        content, sections, ..
    } = message
    {
        if !content.is_empty() {
            parts.push(content.clone());
        }
        for text in sections.values().flatten() {
            if !text.is_empty() {
                parts.push(text.clone());
            }
        }
    }
    parts.join("\n\n")
}

/// 把中途 system 消息渲染为就地的更新文本(pi 的 renderSystemMessageUpdate;
/// 请求期分帧,仅对支持中途 system 的适配器使用)。
pub fn render_system_message_update(message: &Message) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Message::System {
        content, sections, ..
    } = message
    {
        if !content.is_empty() {
            parts.push(content.clone());
        }
        // BTreeMap 保证稳定顺序(JSON 对象无序;01 文档提醒避免整数型名字)
        for (name, value) in sections {
            match value {
                Some(text) => parts.push(format!(
                    "Updated system prompt section \"{name}\":\n\n{text}"
                )),
                None => parts.push(format!("Removed system prompt section \"{name}\".")),
            }
        }
    }
    parts.join("\n\n")
}

/// 重放全部 system 消息为一条首条 system 消息(当前提示词 + 当前工具集)。
pub fn get_current_system_message(messages: &[Message]) -> Option<Message> {
    let mut content: Vec<String> = Vec::new();
    let mut sections: std::collections::BTreeMap<String, Option<String>> = Default::default();
    let mut timestamp: Option<i64> = None;
    for message in messages {
        if let Message::System {
            content: text,
            sections: patch,
            timestamp: ts,
            ..
        } = message
        {
            timestamp = timestamp.or(Some(*ts));
            if !text.is_empty() {
                content.push(text.clone());
            }
            for (name, value) in patch {
                if value.is_none() {
                    sections.remove(name);
                } else {
                    sections.insert(name.clone(), value.clone());
                }
            }
        }
    }
    let tools = get_current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    let joined = content.join("\n\n");
    let sections: BTreeMap<String, Option<String>> = sections;
    Some(Message::System {
        content: joined,
        sections,
        tools_added: tools,
        tools_removed: Vec::new(),
        timestamp: timestamp.unwrap_or(0),
    })
}

/// 重放后的当前系统提示词文本。
pub fn get_current_system_prompt(messages: &[Message]) -> String {
    get_current_system_message(messages)
        .map(|m| get_system_message_text(&m))
        .unwrap_or_default()
}

/// 面向不支持中途 system 消息的 API:重放的 system 消息打头,后续 system 全部丢弃。
pub fn collapse_system_messages(context: TranscriptContext) -> TranscriptContext {
    let head = get_current_system_message(&context.messages);
    let messages: Vec<Message> = context
        .messages
        .into_iter()
        .filter(|m| !matches!(m, Message::System { .. }))
        .collect();
    let mut out = Vec::with_capacity(messages.len() + 1);
    if let Some(head) = head {
        out.push(head);
    }
    out.extend(messages);
    TranscriptContext { messages: out }
}

/// 模型接受中途 system 消息时原样保留,否则折叠(02 文档 §1.1)。
pub fn resolve_transcript(
    context: TranscriptContext,
    supports_mid_convo_system_messages: bool,
) -> TranscriptContext {
    if supports_mid_convo_system_messages {
        context
    } else {
        collapse_system_messages(context)
    }
}

/// 工具引用的便捷构造。
pub fn tool_reference(name: impl Into<String>) -> ToolReference {
    ToolReference { name: name.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Tool;

    fn tool(name: &str) -> Tool {
        Tool::new(
            name,
            format!("{name} desc"),
            serde_json::json!({"type": "object"}),
        )
    }

    #[test]
    fn normalize_folds_prompt_and_tools_into_leading_system() {
        let ctx = normalize_context(Context {
            system_prompt: Some("base prompt".into()),
            messages: vec![Message::user_text("hi")],
            tools: vec![tool("read")],
        });
        assert_eq!(ctx.messages.len(), 2);
        match &ctx.messages[0] {
            Message::System {
                content,
                tools_added,
                ..
            } => {
                assert_eq!(content, "base prompt");
                assert_eq!(tools_added.len(), 1);
            }
            other => panic!("expected system message, got {other:?}"),
        }
        // 空输入保持空转录
        let empty = normalize_context(Context::default());
        assert!(empty.messages.is_empty());
    }

    #[test]
    fn collapse_replays_sections_and_tool_changes() {
        let mut sections = std::collections::BTreeMap::new();
        sections.insert("style".to_string(), Some("be terse".to_string()));
        let later = Message::System {
            content: "extra instructions".into(),
            sections,
            tools_added: vec![tool("bash")],
            tools_removed: vec![tool_reference("read")],
            timestamp: 42,
        };
        let ctx = TranscriptContext {
            messages: vec![
                Message::system("base"),
                Message::user_text("q"),
                later,
                Message::user_text("again"),
                Message::system("mid prompt"),
            ],
        };
        let collapsed = collapse_system_messages(ctx);
        assert_eq!(collapsed.messages.len(), 3);
        match &collapsed.messages[0] {
            Message::System {
                content,
                sections,
                tools_added,
                ..
            } => {
                // content 重放按行拼接;sections 落在字段里由 get_system_message_text 渲染
                assert!(content.contains("base"));
                assert_eq!(sections.get("style"), Some(&Some("be terse".into())));
                assert_eq!(tools_added.len(), 1);
                assert_eq!(tools_added[0].name, "bash");
            }
            other => panic!("expected system message, got {other:?}"),
        }
        // 当前工具集:read 被移除,bash 保留
        let names: Vec<String> = get_current_tools(&collapsed.messages)
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["bash"]);
        // 渲染文本包含中途追加指令
        let text = get_current_system_prompt(&collapsed.messages);
        assert!(text.contains("base prompt") || text.contains("base"));
        assert!(text.contains("extra instructions"));
        assert!(text.contains("be terse"));
    }

    #[test]
    fn resolve_keeps_mid_convo_system_when_supported() {
        let ctx = TranscriptContext {
            messages: vec![
                Message::system("base"),
                Message::user_text("q"),
                Message::system("mid"),
            ],
        };
        let kept = resolve_transcript(ctx.clone(), true);
        assert_eq!(kept.messages.len(), 3);
        let folded = resolve_transcript(ctx, false);
        assert_eq!(folded.messages.len(), 2);
    }
}
