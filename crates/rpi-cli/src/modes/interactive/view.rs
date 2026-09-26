//! 视图渲染:状态(InteractiveState + 转录模型)→ `Vec<Line>`。
//! 全部纯函数;终端 I/O 只发生在 mod.rs 的事件循环里(commit/draw)。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use rpi_tui::footer::FooterData;
use rpi_tui::{loader, markdown, tool_card, Theme, UiLine};

use super::state::{InteractiveState, Status, ToolStatus, TranscriptItem};

/// 流式预览最多展示的行数(超出取尾部)。
pub const MAX_PREVIEW_ROWS: usize = 8;
/// 编辑器最多展示的视觉行数(pi 编辑器同样封顶)。
pub const MAX_EDITOR_ROWS: usize = 6;

/// 转录条目 → 行(含样式;折行由 `TuiApp::commit_lines` 按终端宽度做)。
pub fn render_item(
    item: &TranscriptItem,
    theme: &Theme,
    width: usize,
    expanded: bool,
) -> Vec<UiLine> {
    match item {
        TranscriptItem::Line(line) => vec![line.clone()],
        TranscriptItem::Blank => vec![Line::raw("")],
        TranscriptItem::User { content } => user_block(content, theme, width),
        TranscriptItem::Assistant { markdown } => assistant_markdown(markdown, theme, width),
        TranscriptItem::Thinking { text } => thinking_block(text, theme),
        TranscriptItem::ToolCall { name, args, status } => {
            vec![tool_card::title_line(
                name,
                args,
                (*status).into(),
                width,
                theme,
            )]
        }
        TranscriptItem::ToolResult {
            output, is_error, ..
        } => {
            let status = if *is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
            let mut out = tool_card::output_lines(output, status.into(), expanded, width, theme);
            out.push(Line::raw(""));
            out
        }
        TranscriptItem::Bash {
            command,
            output,
            is_error,
        } => {
            let mut out = vec![Line::from(vec![
                Span::styled(
                    "! ",
                    Style::new()
                        .fg(theme.border_bash)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(command.clone(), Style::new().fg(theme.assistant_text)),
            ])];
            if !output.trim().is_empty() {
                let status = if *is_error {
                    ToolStatus::Error
                } else {
                    ToolStatus::Success
                };
                out.extend(tool_card::output_lines(
                    output,
                    status.into(),
                    expanded,
                    width,
                    theme,
                ));
            }
            out
        }
    }
}

/// 整个转录的重渲染(ctrl+o 展开/收起后的全文重绘)。
pub fn render_transcript(
    items: &[TranscriptItem],
    theme: &Theme,
    width: usize,
    expanded: bool,
) -> Vec<UiLine> {
    let mut out = Vec::new();
    for item in items {
        out.extend(render_item(item, theme, width, expanded));
    }
    out
}

/// user 消息背景块(pi userMessageBg):整行铺背景色。
pub fn user_block(content: &str, theme: &Theme, width: usize) -> Vec<UiLine> {
    let style = Style::new().fg(theme.user_text).bg(theme.user_bg);
    let inner = width.saturating_sub(2).max(1);
    let mut out = Vec::new();
    for raw in rpi_tui::wrap_to_width(content, inner) {
        let pad = inner.saturating_sub(rpi_tui::display_width(&raw));
        out.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(format!("{raw}{}", " ".repeat(pad)), style),
            Span::raw(" "),
        ]));
    }
    if out.is_empty() {
        out.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(" ", style),
            Span::raw(" "),
        ]));
    }
    out
}

/// assistant 正文:Markdown(syntect 代码高亮,主题跟随明暗)。
pub fn assistant_markdown(source: &str, theme: &Theme, width: usize) -> Vec<UiLine> {
    markdown::Markdown::new(theme)
        .with_highlight(rpi_tui::Highlighter::shared(theme.is_dark))
        .render(source, width)
}

/// thinking 块(✻ 前缀,dim;进转录可回看)。折行由 commit_lines 按终端宽度做。
pub fn thinking_block(text: &str, theme: &Theme) -> Vec<UiLine> {
    let style = Style::new().fg(theme.thinking);
    text.lines()
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| {
            Line::from(vec![
                Span::styled(format!("  {} ", loader::THINKING_MARK), style),
                Span::styled(raw.to_string(), style),
            ])
        })
        .collect()
}

