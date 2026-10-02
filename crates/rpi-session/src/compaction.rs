//! Compaction(06 文档 §3):设置/token 估算/切点算法/对话序列化/摘要模板。
//!
//! 摘要的 LLM 调用经 `Summarizer` trait 注入(本 crate 不依赖 provider 实现,
//! 装配方提供 provider 支撑的实现,测试用 mock)—— 保持"rpi-session 只依赖
//! rpi-agent 类型"的依赖纪律。

use rpi_agent::{AgentMessage, ContentBlock, StopReason, Usage};

use crate::entry::Entry;
use crate::projection::{build_session_projection, SessionProjection};

// ============================================================================
// 设置(06 文档 §3.1)
// ============================================================================

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    pub enabled: bool,
    /// 预留预算:**>= 1.0 按绝对 token 数**(16384 = 16k);
    /// **0 < v < 1.0 按 context_window 的百分比**(0.1 = 10%,随模型窗口缩放;
    /// 100% 无法表达,也不会有意义)。分辨率在 should_compact 调用时进行
    /// (此时 context_window 已知)。serde 用 f64 兼容整数与浮点两种写法。
    pub reserve_tokens: f64,
    pub keep_recent_tokens: u64,
}

pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16_384.0,
    keep_recent_tokens: 20_000,
};

impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS
    }
}

/// reserve_tokens → 实际预留 token 数:`v < 1.0` 按 `context_window` 的百分比
/// 解析(0.1 = 10%),`v >= 1.0` 按绝对 token 数;非正数归零(不预留)。
pub fn reserve_tokens_for_window(reserve_tokens: f64, context_window: u64) -> u64 {
    if !(reserve_tokens > 0.0) {
        return 0;
    }
    if reserve_tokens < 1.0 {
        (context_window as f64 * reserve_tokens).round() as u64
    } else {
        reserve_tokens as u64
    }
}

/// `contextTokens > contextWindow - reserveTokens` 时触发(06 文档 §3.1)。
/// reserveTokens < 1.0 时按窗口百分比解析(见 `reserve_tokens_for_window`)。
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    let reserve = reserve_tokens_for_window(settings.reserve_tokens, context_window);
    context_window.saturating_sub(reserve) < context_tokens
}

// ============================================================================
// token 估算(06 文档 §3.2)
// ============================================================================

/// 真实数据优先:totalTokens,否则四项相加。
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

/// 有效 usage:非 aborted/error、非全零(06 文档 getAssistantUsage)。
fn get_assistant_usage(message: &AgentMessage) -> Option<Usage> {
    let assistant = message.as_assistant()?;
    if matches!(
        assistant.stop_reason,
        StopReason::Aborted | StopReason::Error
    ) {
        return None;
    }
    if calculate_context_tokens(&assistant.usage) == 0 {
        return None;
    }
    Some(assistant.usage)
}

/// 最近一条有效 assistant usage(06 文档 getLastAssistantUsage)。
pub fn get_last_assistant_usage(entries: &[Entry]) -> Option<Usage> {
    entries.iter().rev().find_map(|entry| match entry {
        Entry::Message { message, .. } => get_assistant_usage(message),
        _ => None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    /// 最后一条有效 usage 所在的消息下标;None = 全部走估算
    pub last_usage_index: Option<usize>,
}

/// usage 优先 + 其后消息逐条估算(06 文档 estimateContextTokens)。
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let usage_info = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, message)| get_assistant_usage(message).map(|usage| (i, usage)));
    let Some((index, usage)) = usage_info else {
        let estimated: u64 = messages.iter().map(|m| estimate_tokens(m) as u64).sum();
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };
    let usage_tokens = calculate_context_tokens(&usage);
    let trailing_tokens: u64 = messages[index + 1..]
        .iter()
        .map(|m| estimate_tokens(m) as u64)
        .sum();
    ContextUsageEstimate {
        tokens: usage_tokens + trailing_tokens,
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

/// context_edit/compaction 之后旧 usage 失真 → 放弃 usage,全量估算
/// (06 文档 estimateProjectedContextTokens)。
pub fn estimate_projected_context_tokens(
    projection: &SessionProjection,
    branch_entries: &[Entry],
) -> ContextUsageEstimate {
    let estimate = estimate_context_tokens(&projection.messages);
    let Some(last_usage_index) = estimate.last_usage_index else {
        return estimate;
    };

    // 找到 usage 所在消息的源 entry
    let mut projected_message_index = 0usize;
    let mut usage_entry_id: Option<&str> = None;
    for entry in &projection.entries {
        let next = projected_message_index + entry.messages.len();
        if last_usage_index < next {
            usage_entry_id = Some(entry.source_entry.id());
            break;
        }
        projected_message_index = next;
    }
    let usage_entry_index =
        usage_entry_id.and_then(|id| branch_entries.iter().position(|entry| entry.id() == id));
    let latest_invalidating = branch_entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::ContextEdit { .. } | Entry::Compaction { .. }));
    // pi 语义:无 context_edit/compaction 时 latestInvalidating = -1,usage 恒可信
    let trusted = match (usage_entry_index, latest_invalidating) {
        (Some(usage_index), Some(invalidating)) => usage_index > invalidating,
        (Some(_), None) => true,
        _ => false,
    };
    if trusted {
        return estimate;
    }

    // 全量估算(逐条)
    let tokens: u64 = projection
        .messages
        .iter()
        .map(|m| estimate_tokens(m) as u64)
        .sum();
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

