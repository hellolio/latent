//! 视图渲染:状态(InteractiveState + 转录模型)→ `Vec<Line>`。
//! 全部纯函数;终端 I/O 只发生在 mod.rs 的事件循环里(commit/draw)。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use rpi_tui::footer::FooterData;
use rpi_tui::{loader, markdown, tool_card, Theme, UiLine};

use super::state::{InteractiveState, Status, ToolStatus, TranscriptItem};

/// 交流区与输入区之间的空隙行数(空闲/工具执行时预览区固定补空到此值)。
pub const MAX_PREVIEW_ROWS: usize = 2;
/// 流式输出/思考中预览区的固定行数(超出取尾部)。视口高度在全过程中
/// 恒定:模型回话期间视口不因增量而改高(ratatui Inline 视口改高要
/// insert_before + 清屏 + 重建 Terminal,逐增量触发会闪烁并把屏幕顶行
/// 推进 scrollback,冲刷真实历史),全文在 MessageEnd 一并进转录。
pub const STREAM_PREVIEW_ROWS: usize = 10;
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

/// user 消息背景块(pi userMessageBg):整行铺背景色,上下各留一行同色
/// 空行作内边距(块高 ≈ 字体高度 2 倍以上,呼吸感与输入区一致)。背景
/// 覆盖包括行尾在内的每一个单元格,保证块内背景完全一致(无终端底色缝隙)。
pub fn user_block(content: &str, theme: &Theme, width: usize) -> Vec<UiLine> {
    let style = Style::new().fg(theme.user_text).bg(theme.user_bg);
    let pad_line = || Line::from(Span::styled(" ".repeat(width.max(1)), style));
    let mut out = vec![pad_line()];
    let mut body = Vec::new();
    for raw in rpi_tui::wrap_to_width(content, width.max(1)) {
        let pad = width.saturating_sub(rpi_tui::display_width(&raw));
        body.push(Line::from(Span::styled(
            format!("{raw}{}", " ".repeat(pad)),
            style,
        )));
    }
    if body.is_empty() {
        body.push(pad_line());
    }
    out.extend(body);
    out.push(pad_line());
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
/// 编辑器区(带背景色,无边框)+ footer(Codex CLI 布局)。
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

    // 1. 预览区:选择列表 > 流式文本尾部 > thinking/工具参数尾部。
    //    无选择列表时固定占满 preview_cap 行(内容不足补空行):视口高度
    //    从此只随用户操作(多行输入/弹窗/选择列表)变化,模型回话全程
    //    高度恒定,输入框不会因流式更新而跳动。
    let mut preview: Vec<UiLine> = Vec::new();
    if let Some(select) = &state.select {
        preview.push(Line::from(Span::styled(
            select.prompt.clone(),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        )));
        preview.extend(rpi_tui::SelectList::render(&select.list, width, theme));
    } else if !state.stream_text.is_empty() {
        let wrapped = rpi_tui::wrap_to_width(&state.stream_text, width);
        let skip = wrapped.len().saturating_sub(preview_cap);
        for row in wrapped.into_iter().skip(skip) {
            preview.push(Line::from(Span::styled(
                row,
                Style::new().fg(theme.assistant_text),
            )));
        }
    } else if let Some(text) = state
        .pending_thinking
        .as_ref()
        .filter(|t| !t.trim().is_empty())
    {
        let rows: Vec<&str> = text.lines().rev().take(preview_cap).collect();
        for row in rows.into_iter().rev() {
            preview.push(Line::from(vec![
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
            preview.push(Line::from(Span::styled(
                format!("⚙ {args}"),
                Style::new().fg(theme.dim),
            )));
        }
    }
    if state.select.is_none() {
        while preview.len() < preview_cap {
            preview.push(Line::raw(""));
        }
    }
    lines.extend(preview);

    // 1.5 空隙行:预览区(模型输出)与状态行之间保留一行字体间距
    lines.push(Line::raw(""));

    // 2. 状态行(Codex 风格,busy 时一行带 shimmer 渐变;idle 不占行):
    //    `◐ Working (3s · esc to interrupt)` / `⏺ bash` / `aborted`
    let status = status_line(state);
    lines.extend(status);

    // 3. 补全弹窗:紧贴编辑器框上方(Codex 布局)
    if state.select.is_none() && state.slash_popup.visible() {
        lines.extend(state.slash_popup.render(width, theme, popup_cap));
    }

    // 4. 编辑器区:无边框,整行铺 user_bg 背景(与已发送用户消息同款,
    //    靠背景色区分输入区;busy 语义由上方状态行承担)
    let view = state
        .editor
        .view(width.saturating_sub(2), editor_cap.max(1));

    // 5. 编辑器内容行(整行铺 user_bg,上下各一行同色空行作内边距,与
    //    用户消息块一致;首行 ❯ 前缀;光标按显示宽定位;空输入显示占位文本)
    let bg = theme.user_bg;
    let text_style = Style::new().fg(theme.user_text).bg(bg);
    let prefix_style = Style::new().fg(theme.accent).bg(bg);
    let pad_row = |mut spans: Vec<Span<'static>>, used: usize| {
        let pad = width.saturating_sub(used);
        spans.push(Span::styled(" ".repeat(pad), text_style));
        Line::from(spans)
    };
    let full_pad = || Line::from(Span::styled(" ".repeat(width.max(1)), text_style));
    lines.push(full_pad());
    let editor_first_row = lines.len();
    if state.editor.is_empty() {
        let placeholder = "Ask rpi to do anything";
        lines.push(pad_row(
            vec![
                Span::styled("❯ ".to_string(), prefix_style),
                Span::styled(
                    placeholder.to_string(),
                    Style::new().fg(theme.dim).bg(bg),
                ),
            ],
            2 + rpi_tui::display_width(placeholder),
        ));
    } else {
        for (i, row) in view.rows.iter().enumerate() {
            let prefix = if i == 0 { "❯ " } else { "  " };
            lines.push(pad_row(
                vec![
                    Span::styled(prefix.to_string(), prefix_style),
                    Span::styled(row.clone(), text_style),
                ],
                2 + rpi_tui::display_width(row),
            ));
        }
    }
    lines.push(full_pad());
    if let Some((row, col)) = view.cursor {
        cursor = Some((
            (2 + col).min(width.saturating_sub(2)) as u16,
            (editor_first_row + row) as u16,
        ));
    }

    // 6. footer 三行(cwd · 右对齐 token 段 · 右对齐模型)
    let footer = FooterData {
        cwd: state.cwd_display.clone(),
        git_branch: state.git_branch.clone(),
        input_tokens: state.usage.total.input,
        output_tokens: state.usage.total.output,
        cache_read: state.usage.total.cache_read,
        cache_write: state.usage.total.cache_write,
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

/// 状态行(Codex 风格:busy 时一行带 shimmer 渐变与 `esc to interrupt` 提示;
/// idle 时空占一行,保证视口高度不随 busy↔idle 切换抖动)。
/// 等待态文字逐字符做明暗渐变,波峰随 spinner 拍数向右扫动(等待感)。
fn status_line(state: &InteractiveState) -> Vec<UiLine> {
    let theme = &state.theme;
    let secs = state.spin * super::SPINNER_INTERVAL.as_millis() as usize / 1000;
    let (color, text) = match &state.status {
        Status::Idle => return vec![Line::raw("")],
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
        Status::Bash(command) => {
            return vec![Line::from(Span::styled(
                format!("! {command}"),
                Style::new().fg(theme.border_bash),
            ))];
        }
        Status::Aborted => {
            return vec![Line::from(Span::styled(
                "aborted".to_string(),
                Style::new().fg(theme.error),
            ))];
        }
    };
    // shimmer:亮度 = 0.35~1.0 的正弦波,波峰位置随 tick 向右扫
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(1) as f32;
    let phase = (state.spin % 24) as f32 / 24.0;
    let spans: Vec<Span<'static>> = chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let wave =
                0.5 + 0.5 * ((i as f32 / n - phase) * std::f32::consts::TAU).sin();
            let t = 0.35 + 0.65 * wave;
            Span::styled(
                c.to_string(),
                Style::new().fg(rpi_tui::theme::blend_rgb(theme.dim, color, t)),
            )
        })
        .collect();
    vec![Line::from(spans)]
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
        // 上下各一行同背景色空行(内边距)+ 文字行
        assert_eq!(lines.len(), 3);
        let text = line_text(&lines[1]);
        // 内容 + 补齐空格,整行(含行尾)铺满背景
        assert_eq!(rpi_tui::display_width(&text), 40, "{text:?}");
        assert!(text.starts_with("hello"));
        // 三行(含内边距行)背景完全一致:各为单一 span,带 bg 色
        for line in &lines {
            assert_eq!(line.spans.len(), 1);
            assert_eq!(line.spans[0].style.bg, Some(theme().user_bg));
            assert_eq!(rpi_tui::display_width(&line_text(line)), 40);
        }
        // 内边距行为纯空格
        assert!(line_text(&lines[0]).trim().is_empty());
        assert!(line_text(&lines[2]).trim().is_empty());
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
        let mut st = state();
        st.editor.set_text("hi");
        let frame = viewport(&st, None, 8, 6, 8);
        // 预览 8(恒占)+ 空隙 1 + 状态 1 + 编辑区(上下内边距 2 + 编辑行 1)
        // + footer 3 = 16
        assert_eq!(
            frame.lines.len(),
            16,
            "{:?}",
            frame.lines.iter().map(line_text).collect::<Vec<_>>()
        );
        assert_eq!(frame.height, 16);
        // 光标在编辑器行(行 11 = 预览 8 + 空隙 1 + 状态 1 + 顶部内边距),列 = 2 + 2 = 4
        assert_eq!(frame.cursor, Some((4, 11)));
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts[11].starts_with("❯ hi"), "{texts:?}");
        // idle 时预览、空隙行与状态行均为空占位
        for row in &texts[..10] {
            assert!(row.trim().is_empty(), "空闲占位应为空行: {texts:?}");
        }
    }

    #[test]
    fn viewport_reserves_full_preview_height() {
        // 预览区固定占 preview_cap 行(内容不足补空行):视口高度只随用户
        // 操作变化,流式全程恒定,输入框不因高度抖动而闪烁
        let st = state();
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert_eq!(frame.height, 4 + 1 + 1 + 3 + 3);
        for row in &texts[..4] {
            assert!(row.trim().is_empty(), "预览空位应为空行: {texts:?}");
        }
        // 内容增长但未超 cap:预览区行数不变
        let mut st = state();
        st.status = Status::Thinking;
        st.stream_text = "line1\nline2".into();
        let frame = viewport(&st, None, 4, 6, 8);
        assert_eq!(frame.height, 4 + 1 + 1 + 3 + 3);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts.iter().take(4).any(|t| t.contains("line2")));
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
        // idle:预览空位 + 空隙行 + 状态行空占位
        assert_eq!(viewport(&state, None, 0, 6, 8).lines.len(), 8);
        assert!(line_text(&viewport(&state, None, 0, 6, 8).lines[0]).trim().is_empty());
        state.status = Status::Thinking;
        state.spin = 25; // 25 * 120ms = 3s
        let frame = viewport(&state, None, 0, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        // 预览空位(0)+ 空隙行 → 状态行在 index 1
        assert!(texts[1].contains(loader::frame(25)), "{texts:?}");
        assert!(texts[1].contains("Working (3s"), "{texts:?}");
        assert!(texts[1].contains("esc to interrupt"), "{texts:?}");
        state.status = Status::Bash("ls".into());
        let frame = viewport(&state, None, 0, 6, 8);
        assert!(line_text(&frame.lines[1]).contains("! ls"));
    }

    #[test]
    fn viewport_empty_editor_shows_placeholder() {
        let state = state();
        let frame = viewport(&state, None, 0, 6, 8);
        // 预览 0 + 空隙 1 + 状态 1 + 顶部内边距 → 编辑器首行在 index 3
        let text = line_text(&frame.lines[3]);
        assert!(text.starts_with("❯ "), "{text}");
        assert!(text.contains("Ask rpi to do anything"), "{text}");
    }

    #[test]
    fn viewport_editor_row_has_full_width_background() {
        let mut st = state();
        st.editor.set_text("hi");
        let frame = viewport(&st, None, 0, 6, 8);
        // 空隙 1 + 状态 1 → 顶部内边距 idx 2,编辑行 idx 3
        let row = &frame.lines[3];
        // 前缀 + 内容 + 行尾补齐,三个 span 共用 user_bg(整行无底色缝隙)
        assert_eq!(row.spans.len(), 3, "{row:?}");
        for span in &row.spans {
            assert_eq!(span.style.bg, Some(st.theme.user_bg), "{span:?}");
        }
        assert_eq!(rpi_tui::display_width(&line_text(row)), 80);
    }

    #[test]
    fn viewport_shows_slash_popup_above_composer() {
        let mut state = state();
        state.editor.set_text("/mod");
        state.sync_slash_popup();
        assert!(state.slash_popup.visible());
        let frame = viewport(&state, None, 0, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        // 弹窗(边框 + 1 个 /model 匹配行)紧贴编辑器区上方
        let popup_top = texts.iter().position(|t| t.starts_with('╭')).unwrap();
        assert!(texts[popup_top + 1].contains("/model"), "{texts:?}");
        assert!(texts[popup_top + 2].starts_with('╰'), "{texts:?}");
        // 弹窗下方是编辑器区顶部内边距行(空),再往下才是 ❯ 行
        assert!(texts[popup_top + 3].trim().is_empty(), "{texts:?}");
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