/// 错误/警告行。
pub fn error_line(text: &str, theme: &Theme) -> UiLine {
    Line::from(Span::styled(
        format!("Error: {text}"),
        Style::new().fg(theme.error),
    ))
}

/// 启动区横幅(展开态含完整帮助);分隔线由启动组装层追加。
pub fn welcome_lines(version: &str, expanded: bool, theme: &Theme, width: usize) -> Vec<UiLine> {
    let mut out = rpi_tui::header_view::banner(version, expanded, width, theme);
    out.push(Line::raw(""));
    out
}

/// 底部视口帧:预览区(流式尾部/thinking 尾部)+ 状态行 + 补全弹窗 +
/// 编辑器框 + footer(Codex CLI 布局)。
pub struct ViewportFrame {
    pub lines: Vec<UiLine>,
    /// 光标相对视口左上角的 (列, 行);None = 隐藏
    pub cursor: Option<(u16, u16)>,
    pub height: u16,
}

/// 组装视口帧。`preview_cap`/`editor_cap`/`popup_cap` 限制预览区、编辑器与
/// 补全弹窗行数(终端过矮时由调用方收缩重算)。
pub fn viewport(
    state: &InteractiveState,
    partial: Option<&rpi_ai::AssistantMessage>,
    preview_cap: usize,
    editor_cap: usize,
    popup_cap: usize,
) -> ViewportFrame {
    let theme = &state.theme;
    let width = state.width.max(1);
    let mut lines: Vec<UiLine> = Vec::new();
    let mut cursor: Option<(u16, u16)> = None;

    // 1. 预览区:选择列表 > 流式文本尾部 > thinking/工具参数尾部
    if let Some(select) = &state.select {
        lines.push(Line::from(Span::styled(
            select.prompt.clone(),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        )));
        lines.extend(rpi_tui::SelectList::render(&select.list, width, theme));
    } else if !state.stream_text.is_empty() {
        let wrapped = rpi_tui::wrap_to_width(&state.stream_text, width);
        let skip = wrapped.len().saturating_sub(preview_cap);
        for row in wrapped.into_iter().skip(skip) {
            lines.push(Line::from(Span::styled(
                row,
                Style::new().fg(theme.assistant_text),
            )));
        }
    } else if let Some(text) = state
        .pending_thinking
        .as_ref()
        .filter(|t| !t.trim().is_empty())
    {
        let rows: Vec<&str> = text.lines().rev().take(preview_cap.min(3)).collect();
        for row in rows.into_iter().rev() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", loader::THINKING_MARK),
                    Style::new().fg(theme.thinking),
                ),
                Span::styled(
                    truncate_plain(row, width.saturating_sub(4)),
                    Style::new().fg(theme.thinking),
                ),
            ]));
        }
    } else if let Some(partial) = partial {
        if let Some(args) = toolcall_args_preview(partial) {
            lines.push(Line::from(Span::styled(
                format!("⚙ {args}"),
                Style::new().fg(theme.dim),
            )));
        }
    }

    // 2. 状态行(Codex 风格,busy 时一行;idle 不占行):
    //    `↻ Working (3s · esc to interrupt)` / `⏺ bash` / `aborted`
    let status = status_line(state);
    lines.extend(status);

    // 3. 补全弹窗:紧贴编辑器框上方(Codex 布局)
    if state.select.is_none() && state.slash_popup.visible() {
        lines.extend(state.slash_popup.render(width, theme, popup_cap));
    }

    // 4. 编辑器框:无边框标题圆角框;状态语义只体现在边框颜色上
    let border_style = border_style(state);
    let inner_width = width.saturating_sub(2).max(1);
    let view = state
        .editor
        .view(inner_width.saturating_sub(2), editor_cap.max(1));
    lines.push(Line::from(Span::styled(
        format!("╭{}", "─".repeat(width.saturating_sub(2))),
        border_style,
    )));

    // 5. 编辑器内容行(首行 ❯ 前缀;光标按显示宽定位;空输入显示占位文本)
    let editor_first_row = lines.len();
    if state.editor.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("❯ ".to_string(), Style::new().fg(theme.accent)),
            Span::styled(
                "Ask rpi to do anything".to_string(),
                Style::new().fg(theme.dim),
            ),
        ]));
    } else {
        for (i, row) in view.rows.iter().enumerate() {
            let prefix = if i == 0 { "❯ " } else { "  " };
            let mut spans = vec![Span::styled(
                prefix.to_string(),
                Style::new().fg(theme.accent),
            )];
            spans.push(Span::styled(
                row.clone(),
                Style::new().fg(theme.assistant_text),
            ));
            lines.push(Line::from(spans));
        }
    }
    if let Some((row, col)) = view.cursor {
        cursor = Some((
            (2 + col).min(width.saturating_sub(2)) as u16,
            (editor_first_row + row) as u16,
        ));
    }

    // 6. 底边框
    lines.push(Line::from(Span::styled(
        format!("╰{}", "─".repeat(width.saturating_sub(2))),
        border_style,
    )));

    // 7. footer 两行
    let footer = FooterData {
        cwd: state.cwd_display.clone(),
        git_branch: state.git_branch.clone(),
        session_label: state.session_label.clone(),
        input_tokens: state.usage.total.input,
        output_tokens: state.usage.total.output,
        cost_total: state.usage.total.cost.total,
        context_window: state.context_window,
        context_tokens: state.context_tokens,
        model: state.model_label.clone(),
        thinking: state.thinking_label.clone(),
        auto_compact: state.auto_compact,
    };
    lines.extend(rpi_tui::footer::lines(&footer, width, theme));

    let height = lines.len() as u16;
    ViewportFrame {
        lines,
        cursor,
        height,
    }
}

