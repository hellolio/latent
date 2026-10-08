//! 启动回放(pi renderSessionItems 语义):恢复/续聊时把当前转录渲染进
//! 转录模型(样式与实时渲染完全一致,同走 TranscriptItem)。

use latent_agent::AgentMessage;
use latent_ai::{ContentBlock, StopReason};

use super::handlers::InteractiveCtx;
use super::state::{InteractiveState, ToolStatus, TranscriptItem};

/// 回放当前转录(压缩感知的当前分支上下文)并初始化 ctx 估计。
/// 回放**当前会话**(主会话或平行子 agent 会话 —— /session 恢复按各自
/// 谱系进行,子会话恢复后回放的是子会话的转录)。
pub fn replay_history(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    let messages = ctx.session.current().agent().messages();
    for message in &messages {
        state.commit_many(replay_message(message, &state.theme));
    }
    if let Some(tokens) = messages.iter().rev().find_map(|message| {
        message
            .as_assistant()
            .map(|assistant| super::usage::context_tokens_of(&assistant.usage))
    }) {
        state.context_tokens = tokens;
    }
    // 压缩计数只对主谱系有意义(子会话未装配 compactor)
    if ctx.session.is_main() {
        if let Some(manager) = ctx.current_manager() {
            let compactions = manager
                .branch_entries()
                .iter()
                .filter(|entry| matches!(entry, latent_session::Entry::Compaction { .. }))
                .count();
            if compactions > 0 {
                state.commit(TranscriptItem::Line(ratatui::text::Line::from(
                    ratatui::text::Span::styled(
                        format!("Session compacted {compactions} times"),
                        ratatui::style::Style::new().fg(state.theme.dim),
                    ),
                )));
            }
        }
    }
    state.commit(TranscriptItem::Blank);
}

/// 单条历史消息 → 转录条目(与实时渲染同一模型)。
fn replay_message(message: &AgentMessage, theme: &latent_tui::Theme) -> Vec<TranscriptItem> {
    match message {
        AgentMessage::User { content, .. } => {
            vec![
                TranscriptItem::User {
                    content: content.clone(),
                },
                TranscriptItem::Blank,
            ]
        }
        AgentMessage::Assistant(assistant) => {
            // thinking 与正文合成为可重渲染条目;工具调用/错误随 stop_reason。
            let mut markdown = String::new();
            let mut items: Vec<TranscriptItem> = Vec::new();
            for block in &assistant.content {
                match block {
                    ContentBlock::Text { text, .. } => {
                        if !markdown.is_empty() {
                            markdown.push('\n');
                        }
                        markdown.push_str(text);
                    }
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => {
                        let args = serde_json::to_string(arguments).unwrap_or_default();
                        items.push(TranscriptItem::ToolCall {
                            name: name.clone(),
                            args,
                            status: ToolStatus::Success,
                        });
                    }
                    ContentBlock::Thinking { thinking, .. } => {
                        items.push(TranscriptItem::Thinking {
                            text: thinking.clone(),
                        });
                    }
                    ContentBlock::Image { .. } => {}
                }
            }
            if !markdown.trim().is_empty() {
                items.push(TranscriptItem::Assistant { markdown });
            }
            match assistant.stop_reason {
                StopReason::Error => items.push(TranscriptItem::Line(error_line_msg(
                    assistant
                        .error_message
                        .as_deref()
                        .unwrap_or("Unknown error"),
                    theme,
                ))),
                StopReason::Aborted => items.push(TranscriptItem::Line(error_line_msg(
                    "Operation aborted",
                    theme,
                ))),
                StopReason::Length => items.push(TranscriptItem::Line(error_line_msg(
                    "Response was truncated before completion.",
                    theme,
                ))),
                _ => {}
            }
            // 不追加尾随空行(与实时路径一致:用量/后续条目自带间距)
            items
        }
        AgentMessage::ToolResult { is_error, .. } => vec![
            // 不前置空行:紧随其 ToolCall 条目,边框卡片才能拼成同框
            TranscriptItem::ToolResult {
                output: message.tool_result_content().unwrap_or_default(),
                is_error: *is_error,
            },
        ],
        AgentMessage::BashExecution {
            command,
            output,
            exit_code,
            ..
        } => vec![
            TranscriptItem::Bash {
                command: command.clone(),
                output: output.clone(),
                is_error: exit_code.map(|code| code != 0).unwrap_or(false),
            },
            TranscriptItem::Blank,
        ],
        AgentMessage::CompactionSummary { summary, .. } => vec![TranscriptItem::Line(
            ratatui::text::Line::from(ratatui::text::Span::styled(
                format!("── 压缩摘要: {}", first_line(summary)),
                ratatui::style::Style::new().fg(theme.dim),
            )),
        )],
        AgentMessage::BranchSummary { summary, .. } => vec![TranscriptItem::Line(
            ratatui::text::Line::from(ratatui::text::Span::styled(
                format!("── 分支摘要: {}", first_line(summary)),
                ratatui::style::Style::new().fg(theme.dim),
            )),
        )],
        AgentMessage::ModeSection { content, .. } => vec![TranscriptItem::Line(
            ratatui::text::Line::from(ratatui::text::Span::styled(
                format!("── 模式切换: {}", first_line(content)),
                ratatui::style::Style::new().fg(theme.dim),
            )),
        )],
        AgentMessage::ProjectContext { content, .. } => vec![TranscriptItem::Line(
            ratatui::text::Line::from(ratatui::text::Span::styled(
                format!("── 项目上下文: {}", first_line(content)),
                ratatui::style::Style::new().fg(theme.dim),
            )),
        )],
        AgentMessage::Custom(_) => vec![TranscriptItem::Blank],
    }
}

fn error_line_msg(text: &str, theme: &latent_tui::Theme) -> ratatui::text::Line<'static> {
    ratatui::text::Line::from(ratatui::text::Span::styled(
        format!("Error: {text}"),
        ratatui::style::Style::new().fg(theme.error),
    ))
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}
