//! 上下文重建三算法(06 文档 §2):沿 leaf→root 回溯 → compaction 虚拟展开 →
//! context_edit 投影。全部为纯函数,输入 entry 序列,输出模型上下文。

use std::collections::BTreeMap;

use rpi_agent::{AgentMessage, AssistantMessage, ContentBlock};

use crate::entry::{ContextReplacement, Entry};

/// 模型引用(设置态投影结果)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

/// 投影后的 entry:原始 append-only entry + 其对模型上下文的贡献。
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedEntry {
    pub source_entry: Entry,
    /// 经 context_edit 投影后的消息;空 = 设置态/剔除/被旧 compaction 忽略
    pub messages: Vec<AgentMessage>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionProjection {
    pub entries: Vec<ProjectedEntry>,
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<ModelRef>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<ModelRef>,
}

/// by_id 索引(Entry id → 序列位置)。
pub(crate) fn build_entry_index(entries: &[Entry]) -> std::collections::HashMap<&str, usize> {
    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.id(), i))
        .collect()
}

/// ① leaf→root 回溯(06 文档 buildSessionPath):leaf 缺省或未命中时取序列末尾
/// (pi 的 `leaf ??= entries[last]`);损坏的 parentId(缺失或成环)截断,不 panic。
pub fn build_session_path<'a>(entries: &'a [Entry], leaf_id: Option<&str>) -> Vec<&'a Entry> {
    let index = build_entry_index(entries);
    let mut current = match leaf_id {
        None => entries.last(),
        Some(id) => index
            .get(id)
            .map(|&i| &entries[i])
            .or_else(|| entries.last()),
    };
    let mut path: Vec<&Entry> = Vec::new();
    // parentId 成环时路径长度不可能超过 entry 总数,以此兜底防死循环
    let mut remaining = entries.len();
    while let Some(entry) = current {
        if remaining == 0 {
            break;
        }
        remaining -= 1;
        path.push(entry);
        current = entry
            .parent_id()
            .and_then(|pid| index.get(pid).map(|&i| &entries[i]));
    }
    path.reverse();
    path
}

/// ② compaction 虚拟展开(06 文档 buildContextEntries):路径上最新的 compaction
/// 以摘要形式占据其原位置,firstKeptEntryId 起的被保留项(剔除其中 system 消息)
/// 前移到它之后;无 compaction 则原样路径。
pub fn build_context_entries<'a>(entries: &'a [Entry], leaf_id: Option<&str>) -> Vec<&'a Entry> {
    let path = build_session_path(entries, leaf_id);
    let Some(compaction) = path
        .iter()
        .rev()
        .find(|e| matches!(e, Entry::Compaction { .. }))
    else {
        return path;
    };
    let compaction_id = compaction.id().to_string();
    let first_kept_entry_id = match compaction {
        Entry::Compaction {
            first_kept_entry_id,
            ..
        } => first_kept_entry_id.clone(),
        _ => return path,
    };

    let compaction_idx = path
        .iter()
        .position(|e| e.id() == compaction_id)
        .expect("compaction in path");
    let mut context_entries: Vec<&Entry> = vec![compaction];
    let mut found_first_kept = false;
    for entry in &path[..compaction_idx] {
        if entry.id() == first_kept_entry_id {
            found_first_kept = true;
        }
        if found_first_kept
            && !matches!(
                entry,
                Entry::Message {
                    message: AgentMessage::System { .. },
                    ..
                }
            )
        {
            context_entries.push(entry);
        }
    }
    context_entries.extend(&path[compaction_idx + 1..]);
    context_entries
}

/// 从路径提取设置态(06 文档 getSessionContextSettings)。
fn get_session_context_settings(path: &[&Entry]) -> (String, Option<ModelRef>) {
    let mut thinking_level = "off".to_string();
    let mut model: Option<ModelRef> = None;
    for entry in path {
        match entry {
            Entry::ThinkingLevelChange {
                thinking_level: level,
                ..
            } => thinking_level = level.clone(),
            Entry::ModelChange {
                provider, model_id, ..
            } => {
                model = Some(ModelRef {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                })
            }
            Entry::Message {
                message: AgentMessage::Assistant(assistant),
                ..
            } => {
                model = Some(ModelRef {
                    provider: assistant.provider.clone(),
                    model_id: assistant.model.clone(),
                })
            }
            _ => {}
        }
    }
    (thinking_level, model)
}