/// 状态行(Codex 风格:busy 时一行带 spinner 与 `esc to interrupt` 提示;
/// idle 时不占行)。
fn status_line(state: &InteractiveState) -> Vec<UiLine> {
    let theme = &state.theme;
    let secs = state.spin * super::SPINNER_INTERVAL.as_millis() as usize / 1000;
    let (color, text) = match &state.status {
        Status::Idle => return Vec::new(),
        Status::Thinking => (
            theme.spinner,
            format!(
                "{} Working ({secs}s · esc to interrupt)",
                loader::frame(state.spin)
            ),
        ),
        Status::Tool(name) => (
            theme.tool_pending,
            format!(
                "{} Running {name} ({secs}s · esc to interrupt)",
                loader::frame(state.spin)
            ),
        ),
        Status::Compacting => (
            theme.spinner,
            format!("{} Compacting history", loader::frame(state.spin)),
        ),
        Status::Bash(command) => (theme.border_bash, format!("! {command}")),
        Status::Aborted => (theme.error, "aborted".to_string()),
    };
    vec![Line::from(Span::styled(text, Style::new().fg(color)))]
}

fn border_style(state: &InteractiveState) -> Style {
    let theme = &state.theme;
    let color = match &state.status {
        Status::Idle => theme.border_idle,
        Status::Bash(_) => theme.border_bash,
        Status::Aborted => theme.error,
        _ => theme.border_busy,
    };
    Style::new().fg(color)
}

fn truncate_plain(text: &str, width: usize) -> String {
    rpi_tui::truncate_to_width(text, width.max(1)).0
}

