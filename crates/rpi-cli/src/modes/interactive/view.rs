//! 视图渲染:状态(InteractiveState + 转录模型)→ `Vec<Line>`。
//! 全部纯函数;终端 I/O 只发生在 mod.rs 的事件循环里(commit/draw)。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use rpi_tui::footer::FooterData;
use rpi_tui::{loader, markdown, tool_card, Theme, UiLine};

use super::state::{InteractiveState, Status, TranscriptItem};

/// 交流区与输入区之间的空隙行数(空闲时预览区不占行,恒为 0)。
pub const MAX_PREVIEW_ROWS: usize = 0;
/// 流式预览区行数(busy 预留,超出取尾部)。视口高度在回合全程恒定。
/// 守恒律:insert_before 滚动机制决定了「回合末 tokens 下方残留的空带 =
/// busy 预留 + 状态行 + 编辑器内边距」,且 tokens 在收缩**前**落盘时
/// 与 AI 框紧贴、空带全部落在 tokens 下方。预留 1 行 → 生成中可见 1 行
/// 流式尾部,回合末 tokens→输入行共 3 行空白(空带 1 + 状态行 1 +
/// 内边距 1)。pi 上游无此问题的做法是全帧差分重绘,属渲染管线级重构。
pub const STREAM_PREVIEW_ROWS: usize = 1;
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
        TranscriptItem::Assistant { markdown } => {
            // AI 输出卡片:markdown 渲染进彩色渐变边框(宽度让出边框内边距)
            let inner = width.saturating_sub(4).max(1);
            let rendered = assistant_markdown(markdown, theme, inner);
            tool_card::box_around(rendered, width, theme)
        }
        TranscriptItem::Thinking { text } => {
            thinking_block(text, theme, width, expanded)
        }
        TranscriptItem::ToolCall { name, args, status } => {
            tool_card::tool_box_top(name, args, (*status).into(), expanded, width, theme)
        }
        TranscriptItem::ToolResult {
            output, is_error, ..
        } => tool_card::tool_box_bottom(output, *is_error, expanded, width, theme),
        TranscriptItem::Bash {
            command,
            output,
            is_error,
        } => tool_card::bash_box(
            command,
            output,
            *is_error,
            expanded,
            width,
            theme,
        ),
    }
}

/// 整个转录的重渲染(ctrl+o 展开/收起后的全文重绘)。间距由转录中的
/// 空行条目表达(与实时提交路径 state.commit_many 一致)。
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