/// 单个 entry 的上下文消息(06 文档 sessionEntryToContextMessages)。
/// 失败 assistant 消息**保留进上下文**(10 文档 §6 默认决策)。
pub fn session_entry_to_context_messages(entry: &Entry) -> Vec<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => vec![message.clone()],
        Entry::CustomMessage {
            custom_type,
            content,
            details,
            display,
            timestamp,
            ..
        } => {
            // pi 的 createCustomMessage:content/details/display 打包进 Custom
            vec![AgentMessage::Custom(rpi_agent::CustomMessage {
                kind: custom_type.clone(),
                data: serde_json::json!({
                    "content": content,
                    "display": display,
                    "details": details,
                    "timestamp": timestamp,
                }),
            })]
        }
        Entry::BranchSummary {
            summary, timestamp, ..
        } => {
            if summary.is_empty() {
                return Vec::new();
            }
            vec![AgentMessage::BranchSummary {
                summary: summary.clone(),
                timestamp: *timestamp,
            }]
        }
        Entry::Compaction {
            summary,
            system_message,
            timestamp,
            ..
        } => {
            let mut messages = Vec::new();
            if let Some(system) = system_message {
                messages.push(system.clone());
            }
            messages.push(AgentMessage::CompactionSummary {
                summary: summary.clone(),
                timestamp: *timestamp,
            });
            messages
        }
        _ => Vec::new(),
    }
}

/// context_edit 投影(06 文档 projectContextEntry):`replacement` 是 edit entry
/// 的替换值 —— None(null)剔除消息;Some 只替换 content(assistant/toolResult
/// 的字符串内容包成 text 块)。仅在 target 上存在 edit entry 时调用。
fn project_context_entry(
    entry: &Entry,
    replacement: Option<&ContextReplacement>,
) -> Vec<AgentMessage> {
    let messages = session_entry_to_context_messages(entry);
    let Some(replacement) = replacement else {
        return Vec::new();
    };
    messages
        .into_iter()
        .map(|message| match message {
            AgentMessage::User { timestamp, .. } => AgentMessage::User {
                content: replacement.content.clone(),
                timestamp,
            },
            AgentMessage::Assistant(assistant) => {
                AgentMessage::Assistant(Box::new(AssistantMessage {
                    content: vec![ContentBlock::text(replacement.content.clone())],
                    ..*assistant
                }))
            }
            AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                usage,
                is_error,
                timestamp,
                ..
            } => AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                content: vec![ContentBlock::text(replacement.content.clone())],
                details: None,
                usage,
                is_error,
                timestamp,
            },
            AgentMessage::Custom(custom) => AgentMessage::Custom(rpi_agent::CustomMessage {
                data: {
                    let mut data = custom.data.clone();
                    if let Some(obj) = data.as_object_mut() {
                        obj.insert(
                            "content".into(),
                            serde_json::Value::String(replacement.content.clone()),
                        );
                    }
                    data
                },
                ..custom
            }),
            other => other,
        })
        .collect()
}

/// ②③ 完整投影(06 文档 buildSessionProjection):只投影 index==0 的 compaction
/// (多个 compaction 时旧的忽略);同时从设置态 entry 提取 thinkingLevel/model。
pub fn build_session_projection(entries: &[Entry], leaf_id: Option<&str>) -> SessionProjection {
    let path = build_session_path(entries, leaf_id);
    let (thinking_level, model) = get_session_context_settings(&path);
    let context_entries = build_context_entries(entries, leaf_id);

    // 路径上的 context_edit 按 targetId 建表;同一 target 后者覆盖(pi 语义)
    let mut edits: BTreeMap<&str, Option<&ContextReplacement>> = BTreeMap::new();
    for entry in &context_entries {
        if let Entry::ContextEdit {
            target_id,
            replacement,
            ..
        } = entry
        {
            edits.insert(target_id.as_str(), replacement.as_ref());
        }
    }

    let projected: Vec<ProjectedEntry> = context_entries
        .iter()
        .enumerate()
        .map(|(index, source_entry)| {
            // Some(_) = 存在 edit entry;内层 None = replacement 为 null(剔除)
            let edit = edits.get(source_entry.id()).copied();
            let messages = if matches!(source_entry, Entry::Compaction { .. }) && index > 0 {
                // 旧 compaction(其原始 id 落在最新保留范围内)不产生消息
                Vec::new()
            } else {
                match edit {
                    None => session_entry_to_context_messages(source_entry),
                    Some(replacement) => project_context_entry(source_entry, replacement),
                }
            };
            ProjectedEntry {
                source_entry: (*source_entry).clone(),
                messages,
            }
        })
        .collect();
    let messages = projected
        .iter()
        .flat_map(|entry| entry.messages.iter().cloned())
        .collect();
    SessionProjection {
        entries: projected,
        messages,
        thinking_level,
        model,
    }
}

/// ③ 终态上下文(06 文档 buildSessionContext)。
pub fn build_session_context(entries: &[Entry], leaf_id: Option<&str>) -> SessionContext {
    let projection = build_session_projection(entries, leaf_id);
    SessionContext {
        messages: projection.messages,
        thinking_level: projection.thinking_level,
        model: projection.model,
    }
}