/// image 内容块的估算字符数(06 文档 ESTIMATED_IMAGE_CHARS)。
const ESTIMATED_IMAGE_CHARS: usize = 4800;

fn estimate_content_blocks_chars(blocks: &[ContentBlock]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => text.len(),
            ContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall { .. } => 0,
        })
        .sum()
}

/// chars/4 启发式,保守高估(06 文档 estimateTokens)。
pub fn estimate_tokens(message: &AgentMessage) -> usize {
    let chars = match message {
        AgentMessage::User { content, .. } => content.len(),
        AgentMessage::Assistant(assistant) => {
            let mut chars = 0;
            for block in &assistant.content {
                match block {
                    ContentBlock::Text { text, .. } => chars += text.len(),
                    ContentBlock::Thinking { thinking, .. } => chars += thinking.len(),
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => {
                        chars += name.len()
                            + serde_json::to_string(arguments)
                                .map(|s| s.len())
                                .unwrap_or(0);
                    }
                    ContentBlock::Image { .. } => chars += ESTIMATED_IMAGE_CHARS,
                }
            }
            chars
        }
        AgentMessage::ToolResult { content, .. } => estimate_content_blocks_chars(content),
        AgentMessage::BashExecution {
            command, output, ..
        } => command.len() + output.len(),
        AgentMessage::BranchSummary { summary, .. }
        | AgentMessage::CompactionSummary { summary, .. } => summary.len(),
        AgentMessage::Custom(custom) => custom.kind.len() + custom.data.to_string().len(),
    };
    chars.div_ceil(4)
}

// ============================================================================
// 切点算法(06 文档 §3.3)
// ============================================================================

/// 有效切点消息;**绝不在 toolResult 处切**(06 文档 isCutPointMessage)。
pub fn is_cut_point_message(message: &AgentMessage) -> bool {
    matches!(
        message,
        AgentMessage::User { .. }
            | AgentMessage::Assistant(_)
            | AgentMessage::BashExecution { .. }
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary { .. }
            | AgentMessage::CompactionSummary { .. }
    )
}

fn is_turn_start_message(message: &AgentMessage) -> bool {
    matches!(
        message,
        AgentMessage::User { .. }
            | AgentMessage::BashExecution { .. }
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary { .. }
            | AgentMessage::CompactionSummary { .. }
    )
}

fn is_turn_start_entry(entry: &Entry) -> bool {
    if matches!(entry, Entry::Compaction { .. }) {
        return false;
    }
    session_entry_is(entry, &is_turn_start_message)
}

fn session_entry_is(entry: &Entry, predicate: &dyn Fn(&AgentMessage) -> bool) -> bool {
    crate::projection::session_entry_to_context_messages(entry)
        .iter()
        .any(predicate)
}

/// 有效切点下标(user/assistant 等;compaction 与无上下文消息的 entry 不算)。
fn find_valid_cut_points(entries: &[Entry], start_index: usize, end_index: usize) -> Vec<usize> {
    let mut cut_points = Vec::new();
    for (i, entry) in entries.iter().enumerate().take(end_index).skip(start_index) {
        if matches!(entry, Entry::Compaction { .. }) {
            continue;
        }
        if session_entry_is(entry, &is_cut_point_message) {
            cut_points.push(i);
        }
    }
    cut_points
}