/// thinking 块(✻ 前缀,dim;进转录可回看)。折叠逻辑与工具输出一致:
/// 默认保留前 `COLLAPSED_OUTPUT_ROWS` 行 + 余量提示,ctrl+o 展开全部。
pub fn thinking_block(text: &str, theme: &Theme, width: usize, expanded: bool) -> Vec<UiLine> {
    let style = Style::new().fg(theme.thinking);
    let mark = format!("  {} ", loader::THINKING_MARK);
    let indent = " ".repeat(rpi_tui::display_width(&mark));
    let mark_width = rpi_tui::display_width(&mark);
    let inner = width.max(mark_width + 1) - mark_width;
    let mut rows: Vec<UiLine> = Vec::new();
    for raw in text.lines().filter(|raw| !raw.trim().is_empty()) {
        for (i, piece) in rpi_tui::wrap_to_width(raw, inner).into_iter().enumerate() {
            let prefix = if i == 0 { mark.clone() } else { indent.clone() };
            rows.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(piece, style),
            ]));
        }
    }
    if expanded || rows.len() <= tool_card::COLLAPSED_OUTPUT_ROWS {
        return rows;
    }
    let more = rows.len() - tool_card::COLLAPSED_OUTPUT_ROWS;
    rows.truncate(tool_card::COLLAPSED_OUTPUT_ROWS);
    rows.push(Line::from(Span::styled(
        format!("{indent}… +{more} lines (ctrl+o to expand)"),
        Style::new().fg(theme.dim),
    )));
    rows
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
    } else if preview_cap > 0 && !state.stream_text.is_empty() {
        let wrapped = rpi_tui::wrap_to_width(&state.stream_text, width);
        // 预览恒为固定尾部窗口(不随 ctrl+o 展开态变化):展开态只作用于
        // 定稿转录,避免展开后视口逐增量改高引发闪烁
        let skip = wrapped.len().saturating_sub(preview_cap);
        for row in wrapped.into_iter().skip(skip) {
            preview.push(Line::from(Span::styled(
                row,
                Style::new().fg(theme.assistant_text),
            )));
        }
    } else if preview_cap > 0
        && state
            .pending_thinking
            .as_ref()
            .is_some_and(|t| !t.trim().is_empty())
    {
        if let Some(text) = state.pending_thinking.as_ref() {
            // 与 bash 折叠逻辑一致:尾部 4 行 + 余量提示(与展开态无关)
            let rows: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let collapsed = rows.len() > tool_card::COLLAPSED_OUTPUT_ROWS;
            let start = if collapsed {
                rows.len() - tool_card::COLLAPSED_OUTPUT_ROWS
            } else {
                0
            };
            for row in &rows[start..] {
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
            if collapsed {
                let more = rows.len() - tool_card::COLLAPSED_OUTPUT_ROWS;
                preview.push(Line::from(Span::styled(
                    format!("  … +{more} lines (ctrl+o to expand)"),
                    Style::new().fg(theme.dim),
                )));
            }
        }
    } else if preview_cap > 0 && partial.is_some() {
        if let Some(args) = partial.and_then(toolcall_args_preview) {
            preview.push(Line::from(Span::styled(
                format!("⚙ {args}"),
                Style::new().fg(theme.dim),
            )));
        }
    }
    if state.select.is_none() {
        // 预览恒为尾部窗口:超出 cap 时从头部丢弃(thinking 折叠尾迹可能
        // 超过 cap,不能让视口高度随内容回涨——那会在回合中段重新产生
        // 高度变化与收缩空带)
        if preview.len() > preview_cap {
            preview.drain(..preview.len() - preview_cap);
        }
        while preview.len() < preview_cap {
            preview.push(Line::raw(""));
        }
    }
    lines.extend(preview);

    // 状态行(busy 时一行带 shimmer 渐变;idle 空占一行)。无预览预留,
    // busy/idle 视口高度恒同,无需空隙行。
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
        expanded: state.expanded,
    };
    lines.extend(rpi_tui::footer::lines(&footer, width, theme));

    let height = lines.len() as u16;
    ViewportFrame {
        lines,
        cursor,
        height,
    }
}