/// 从 partial 快照提取工具参数预览(T2:参数逐块增长)。
fn toolcall_args_preview(partial: &rpi_ai::AssistantMessage) -> Option<String> {
    let args: String = partial
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::ContentBlock::ToolCall {
                name, arguments, ..
            } => Some(format!(
                "{}{}",
                if name.is_empty() {
                    String::new()
                } else {
                    format!("{name} ")
                },
                arguments
            )),
            _ => None,
        })
        .collect();
    (!args.is_empty()).then_some(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::state::InteractiveState;
    use rpi_tui::text::line_text;

    fn theme() -> Theme {
        Theme::dark_ansi()
    }

    fn state() -> InteractiveState {
        InteractiveState::new(theme(), 80)
    }

    #[test]
    fn user_block_pads_background() {
        let lines = user_block("hello", &theme(), 40);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        // 内容 + 补齐空格 + 两侧留白
        assert_eq!(rpi_tui::display_width(&text), 40, "{text:?}");
        assert!(text.trim().starts_with("hello"));
    }

    #[test]
    fn assistant_markdown_renders_headings_and_code() {
        let lines = assistant_markdown("# Hi\n```rust\nlet a=1;\n```", &theme(), 60);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts[0], "Hi");
        assert!(texts.iter().any(|t| t.starts_with("╭── rust")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("let a=1;")));
    }

    #[test]
    fn thinking_block_prefixes_mark() {
        let lines = thinking_block("step\nstep2", &theme());
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).contains(loader::THINKING_MARK));
        assert!(line_text(&lines[0]).contains("step"));
    }

    #[test]
    fn viewport_layout_shape() {
        let mut state = state();
        state.editor.set_text("hi");
        let frame = viewport(&state, None, 8, 6, 8);
        // 空预览 + 顶边框 + 1 编辑行 + 底边框 + footer 2 行 = 5
        assert_eq!(
            frame.lines.len(),
            5,
            "{:?}",
            frame.lines.iter().map(line_text).collect::<Vec<_>>()
        );
        assert_eq!(frame.height, 5);
        // 光标在编辑器行(行 1),列 = 前缀 2 + "hi" 2 = 4
        assert_eq!(frame.cursor, Some((4, 1)));
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts[0].starts_with("╭"), "{texts:?}");
        assert!(texts[1].starts_with("❯ hi"));
        assert!(texts[2].starts_with("╰"), "{texts:?}");
    }

    #[test]
    fn viewport_shows_stream_tail() {
        let mut state = state();
        state.stream_text = "line1\nline2\nline3".into();
        let frame = viewport(&state, None, 2, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        // 预览只显示尾部 2 行
        assert!(texts.iter().any(|t| t.contains("line2")));
        assert!(texts.iter().any(|t| t.contains("line3")));
        assert!(!texts.iter().any(|t| t.contains("line1")), "{texts:?}");
    }

    #[test]
    fn viewport_status_line_reflects_busy_state() {
        let mut state = state();
        // idle:状态行不占位
        assert_eq!(viewport(&state, None, 0, 6, 8).lines.len(), 5);
        state.status = Status::Thinking;
        state.spin = 25; // 25 * 120ms = 3s
        let frame = viewport(&state, None, 0, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts[0].contains(loader::frame(25)), "{texts:?}");
        assert!(texts[0].contains("Working (3s"), "{texts:?}");
        assert!(texts[0].contains("esc to interrupt"), "{texts:?}");
        state.status = Status::Bash("ls".into());
        let frame = viewport(&state, None, 0, 6, 8);
        assert!(line_text(&frame.lines[0]).contains("! ls"));
    }

    #[test]
    fn viewport_empty_editor_shows_placeholder() {
        let state = state();
        let frame = viewport(&state, None, 0, 6, 8);
        let text = line_text(&frame.lines[1]);
        assert!(text.starts_with("❯ "), "{text}");
        assert!(text.contains("Ask rpi to do anything"), "{text}");
    }

    #[test]
    fn viewport_shows_slash_popup_above_composer() {
        let mut state = state();
        state.editor.set_text("/mod");
        state.sync_slash_popup();
        assert!(state.slash_popup.visible());
        let frame = viewport(&state, None, 0, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        // 弹窗(边框 + 1 个 /model 匹配行)紧贴编辑器框上方
        let popup_top = texts.iter().position(|t| t.starts_with('╭')).unwrap();
        assert!(texts[popup_top + 1].contains("/model"), "{texts:?}");
        assert!(texts[popup_top + 2].starts_with('╰'), "{texts:?}");
        assert!(texts[popup_top + 3].starts_with('╭'), "弹窗下方是编辑器框: {texts:?}");
        assert!(texts[popup_top + 4].starts_with("❯ /mod"), "{texts:?}");
    }

    #[test]
    fn viewport_height_grows_with_multiline_editor() {
        let mut state = state();
        state.editor.set_text("a\nb\nc\nd");
        let single = viewport(&state_with_one(&state.theme), None, 0, 6, 8);
        let multi = viewport(&state, None, 0, 6, 8);
        assert_eq!(multi.height, single.height + 3);
    }

    fn state_with_one(theme: &Theme) -> InteractiveState {
        InteractiveState::new(*theme, 80)
    }
}