/// 找到包含给定下标的 turn 的起始 user 消息(06 文档 findTurnStartIndex)。
pub fn find_turn_start_index(
    entries: &[Entry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    if entries.is_empty() || start_index >= entries.len() || entry_index < start_index {
        return None;
    }
    let end = entry_index.min(entries.len() - 1);
    entries[start_index..=end]
        .iter()
        .rposition(is_turn_start_entry)
        .map(|offset| start_index + offset)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPointResult {
    /// 第一个保留 entry 的下标
    pub first_kept_entry_index: usize,
    /// 被分割 turn 的起始 user 消息下标(供摘要请求分割);None = 未分割
    pub turn_start_index: Option<usize>,
    pub is_split_turn: bool,
}

/// 切点算法(06 文档 findCutPoint):从最新往回累积估算 token,达
/// keepRecentTokens 停;取不早于当前位置的最近有效切点;再向前吞并相邻的
/// 纯元数据 entry。
pub fn find_cut_point(
    entries: &[Entry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let end_index = end_index.min(entries.len());
    let cut_points = find_valid_cut_points(entries, start_index, end_index);
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens: u64 = 0;
    let mut cut_index = cut_points[0];
    for i in (start_index..end_index).rev() {
        let message_tokens: u64 = crate::projection::session_entry_to_context_messages(&entries[i])
            .iter()
            .map(|m| estimate_tokens(m) as u64)
            .sum();
        if message_tokens == 0 {
            continue;
        }
        accumulated_tokens += message_tokens;
        if accumulated_tokens >= keep_recent_tokens {
            // 优先不早于当前位置的最近有效切点;若尾部 toolResult 自己超预算,
            // 保留其前面的 assistant tool call(而非退回第一条消息)
            cut_index = cut_points
                .iter()
                .copied()
                .find(|&candidate| candidate >= i)
                .unwrap_or_else(|| {
                    *cut_points
                        .last()
                        .expect("cut_points non-empty checked above")
                });
            break;
        }
    }

    // 向前吞并相邻元数据 entry(不产生上下文消息的;compaction 停)
    while cut_index > start_index {
        let prev = &entries[cut_index - 1];
        if matches!(prev, Entry::Compaction { .. })
            || !crate::projection::session_entry_to_context_messages(prev).is_empty()
        {
            break;
        }
        cut_index -= 1;
    }

    let cut_entry = &entries[cut_index];
    let starts_turn = is_turn_start_entry(cut_entry);
    let turn_start_index = if starts_turn {
        None
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };
    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

// ============================================================================
// 对话序列化与摘要(06 文档 §3.4)
// ============================================================================

/// 摘要请求:待摘要的上下文消息 + 摘要指令(实现方负责 convertToLlm 与调用)。
pub struct SummarizationRequest {
    pub messages: Vec<AgentMessage>,
    pub instruction: String,
}

#[derive(Debug, Clone)]
pub struct SummarizationResponse {
    pub summary: String,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    pub usage: Option<Usage>,
}

/// 摘要 LLM 调用的注入接缝(装配方提供 provider 支撑实现)。
#[async_trait::async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(
        &self,
        request: &SummarizationRequest,
    ) -> Result<SummarizationResponse, String>;
}

/// 工厂:恒定返回固定摘要的 Summarizer(测试/离线装配用)。
pub fn create_fixed_summarizer(summary: impl Into<String>) -> std::sync::Arc<dyn Summarizer> {
    std::sync::Arc::new(FixedSummarizer {
        summary: summary.into(),
    })
}

struct FixedSummarizer {
    summary: String,
}

#[async_trait::async_trait]
impl Summarizer for FixedSummarizer {
    async fn summarize(
        &self,
        _request: &SummarizationRequest,
    ) -> Result<SummarizationResponse, String> {
        Ok(SummarizationResponse {
            summary: self.summary.clone(),
            stop_reason: StopReason::Stop,
            error_message: None,
            usage: None,
        })
    }
}

/// tool result 在摘要序列化里的截断上限(06 文档 TOOL_RESULT_MAX_CHARS)。
const TOOL_RESULT_MAX_CHARS: usize = 2000;

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_string();
    }
    // 按字符截断(字节切片会切进多字节字符中间而 panic)
    let cut: String = text.chars().take(max_chars).collect();
    format!(
        "{cut}\n\n[... {} more characters truncated]",
        total_chars - max_chars
    )
}