/// 状态行(Codex 风格:busy 时一行带彩色渐变与 `esc to interrupt` 提示;
/// idle 时空占一行,保证视口高度不随 busy↔idle 切换抖动)。
/// 等待态文字逐字符在彩色光谱上取色,色相随位置渐变、随 spinner 拍数向右扫动。
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
    // codex 式渐变:整行文字在彩色光谱上取色,色相随位置渐变、波峰随
    // spinner 拍数向右扫动(真彩色主题;ANSI 兜底退化为状态色单色)
    let anchors = theme.gradient_anchors();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(1) as f32;
    let phase = (state.spin % 24) as f32 / 24.0;
    let spans: Vec<Span<'static>> = chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let color = rpi_tui::theme::gradient_at(
                &anchors,
                i as f32 / n + phase,
            )
            .unwrap_or(color);
            Span::styled(c.to_string(), Style::new().fg(color))
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
    use crate::modes::interactive::state::{InteractiveState, ToolStatus};
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
    fn thinking_block_prefixes_mark_and_collapses() {
        let lines = thinking_block("step\nstep2", &theme(), 80, false);
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).contains(loader::THINKING_MARK));
        assert!(line_text(&lines[0]).contains("step"));

        // 超过 4 行折叠 + 提示;ctrl+o 展开全量
        let text = (1..=6).map(|i| format!("step{i}")).collect::<Vec<_>>().join("\n");
        let collapsed = thinking_block(&text, &theme(), 80, false);
        assert_eq!(collapsed.len(), 5, "{collapsed:?}"); // 4 行 + 提示
        assert!(line_text(&collapsed[4]).contains("+2 lines"));
        assert!(line_text(&collapsed[4]).contains("ctrl+o"));
        let expanded = thinking_block(&text, &theme(), 80, true);
        assert_eq!(expanded.len(), 6);
        assert!(line_text(&expanded[5]).contains("step6"));
    }

    #[test]
    fn transcript_groups_stack_without_separators() {
        let items = vec![
            TranscriptItem::User {
                content: "hi".into(),
            },
            TranscriptItem::Thinking {
                text: "hmm".into(),
            },
            TranscriptItem::Assistant {
                markdown: "yo".into(),
            },
            TranscriptItem::ToolCall {
                name: "bash".into(),
                args: String::new(),
                status: ToolStatus::Success,
            },
            TranscriptItem::ToolResult {
                output: "ok".into(),
                is_error: false,
            },
            TranscriptItem::Assistant {
                markdown: "done".into(),
            },
        ];
        let lines = render_transcript(&items, &theme(), 40, false);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // 分割线已移除:任何行都不再以 ─ 开头的全宽横线形式出现
        // (assistant markdown 若带代码块,其框有 `╭── label` 标签形态)
        assert!(
            !texts.iter().any(|t| t.starts_with("───")),
            "不应再有分割线: {texts:?}"
        );
        // assistant 正文包进渐变边框(│ yo … │)
        assert!(texts.iter().any(|t| t.contains("│ yo")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("│ done")), "{texts:?}");
    }

    #[test]
    fn viewport_thinking_preview_shows_tail_with_hint() {
        let mut st = state();
        st.pending_thinking = Some((1..=6).map(|i| format!("step{i}")).collect::<Vec<_>>().join("\n"));
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("step6")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("step1")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("+2 lines")), "{texts:?}");
        // 预览恒为折叠尾部窗口(与 ctrl+o 展开态无关,展开只作用于定稿转录)
        st.expanded = true;
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(!texts.iter().any(|t| t.contains("step1")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("ctrl+o")), "{texts:?}");
    }

    #[test]
    fn viewport_layout_shape() {
        let mut st = state();
        st.editor.set_text("hi");
        let frame = viewport(&st, None, 8, 6, 8);
        // 预览 8(恒占)+ 状态 1(空闲无空隙行)+ 编辑区(上下内边距 2 +
        // 编辑行 1)+ footer 3 = 15
        assert_eq!(
            frame.lines.len(),
            15,
            "{:?}",
            frame.lines.iter().map(line_text).collect::<Vec<_>>()
        );
        assert_eq!(frame.height, 15);
        // 光标在编辑器行(行 10 = 预览 8 + 状态 1 + 顶部内边距),列 = 2 + 2 = 4
        assert_eq!(frame.cursor, Some((4, 10)));
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts[10].starts_with("❯ hi"), "{texts:?}");
        // idle 时预览与状态行均为空占位
        for row in &texts[..9] {
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
        // 空闲:预览 4 + 状态 1 + 编辑区 3 + footer 3(无空隙行)
        assert_eq!(frame.height, 4 + 1 + 3 + 3);
        for row in &texts[..4] {
            assert!(row.trim().is_empty(), "预览空位应为空行: {texts:?}");
        }
        // 内容增长但未超 cap:预览区行数不变;空隙行已移除,busy/idle 同构
        let mut st = state();
        st.status = Status::Thinking;
        st.stream_text = "line1\nline2".into();
        let frame = viewport(&st, None, 4, 6, 8);
        assert_eq!(frame.height, 4 + 1 + 3 + 3);
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
        // idle:状态行空占位(无预览、无空隙行)
        assert_eq!(viewport(&state, None, 0, 6, 8).lines.len(), 7);
        assert!(line_text(&viewport(&state, None, 0, 6, 8).lines[0]).trim().is_empty());
        state.status = Status::Thinking;
        state.spin = 25; // 25 * 120ms = 3s
        let frame = viewport(&state, None, 0, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        // 无空隙行(busy/idle 同构)→ 状态行在 index 0
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
        // 状态行空占位 → 顶部内边距 idx 1,编辑器首行在 index 2
        let text = line_text(&frame.lines[2]);
        assert!(text.starts_with("❯ "), "{text}");
        assert!(text.contains("Ask rpi to do anything"), "{text}");
    }

    #[test]
    fn viewport_editor_row_has_full_width_background() {
        let mut st = state();
        st.editor.set_text("hi");
        let frame = viewport(&st, None, 0, 6, 8);
        // 状态行空占位 → 顶部内边距 idx 1,编辑行 idx 2
        let row = &frame.lines[2];
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