fn content_blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// 序列化对话为摘要输入(06 文档 serializeConversation):tool result 截 2000 字符。
pub fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for message in messages {
        match message {
            AgentMessage::User { content, .. } => {
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            AgentMessage::Assistant(assistant) => {
                let mut thinking_parts: Vec<&str> = Vec::new();
                let mut tool_calls: Vec<String> = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Thinking { thinking, .. } => thinking_parts.push(thinking),
                        ContentBlock::ToolCall {
                            name, arguments, ..
                        } => {
                            let args = arguments
                                .as_object()
                                .map(|obj| {
                                    obj.iter()
                                        .map(|(k, v)| {
                                            format!(
                                                "{k}={}",
                                                serde_json::to_string(v).unwrap_or_default()
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            tool_calls.push(format!("{name}({args})"));
                        }
                        _ => {}
                    }
                }
                if !thinking_parts.is_empty() {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        thinking_parts.join("\n")
                    ));
                }
                let text = content_blocks_text(&assistant.content);
                if assistant
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. }))
                {
                    parts.push(format!("[Assistant]: {text}"));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            AgentMessage::ToolResult { content, .. } => {
                let text = content_blocks_text(content);
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            // pi 先 convertToLlm 再序列化:以下三类折叠为 user 文本后才进摘要
            AgentMessage::BashExecution {
                command, output, ..
            } => {
                let mut text = format!("Ran `{command}`\n");
                if output.is_empty() {
                    text.push_str("(no output)");
                } else {
                    text.push_str(&format!("```\n{output}\n```"));
                }
                parts.push(format!("[User]: {text}"));
            }
            AgentMessage::BranchSummary { summary, .. }
            | AgentMessage::CompactionSummary { summary, .. } => {
                parts.push(format!("[User]: {summary}"));
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// 固定摘要模板(06 文档 SUMMARIZATION_PROMPT,文本与 pi 一致)。
pub const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_INSTRUCTIONS: &str = "Update the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// 增量更新模板(06 文档 UPDATE_SUMMARIZATION_PROMPT = 头部 + 更新指令)。
const UPDATE_SUMMARIZATION_HEADER: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.";

pub fn update_summarization_prompt() -> String {
    format!("{UPDATE_SUMMARIZATION_HEADER}\n\n{UPDATE_SUMMARIZATION_INSTRUCTIONS}")
}

/// length 摘要不可入库(06 文档 getSummarizationFailure)。
pub fn get_summarization_failure(response: &SummarizationResponse, label: &str) -> Option<String> {
    match response.stop_reason {
        StopReason::Error => Some(format!(
            "{label} failed: {}",
            response.error_message.as_deref().unwrap_or("Unknown error")
        )),
        StopReason::Length => Some(format!(
            "{label} failed: generation hit the token cap and the summary is incomplete"
        )),
        _ => None,
    }
}

/// 从 assistant 工具调用提取文件操作(06 文档 compaction/utils.ts computeFileLists)。
fn compute_file_lists(messages: &[AgentMessage]) -> (Vec<String>, Vec<String>) {
    use std::collections::BTreeSet;
    let mut read = BTreeSet::new();
    let mut written = BTreeSet::new();
    let mut edited = BTreeSet::new();
    for message in messages {
        let AgentMessage::Assistant(assistant) = message else {
            continue;
        };
        for block in &assistant.content {
            let ContentBlock::ToolCall {
                name, arguments, ..
            } = block
            else {
                continue;
            };
            let Some(path) = arguments.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            match name.as_str() {
                "read" => {
                    read.insert(path.to_string());
                }
                "write" => {
                    written.insert(path.to_string());
                }
                "edit" => {
                    edited.insert(path.to_string());
                }
                _ => {}
            }
        }
    }
    let modified: BTreeSet<String> = written.union(&edited).cloned().collect();
    let read_files: Vec<String> = read.into_iter().filter(|f| !modified.contains(f)).collect();
    let modified_files: Vec<String> = modified.into_iter().collect();
    (read_files, modified_files)
}

fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

#[derive(Debug, Clone)]
pub struct CompactionOutcome {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    /// details:`{readFiles, modifiedFiles}`(06 文档 §3.4)
    pub details: serde_json::Value,
    pub usage: Option<Usage>,
}

/// 运行一次压缩摘要(06 文档 §3.4 的 M3 实现):对 `entries`(活动分支路径,
/// leaf→root)做切点 → 序列化 → Summarizer 调用 → 产出可 append 的 Compaction
/// 数据。无有效切点/无可摘要内容时返回 None。
///
/// 取舍:分割 turn 时不做 history/prefix 两次请求,
/// 单请求覆盖全范围,turn_start_index 经 `CompactionOutcome` 的 details 之外
/// 由调用方按需从 entries 推导。
pub async fn run_compaction(
    entries: &[Entry],
    settings: &CompactionSettings,
    summarizer: &dyn Summarizer,
) -> Result<Option<CompactionOutcome>, String> {
    let projection = build_session_projection(entries, None);
    let estimate = estimate_projected_context_tokens(&projection, entries);

    let cut = find_cut_point(entries, 0, entries.len(), settings.keep_recent_tokens);
    let first_kept = cut.first_kept_entry_index;
    if first_kept == 0 || entries.is_empty() {
        return Ok(None);
    }

    // 待摘要范围的上下文消息
    let summarized_messages: Vec<AgentMessage> = entries[..first_kept]
        .iter()
        .flat_map(crate::projection::session_entry_to_context_messages)
        .collect();
    if summarized_messages.is_empty() {
        return Ok(None);
    }

    // 增量:范围里已有 compaction 摘要时走 update 模板(旧摘要进 <previous-summary>)
    let previous_summary = entries[..first_kept]
        .iter()
        .rev()
        .find_map(|entry| match entry {
            Entry::Compaction { summary, .. } => Some(summary.clone()),
            _ => None,
        });
    let (conversation, instruction) = match previous_summary {
        Some(previous) => (
            format!(
                "<previous-summary>\n{previous}\n</previous-summary>\n\n{}",
                serialize_conversation(&summarized_messages)
            ),
            update_summarization_prompt(),
        ),
        None => (
            serialize_conversation(&summarized_messages),
            SUMMARIZATION_PROMPT.to_string(),
        ),
    };
    // 序列化后为空(短会话切点落在首个 user 消息,范围内只有元数据 entry)
    // → 没有可摘要的内容:不发摘要请求、不产生 Compaction entry,否则 LLM
    // 在看不到任何对话的情况下编造的"摘要"会被当作真实历史压缩结果落盘
    if conversation.trim().is_empty() {
        return Ok(None);
    }

    // 摘要请求消息:占位 user 承载序列化对话(实现方把它发给 LLM)
    let request_messages = vec![AgentMessage::User {
        content: conversation,
        timestamp: rpi_agent::now_ms(),
    }];

    let response = summarizer
        .summarize(&SummarizationRequest {
            messages: request_messages,
            instruction,
        })
        .await?;
    if let Some(failure) = get_summarization_failure(&response, "summarization") {
        return Err(failure);
    }

    let (read_files, modified_files) = compute_file_lists(&summarized_messages);
    // pi 语义:文件清单以 XML 节追加到摘要文本,同时存 details
    let summary = format!(
        "{}{}",
        response.summary,
        format_file_operations(&read_files, &modified_files)
    );
    let mut details = serde_json::json!({"readFiles": read_files, "modifiedFiles": modified_files});
    if cut.is_split_turn {
        if let Some(turn_start) = cut.turn_start_index.and_then(|i| entries.get(i)) {
            details["turnStartEntryId"] = serde_json::Value::String(turn_start.id().to_string());
        }
    }

    // 压缩边界不再快照系统提示词/工具状态:session 不持久化能力规则,
    // 恢复时系统提示词从配置重组、工具 schema 每次请求动态下发

    Ok(Some(CompactionOutcome {
        summary,
        first_kept_entry_id: entries[first_kept].id().to_string(),
        tokens_before: estimate.tokens,
        details,
        usage: response.usage,
    }))
}
