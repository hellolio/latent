//! 视图渲染:状态(InteractiveState + 转录模型)→ `Vec<Line>`。
//! 全部纯函数;终端 I/O 只发生在 mod.rs 的事件循环里(commit/draw)。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use latent_tui::footer::FooterData;
use latent_tui::{loader, markdown, tool_card, Theme, UiLine};

use super::state::{InteractiveState, Status, ThinkingFenceCache, TranscriptItem};

/// 流式输出实时预览的兜底行数(仅极端收缩场景;流式内容活跃时
/// build_frame 按屏高给全量预算)。全帧差分渲染下尾部高度可自由
/// 变化,无需预留行数守恒。
pub const STREAM_PREVIEW_ROWS: usize = 4;
/// 编辑器最多展示的视觉行数(pi 编辑器同样封顶)。
pub const MAX_EDITOR_ROWS: usize = 6;
/// 流式内容(正文/thinking/工具实时输出)全量滚动时预览区之外保留的
/// 行数(状态行 1 + 间隔 2 + 编辑器区含上下内边距 8 + footer 2);
/// build_frame 据此从屏高推算预览预算。
pub const LIVE_PREVIEW_RESERVED: usize = 13;
/// 工具实时输出并入滚动视口的全量行数兜底(上游 partial 已是滚动尾窗
/// 快照,此处仅防御极端尺寸)。
const TOOL_OUTPUT_FULL_ROWS: usize = 1000;

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
            // 模型消息裸渲染:无外框无背景色(与工具卡片背景块形成对比)
            assistant_markdown(markdown, theme, width)
        }
        TranscriptItem::Thinking { text } => {
            thinking_block(text, theme, width, expanded)
        }
        TranscriptItem::Plan { markdown } => {
            // 计划模式产出的 <proposed_plan> 块:紫色背景卡片 + 标题 + 引导行
            // (13 文档 §8.3/§10.4;原始文本仍按普通消息入转录)
            let rendered = assistant_markdown(markdown, theme, width);
            // 背景块上下各一行同色内边距(与用户消息块/工具卡片一致)
            let mut body = rendered;
            body.insert(0, Line::raw(""));
            body.push(Line::raw(""));
            let mut card = vec![latent_tui::UiLine::from(ratatui::text::Line::from(
                ratatui::text::Span::styled(
                    "实施计划(计划模式产出)".to_string(),
                    Style::new().fg(theme.warning).add_modifier(Modifier::BOLD),
                ),
            ))];
            card.extend(tool_card::bg_block(body, width, theme.plan_bg));
            card.push(latent_tui::UiLine::from(ratatui::text::Line::from(
                ratatui::text::Span::styled(
                    "确认后 /mode confirm 并让模型开始实现".to_string(),
                    Style::new().fg(theme.dim),
                ),
            )));
            card
        }
        TranscriptItem::ToolCall { name, args, status } => {
            // 已定稿转录行帧间不可变:pending 态(异步 subagent 运行中)用
            // 静态 ⏺,旋转动画只存在于实时预览(见 tool_preview_parts)
            tool_card::tool_box_top(name, args, (*status).into(), width, theme, None)
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
    for raw in latent_tui::wrap_to_width(content, width.max(1)) {
        let pad = width.saturating_sub(latent_tui::display_width(&raw));
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
        .with_highlight(latent_tui::Highlighter::shared(theme.is_dark))
        .render(source, width)
}

/// thinking 块(✻ 前缀,dim;进转录可回看,围栏代码块以高亮代码盒呈现)。
/// 折叠逻辑与工具输出一致:默认保留前 `COLLAPSED_OUTPUT_ROWS` 行 + 余量
/// 提示,ctrl+o 展开全部。
pub fn thinking_block(text: &str, theme: &Theme, width: usize, expanded: bool) -> Vec<UiLine> {
    let mut rows = thinking_rows(text, theme, width, None, true);
    if expanded || rows.len() <= tool_card::COLLAPSED_OUTPUT_ROWS {
        return rows;
    }
    let mark = format!("  {} ", loader::THINKING_MARK);
    let more = rows.len() - tool_card::COLLAPSED_OUTPUT_ROWS;
    rows.truncate(tool_card::COLLAPSED_OUTPUT_ROWS);
    rows.push(Line::from(Span::styled(
        format!("{}… +{more} lines (ctrl+o to expand)", " ".repeat(latent_tui::display_width(&mark))),
        Style::new().fg(theme.dim),
    )));
    rows
}

/// thinking 内容渲染(流式预览与定稿转录共用):prose 行保持思考灰
/// (流式逐行 ✻ 前缀 + 截断;定稿折行,首行 ✻、续行缩进),围栏代码块
/// 渲染成 2 空格缩进的 syntect 高亮代码盒(与正文代码块同款,盒内空行
/// 保留、不带 ✻)。`fences` 提供时已闭合围栏走增量缓存(流式逐帧调用);
/// `prose_wrap` = 定稿路径(折行),否则流式截断。
pub fn thinking_rows(
    text: &str,
    theme: &Theme,
    width: usize,
    mut fences: Option<&mut ThinkingFenceCache>,
    prose_wrap: bool,
) -> Vec<UiLine> {
    let style = Style::new().fg(theme.thinking);
    let mark = format!("  {} ", loader::THINKING_MARK);
    let mark_width = latent_tui::display_width(&mark);
    let highlighter = latent_tui::Highlighter::shared(theme.is_dark);
    let mut rows: Vec<UiLine> = Vec::new();
    // 围栏段整体收集渲染成代码盒(判定与 Markdown::render 同源:trim 后
    // ``` 开头;空行仅在围栏内保留,prose 空行跳过与既有行为一致)
    let mut fence: Option<(String, Vec<String>, usize)> = None; // (lang, 内容, 字节起点)
    let mut offset = 0usize;
    for piece in text.split_inclusive('\n') {
        let raw = piece.strip_suffix('\n').unwrap_or(piece);
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let line_start = offset;
        offset += piece.len();
        if fence.is_some() {
            if line.trim_start().starts_with("```") {
                let (lang, code, start) = fence.take().unwrap();
                rows.extend(fence_box_rows(
                    &lang,
                    &code,
                    start,
                    theme,
                    highlighter,
                    width,
                    fences.as_deref_mut(),
                ));
            } else {
                fence.as_mut().unwrap().1.push(line.to_string());
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            fence = Some((line.trim_start()[3..].trim().to_string(), Vec::new(), line_start));
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if prose_wrap {
            let inner = width.max(mark_width + 1) - mark_width;
            for (i, piece) in latent_tui::wrap_to_width(line, inner).into_iter().enumerate() {
                let prefix = if i == 0 {
                    mark.clone()
                } else {
                    " ".repeat(mark_width)
                };
                rows.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(piece, style),
                ]));
            }
        } else {
            rows.push(Line::from(vec![
                Span::styled(mark.clone(), style),
                Span::styled(truncate_plain(line, width.saturating_sub(4)), style),
            ]));
        }
    }
    // 未闭合围栏照常成盒(与 Markdown::render 的尾部行为一致)
    if let Some((lang, code, start)) = fence {
        rows.extend(fence_box_rows(&lang, &code, start, theme, highlighter, width, fences));
    }
    rows
}

/// 围栏代码盒(2 空格缩进对齐 ✻);`fences` 提供时走增量缓存(流式路径)。
fn fence_box_rows(
    lang: &str,
    code: &[String],
    byte_start: usize,
    theme: &Theme,
    highlighter: &'static latent_tui::Highlighter,
    width: usize,
    fences: Option<&mut ThinkingFenceCache>,
) -> Vec<UiLine> {
    let source = code.join("\n");
    let box_width = width.saturating_sub(2).max(1); // 左侧 2 空格缩进
    let rendered = match fences {
        Some(cache) => cache.rows(byte_start, &source, Some(lang), theme, highlighter, box_width),
        None => {
            latent_tui::markdown::code_block(&source, Some(lang), theme, Some(highlighter), box_width)
        }
    };
    rendered
        .into_iter()
        .map(|row| {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(row.spans);
            Line::from(spans)
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
    let mut out = latent_tui::header_view::banner(version, expanded, width, theme);
    out.push(Line::raw(""));
    out
}

/// 底部视口帧:预览区(流式尾部/thinking 尾部)+ 状态行 + 补全弹窗 +
/// 编辑器区(带背景色,无边框)+ footer(Codex CLI 布局)。
pub struct ViewportFrame {
    pub lines: Vec<UiLine>,
    /// `lines` 头部属于预览尾窗的行数(未折行计数;0 = 无可滚预览或模态
    /// UI)。全屏非 follow 布局据此把预览从钉底尾部剔除,并入滚动视口。
    pub preview_window: usize,
    /// 预览全量折行行数(≥ preview_window;follow 帧滚动数学的依据)。
    pub preview_full_len: usize,
    /// 预览全量行(内容顺序;仅非 follow 帧构建供滚动视口渲染,follow
    /// 帧为空省 O(n) 重建)。
    pub scroll_extra: Vec<UiLine>,
    /// 光标相对视口左上角的 (列, 行);None = 隐藏
    pub cursor: Option<(u16, u16)>,
    pub height: u16,
}

/// 组装尾部帧。`preview_cap`/`editor_cap`/`popup_cap` 限制流式预览、
/// 编辑器与补全弹窗行数(终端过矮时由调用方收缩重算)。
/// 全帧差分渲染下尾部高度可逐帧自由变化:预览区不再占位补空。
pub fn viewport(
    state: &InteractiveState,
    partial: Option<&latent_ai::AssistantMessage>,
    preview_cap: usize,
    editor_cap: usize,
    popup_cap: usize,
) -> ViewportFrame {
    let theme = &state.theme;
    let width = state.width.max(1);
    let mut lines: Vec<UiLine> = Vec::new();
    let mut cursor: Option<(u16, u16)> = None;

    // 0. 「添加模型」表单信息面板:圆角外框(与选择器面板同款)内含标题 +
    //    字段清单(全部字段恒显示,已配置的显示内容、未配置的留空)+
    //    当前问题行;选择列表/编辑器作为"具体配置选项"紧随其后。api 协议
    //    选择的选择器 prompt 即问题行,激活时跳过问题行避免重复。
    if let Some(form) = &state.model_form {
        let mut content: Vec<UiLine> = vec![Line::from(Span::styled(
            "模型信息",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ))];
        // 键名一律灰色;值按字段着色区分(provider/api/baseUrl/apiKey/model 各一色)
        let value_colors = [
            theme.assistant_text,
            theme.md_code,
            theme.md_link,
            theme.success,
            theme.warning,
        ];
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
        for ((name, value), color) in form.fields().into_iter().zip(value_colors) {
            if spans.len() > 1 {
                spans.push(Span::styled("  ", Style::new().fg(theme.dim)));
            }
            spans.push(Span::styled(
                format!("{name}="),
                Style::new().fg(theme.dim),
            ));
            if let Some(value) = value {
                spans.push(Span::styled(value.to_string(), Style::new().fg(color)));
            }
        }
        content.push(Line::from(spans));
        if state.select.is_none() {
            // 字段行与问题行之间空一行
            content.push(Line::raw(""));
            content.push(Line::from(Span::styled(
                form.question(),
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            )));
        }
        lines.extend(latent_tui::popup::frame(content, width, theme));
    }

    // 1. 预览区:选择列表 > 正文流式 > thinking > 工具命令卡片 + 实时
    //    输出。流式内容全量滚动(超 preview_cap 取尾窗,cap 由 build_frame
    //    按屏高给出,不受 ctrl+o 影响);命令卡片全显。预览同时是全屏滚动
    //    视口的"活动内容":非 follow 帧把全量行带给 TuiApp(scroll_extra,
    //    随滚动并入视口),follow 帧省构建只报行数。
    let mut preview: Vec<UiLine> = Vec::new();
    let mut scroll_extra: Vec<UiLine> = Vec::new();
    let mut preview_window = 0usize;
    let mut preview_full_len = 0usize;
    if let Some(select) = &state.select {
        // 圆角外框面板(与 / 命令弹窗同款),与上方已滚走的转录内容视觉
        // 分离;prompt 按行拆分(审批弹窗 prompt 含 \n,单行直排会破坏帧
        // 行对齐),超宽行折到内宽
        let prompt_style = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
        let inner_w = width.saturating_sub(4).max(1);
        let mut content: Vec<UiLine> = Vec::new();
        for row in select.prompt.lines() {
            let line = Line::from(Span::styled(row.to_string(), prompt_style));
            content.extend(latent_tui::text::wrap_line(&line, inner_w));
        }
        content.extend(latent_tui::SelectList::render(&select.list, inner_w, theme));
        preview.extend(latent_tui::popup::frame(content, width, theme));
    } else if !state.stream_text.is_empty() {
        // 全量 Markdown 渲染读自增量缓存(append-only 下稳定块只渲染一次,
        // 逐帧仅重渲未稳定尾段,长回复不再逐帧 O(n) 重算 + 全量 syntect)。
        // 尾窗 = 全量行尾切 preview_cap,预览恒为固定尾部窗口(不随 ctrl+o
        // 展开态变化:展开态只作用于定稿转录)
        let md = markdown::Markdown::new(theme)
            .with_highlight(latent_tui::Highlighter::shared(theme.is_dark));
        {
            let mut cache = state.stream_markdown.borrow_mut();
            let full = cache.update(&state.stream_text, width, &md);
            preview_full_len = full.len();
            if !state.following {
                scroll_extra.extend(full.iter().cloned());
            }
            let skip = full.len().saturating_sub(preview_cap);
            preview.extend(full.iter().skip(skip).cloned());
        }
        preview_window = preview.len();
    } else if state
        .pending_thinking
        .as_ref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        if let Some(text) = state.pending_thinking.as_ref() {
            // 流式中全量滚动显示:prose 行保持 ✻ 前缀,围栏代码块实时 syntect
            // 高亮(已闭合围栏走增量缓存);≤ preview_cap 行全显,超出取尾部
            // 窗口;完成定稿后由 thinking_block 折叠为 4 行 + 余量提示
            let mut rows = thinking_rows(
                text,
                theme,
                width,
                Some(&mut state.thinking_fences.borrow_mut()),
                false,
            );
            preview_full_len = rows.len();
            if !state.following {
                scroll_extra.extend(rows.iter().cloned());
            }
            let skip = rows.len().saturating_sub(preview_cap);
            preview.extend(rows.iter().skip(skip).cloned());
            preview_window = preview.len();
        }
    } else {
        // 工具命令卡片 + 实时输出:命令全显,输出尾窗 ≤ preview_cap
        let (win, extra, full_len) =
            tool_preview_parts(state, partial, preview_cap, state.following, width);
        preview.extend(win);
        preview_window = preview.len();
        scroll_extra = extra;
        preview_full_len = full_len;
    }
    // 「添加模型」表单激活时预览保持钉底(模态 UI 不并入滚动视口)
    if state.model_form.is_some() {
        preview_window = 0;
        scroll_extra = Vec::new();
        preview_full_len = 0;
    }
    lines.extend(preview);

    // 状态行:仅 busy 时渲染(spinner 紧贴最近的模型/工具输出下方);
    // idle 不占行,输出→输入框之间恒定两行空行。
    lines.extend(status_line(state));
    // 输出/状态行与编辑器区之间的固定两行间隔(流式中与完成后一致)。
    lines.push(Line::raw(""));
    lines.push(Line::raw(""));

    // 3. 补全弹窗:紧贴编辑器框上方(Codex 布局);斜杠与 `@` 文件弹窗
    //    互斥(state 层让位规则),同槽渲染
    if state.select.is_none() && state.slash_popup.visible() {
        lines.extend(state.slash_popup.render(width, theme, popup_cap));
    } else if state.select.is_none() && state.mention_popup.visible() {
        lines.extend(state.mention_popup.render(width, theme, popup_cap));
    }

    // 4. 编辑器区:无边框,整行铺 user_bg 背景(与已发送用户消息同款,
    //    靠背景色区分输入区;busy 语义由上方状态行承担)
    let view = state
        .editor
        .view(width.saturating_sub(2), editor_cap.max(1));

    // 5. 编辑器内容行(整行铺 user_bg,上下各一行同色空行作内边距,与
    //    用户消息块一致;无前缀符,光标定位即视觉锚点;空输入显示占位文本)
    let bg = theme.user_bg;
    let text_style = Style::new().fg(theme.user_text).bg(bg);
    let pad_row = |mut spans: Vec<Span<'static>>, used: usize| {
        let pad = width.saturating_sub(used);
        spans.push(Span::styled(" ".repeat(pad), text_style));
        Line::from(spans)
    };
    let full_pad = || Line::from(Span::styled(" ".repeat(width.max(1)), text_style));
    lines.push(full_pad());
    let editor_first_row = lines.len();
    if state.editor.is_empty() {
        let placeholder = "Ask latent to do anything";
        lines.push(pad_row(
            vec![Span::styled(
                placeholder.to_string(),
                Style::new().fg(theme.dim).bg(bg),
            )],
            latent_tui::display_width(placeholder),
        ));
    } else {
        for row in &view.rows {
            lines.push(pad_row(
                vec![Span::styled(row.clone(), text_style)],
                latent_tui::display_width(row),
            ));
        }
    }
    lines.push(full_pad());
    if let Some((row, col)) = view.cursor {
        cursor = Some((
            col.min(width.saturating_sub(1)) as u16,
            (editor_first_row + row) as u16,
        ));
    }

    // 6. footer 两行(上=左 cwd + 右 token 段;下=左 agent/模式 + 右模型)
    let footer = FooterData {
        cwd: state.cwd_display.clone(),
        git_branch: state.git_branch.clone(),
        input_tokens: state.usage.total.input,
        output_tokens: state.usage.total.output,
        cache_read: state.usage.total.cache_read,
        cache_write: state.usage.total.cache_write,
        cache_reported: state.usage.cache_ever_reported,
        cost_total: state.usage.total.cost.total,
        context_window: state.context_window,
        context_tokens: state.context_tokens,
        model: state.model_label.clone(),
        thinking: state.thinking_label.clone(),
        auto_compact: state.auto_compact,
        expanded: state.expanded,
        active_agent: state.active_agent.clone(),
        subagent_active: state.subagent_active,
        mode: Some(state.mode_label.clone()),
    };
    lines.extend(latent_tui::footer::lines(&footer, width, theme));

    let height = lines.len() as u16;
    ViewportFrame {
        lines,
        preview_window,
        preview_full_len,
        scroll_extra,
        cursor,
        height,
    }
}

/// 状态行(Codex 风格:busy 时一行带彩色渐变与 `esc to interrupt` 提示,
/// 紧贴最近的模型/工具输出下方;idle 不占行——输出→输入框的间隔恒为
/// 两行空行)。等待态文字逐字符在彩色光谱上取色,色相随位置渐变、
/// 随 spinner 拍数向右扫动。
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
    // 正在接收正文增量时,状态行右侧追加输出提示(纯思考/工具执行不显示)
    let text = if state.stream_text.is_empty() {
        text
    } else {
        format!("{text} · writing…")
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
            let color = latent_tui::theme::gradient_at(
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
    latent_tui::truncate_to_width(text, width.max(1)).0
}

/// 工具命令实时预览部件:`(尾部窗口行, 全量行, 全量行数)`。命令卡片
/// 始终完整折行显示,不受预览行数上限约束;执行中工具的实时输出
/// (pending_tool_output)全量滚动,窗口超 preview_cap 取尾部窗口
/// (结果到达后由 ToolResult 折叠)。全量行仅非 follow 帧构建(随滚动
/// 并入视口),行数受 TOOL_OUTPUT_FULL_ROWS 兜底 —— 上游 partial 本身
/// 是滚动尾窗快照,此处仅防御极端尺寸。
fn tool_preview_parts(
    state: &InteractiveState,
    partial: Option<&latent_ai::AssistantMessage>,
    preview_cap: usize,
    following: bool,
    width: usize,
) -> (Vec<UiLine>, Vec<UiLine>, usize) {
    let mut cards: Vec<UiLine> = Vec::new();
    // 执行中的工具卡片用旋转字符替代静态 ⏺:预览区逐帧重渲染,动画生效
    // (spinner 帧 = 状态行同源拍数)
    let spinner = Some(loader::frame(state.spin));
    if let Some(partial) = partial {
        for block in &partial.content {
            if let latent_ai::ContentBlock::ToolCall {
                name, arguments, ..
            } = block
            {
                cards.extend(tool_card::tool_box_top(
                    name,
                    &arguments.to_string(),
                    latent_tui::tool_card::ToolStatus::Pending,
                    width,
                    &state.theme,
                    spinner,
                ));
            }
        }
    }
    if cards.is_empty() {
        for (_, name, args) in &state.pending_tools {
            cards.extend(tool_card::tool_box_top(
                name,
                args,
                latent_tui::tool_card::ToolStatus::Pending,
                width,
                &state.theme,
                spinner,
            ));
        }
    }
    let output_rows: Vec<String> = match &state.pending_tool_output {
        Some((_, tail)) => tail
            .lines()
            .filter(|row| !row.trim().is_empty())
            .map(|row| format!("  {}", truncate_plain(row, width.saturating_sub(2))))
            .collect(),
        None => Vec::new(),
    };
    let full_len = cards.len() + output_rows.len().min(TOOL_OUTPUT_FULL_ROWS);
    let mut extra: Vec<UiLine> = Vec::new();
    if following {
        // follow 帧:全量行不构建,行数已随 full_len 上报
        return (cards, extra, full_len);
    }
    extra.extend(cards.iter().cloned());
    let skip = output_rows.len().saturating_sub(TOOL_OUTPUT_FULL_ROWS);
    extra.extend(output_rows[skip..].iter().map(|row| {
        Line::from(Span::styled(row.clone(), Style::new().fg(state.theme.dim)))
    }));
    // 尾窗:卡片 + 输出尾部 ≤ preview_cap(与卡片数无关,维持既有窗口
    // 语义;cap 为 0 时窗口为空,全量行仍随 extra 上报供滚动视口使用)
    let mut window: Vec<UiLine> = Vec::new();
    if preview_cap > 0 {
        let start = output_rows.len().saturating_sub(preview_cap);
        window.extend(cards.iter().cloned());
        window.extend(output_rows[start..].iter().map(|row| {
            Line::from(Span::styled(row.clone(), Style::new().fg(state.theme.dim)))
        }));
    }
    (window, extra, full_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::state::{
        InteractiveState, ModelForm, SelectKind, SelectRequest, ToolStatus, TranscriptItem,
    };
    use latent_tui::text::line_text;

    fn theme() -> Theme {
        Theme::dark_ansi()
    }

    #[test]
    fn plan_item_renders_as_bordered_card_with_hint() {
        let item = TranscriptItem::Plan {
            markdown: "- step 1".into(),
        };
        let text = render_item(&item, &theme(), 80, false)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("实施计划"), "{text}");
        assert!(text.contains("step 1"), "{text}");
        assert!(text.contains("/mode confirm"), "应带切换引导行: {text}");
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
        assert_eq!(latent_tui::display_width(&text), 40, "{text:?}");
        assert!(text.starts_with("hello"));
        // 三行(含内边距行)背景完全一致:各为单一 span,带 bg 色
        for line in &lines {
            assert_eq!(line.spans.len(), 1);
            assert_eq!(line.spans[0].style.bg, Some(theme().user_bg));
            assert_eq!(latent_tui::display_width(&line_text(line)), 40);
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
        // assistant 正文裸渲染:无边框内容行(│ yo … │ 已随外框移除)
        assert!(texts.iter().any(|t| t.contains("yo")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("done")), "{texts:?}");
        // 无任何消息行再带 │ 边框(工具卡片已改为背景色块)
        assert!(
            !texts.iter().any(|t| t.trim_start().starts_with('│')),
            "{texts:?}"
        );
    }

    #[test]
    fn viewport_busy_keeps_cursor_in_editor() {
        // 光标常驻输入框:busy 且编辑器为空也不停靠底部等待区
        let mut st = state();
        st.status = Status::Thinking;
        st.stream_text = "partial".into();
        let frame = viewport(&st, None, 4, 6, 8);
        assert!(frame.cursor.is_some(), "busy 时光标应在编辑器内");
        assert_ne!(
            frame.cursor,
            Some((0, frame.height - 1)),
            "不应停靠在最底行"
        );
        // busy 中用户开始输入:光标仍在编辑器
        st.editor.set_text("x");
        let frame = viewport(&st, None, 4, 6, 8);
        assert!(frame.cursor.is_some());
        assert_ne!(frame.cursor, Some((0, frame.height - 1)));
        // idle:光标回到编辑器
        st.status = Status::Idle;
        let frame = viewport(&st, None, 4, 6, 8);
        assert!(frame.cursor.is_some());
        assert_ne!(frame.cursor, Some((0, frame.height - 1)));
    }

    #[test]
    fn viewport_thinking_preview_full_scroll_then_tail_window() {
        let mut st = state();
        st.pending_thinking = Some((1..=6).map(|i| format!("step{i}")).collect::<Vec<_>>().join("\n"));
        // cap 充足:6 行全量显示,无折叠提示行(流式中不再显示 +N lines)
        let frame = viewport(&st, None, 10, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        for i in 1..=6 {
            assert!(texts.iter().any(|t| t.contains(&format!("step{i}"))), "{texts:?}");
        }
        assert!(!texts.iter().any(|t| t.contains("ctrl+o")), "{texts:?}");
        // cap 不足:取尾部窗口,仍无提示行(与 ctrl+o 展开态无关,展开只作
        // 用于定稿转录)
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("step6")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("step1") || t.contains("step2")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("ctrl+o")), "{texts:?}");
        st.expanded = true;
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(!texts.iter().any(|t| t.contains("step1")), "{texts:?}");
    }

    #[test]
    fn thinking_preview_renders_fenced_code_as_highlighted_box() {
        let mut st = state();
        st.status = Status::Thinking;
        st.pending_thinking = Some(
            "Before the code\n```rust\nlet a = 1;\n\nlet b = 2;\n```\nAfter the code".into(),
        );
        let frame = viewport(&st, None, 20, 6, 8);
        let rows = &frame.lines[..frame.preview_window];
        let texts: Vec<String> = rows.iter().map(line_text).collect();
        // prose 行保持 ✻ 前缀;围栏渲染为代码盒(语言标注行)
        assert!(
            texts.iter().any(|t| t.contains(loader::THINKING_MARK) && t.contains("Before the code")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("╭── rust")), "{texts:?}");
        // 代码盒行不带 ✻(2 空格缩进);盒内空行保留(两行代码夹一行空行)
        let box_rows: Vec<&String> = texts.iter().filter(|t| t.starts_with("  │")).collect();
        assert_eq!(box_rows.len(), 3, "{texts:?}");
        assert!(box_rows.iter().all(|t| !t.contains(loader::THINKING_MARK)), "{box_rows:?}");
        assert!(box_rows.iter().any(|t| t.contains("let a = 1;")), "{box_rows:?}");
        assert!(box_rows[1].trim() == "│", "空行应保留在盒内: {box_rows:?}");
        // 代码行带语法高亮(≥2 种前景色);prose 尾行在盒后
        let code_line = rows.iter().find(|l| line_text(l).contains("let a = 1;")).unwrap();
        let colored: std::collections::HashSet<_> =
            code_line.spans.iter().filter_map(|s| s.style.fg).collect();
        assert!(colored.len() > 1, "rust 代码应有语法高亮: {colored:?}");
        assert!(texts.iter().any(|t| t.contains("After the code")), "{texts:?}");
        // 二次渲染走缓存:行内容不变
        let again = thinking_rows(
            st.pending_thinking.as_ref().unwrap(),
            &st.theme,
            st.width,
            Some(&mut st.thinking_fences.borrow_mut()),
            false,
        );
        assert_eq!(
            again.iter().map(line_text).collect::<Vec<_>>(),
            rows.iter().map(line_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn thinking_rows_renders_unclosed_fence_while_streaming() {
        let t = theme();
        let rows = thinking_rows("```rust\nlet a = 1;", &t, 80, None, false);
        let texts: Vec<String> = rows.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("╭── rust")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("let a = 1;")), "{texts:?}");
    }

    #[test]
    fn thinking_block_boxes_code_and_keeps_prose_mark() {
        let t = theme();
        let lines = thinking_block("a\n```rust\nlet x = 1;\n```\nb", &t, 80, true);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts[0].contains(loader::THINKING_MARK), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("╭── rust")), "{texts:?}");
        assert!(
            texts.iter().filter(|t| t.contains("let x")).all(|t| !t.contains(loader::THINKING_MARK)),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains(" b") || t.ends_with('b')), "{texts:?}");
    }

    #[test]
    fn viewport_stream_renders_markdown_code_block_live() {
        let mut st = state();
        st.status = Status::Thinking;
        st.stream_text = "回答正文\n```rust\nlet a = 1;\n```".into();
        let frame = viewport(&st, None, 10, 6, 8);
        let rows = &frame.lines[..frame.preview_window];
        let texts: Vec<String> = rows.iter().map(line_text).collect();
        // 正文流式中代码块实时渲染为高亮盒(而非纯文本行)
        assert!(texts.iter().any(|t| t.contains("回答正文")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("╭── rust")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("│ let a = 1;")), "{texts:?}");
        let code_line = rows.iter().find(|l| line_text(l).contains("let a = 1;")).unwrap();
        let colored: std::collections::HashSet<_> =
            code_line.spans.iter().filter_map(|s| s.style.fg).collect();
        assert!(colored.len() > 1, "代码块应带语法高亮: {colored:?}");
    }

    #[test]
    fn viewport_tool_live_output_full_scroll_then_tail_window() {
        let mut st = state();
        st.status = Status::Tool("bash".into());
        st.pending_tools = vec![("t1".into(), "bash".into(), r#"{"command":"ls"}"#.into())];
        st.pending_tool_output = Some((
            "t1".into(),
            (1..=6).map(|i| format!("out{i}")).collect::<Vec<_>>().join("\n"),
        ));
        // cap 充足:命令卡片 + 输出全量
        let frame = viewport(&st, None, 10, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("ls")), "命令卡片: {texts:?}");
        for i in 1..=6 {
            assert!(texts.iter().any(|t| t.contains(&format!("out{i}"))), "{texts:?}");
        }
        // cap 不足:输出取尾部窗口,命令卡片仍全显
        let frame = viewport(&st, None, 3, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("ls")), "命令卡片: {texts:?}");
        assert!(texts.iter().any(|t| t.contains("out6")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("out1")), "{texts:?}");
        // 无输出的工具不渲染输出段
        st.pending_tool_output = None;
        let frame = viewport(&st, None, 10, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(!texts.iter().any(|t| t.contains("out")), "{texts:?}");
    }

    #[test]
    fn viewport_stream_scroll_context_window_vs_full() {
        let mut st = state();
        st.status = Status::Thinking;
        st.stream_text = (1..=9)
            .map(|i| format!("stream{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        // follow 帧:尾窗 3 行,全量行不构建(只报行数,滚动数学用)
        st.following = true;
        let frame = viewport(&st, None, 3, 6, 8);
        assert_eq!(frame.preview_window, 3);
        assert_eq!(frame.preview_full_len, 9);
        assert!(frame.scroll_extra.is_empty());
        // 非 follow 帧:全量行随帧携带(上滚后并入滚动视口)
        st.following = false;
        let frame = viewport(&st, None, 3, 6, 8);
        assert_eq!(frame.preview_window, 3);
        assert_eq!(frame.preview_full_len, 9);
        assert_eq!(frame.scroll_extra.len(), 9);
        let extra: Vec<String> = frame.scroll_extra.iter().map(line_text).collect();
        assert!(extra[0].contains("stream1"), "{extra:?}");
        assert!(extra[8].contains("stream9"), "{extra:?}");
        // 尾窗 = 全量行尾 3 行(follow 与非 follow 显示一致)
        let window: Vec<String> = frame.lines[..frame.preview_window]
            .iter()
            .map(line_text)
            .collect();
        assert!(window[0].contains("stream7"), "{window:?}");
        assert!(window[2].contains("stream9"), "{window:?}");
    }

    #[test]
    fn viewport_modal_ui_keeps_preview_pinned() {
        // 「添加模型」表单激活:流式预览保持钉底,不并入滚动视口
        let mut st = state();
        st.stream_text = "streaming".into();
        st.model_form = Some(ModelForm::new());
        let frame = viewport(&st, None, 4, 6, 8);
        assert_eq!(frame.preview_window, 0);
        assert_eq!(frame.preview_full_len, 0);
        assert!(frame.scroll_extra.is_empty());
        // 选择列表激活(替换预览区):同样无可滚预览
        let mut st = state();
        st.select = Some(SelectRequest {
            prompt: "pick".into(),
            list: latent_tui::SelectList::new(vec!["a".into(), "b".into()]),
            kind: SelectKind::Thinking,
        });
        let frame = viewport(&st, None, 4, 6, 8);
        assert_eq!(frame.preview_window, 0);
        assert!(frame.scroll_extra.is_empty());
    }

    #[test]
    fn viewport_select_renders_framed_panel() {
        // 选择器面板带圆角外框(与 / 命令弹窗同款):prompt 与选项都在框内,
        // 与上方已滚走的转录内容视觉分离
        let mut st = state();
        st.select = Some(SelectRequest {
            prompt: "设置(Enter 切换 · Esc 关闭)".into(),
            list: latent_tui::SelectList::new(vec!["全屏渲染".into(), "常规滚动".into()]),
            kind: SelectKind::Thinking,
        });
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts[0].starts_with('╭'), "{texts:?}");
        assert!(texts[0].ends_with('╮'));
        assert!(texts.iter().any(|t| t.starts_with("│ 设置(Enter")), "{texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("│ ❯ 全屏渲染")), "{texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("│   常规滚动")), "{texts:?}");
        assert!(
            texts.iter().any(|t| t.starts_with("╰──") && t.ends_with('╯')),
            "{texts:?}"
        );
        // 模态不并入滚动视口(边框行也不计入)
        assert_eq!(frame.preview_window, 0);
        assert!(frame.scroll_extra.is_empty());
    }

    #[test]
    fn viewport_select_splits_and_wraps_multiline_prompt() {
        // 审批弹窗 prompt 含 \n:拆成多个框内行,不再单行直排破坏帧行对齐;
        // 超宽行折到内宽(80 宽 → 内宽 76)
        let mut st = state();
        st.select = Some(SelectRequest {
            prompt: format!(
                "审批 bash · 需要批准\nrm -rf /tmp/dir\n{}\n{}",
                "x".repeat(120),
                "y".repeat(200)
            ),
            list: latent_tui::SelectList::new(vec!["批准一次".into()]),
            kind: SelectKind::Thinking,
        });
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.starts_with("│ 审批 bash")), "{texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("│ rm -rf")), "{texts:?}");
        // 120 个 x 折成 2 行、200 个 y 折成 3 行:均为独立框内行
        assert_eq!(texts.iter().filter(|t| t.starts_with("│ xx")).count(), 2, "{texts:?}");
        assert_eq!(texts.iter().filter(|t| t.starts_with("│ yy")).count(), 3, "{texts:?}");
        // 任何框内行都不再含字面换行
        assert!(!texts.iter().any(|t| t.contains('\n')), "{texts:?}");
    }

    #[test]
    fn viewport_model_form_renders_framed_panel() {
        // 「添加模型」表单信息面板同样带圆角外框;选择列表未激活时问题行
        // 也在框内
        let mut st = state();
        st.model_form = Some(ModelForm::new());
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts[0].starts_with('╭'), "{texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("│ 模型信息")), "{texts:?}");
        assert!(
            texts.iter().any(|t| t.starts_with("│ 添加模型 · 写入位置")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.starts_with("╰──") && t.ends_with('╯')),
            "{texts:?}"
        );

        // 选择列表激活:问题行让位,呈现「表单面板 + 选择器面板」两块堆叠
        st.select = Some(SelectRequest {
            prompt: "添加模型 · api 协议".into(),
            list: latent_tui::SelectList::new(vec!["openai-completions".into()]),
            kind: SelectKind::ModelApiChoice,
        });
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert_eq!(
            texts.iter().filter(|t| t.starts_with('╭')).count(),
            2,
            "两块独立面板: {texts:?}"
        );
        assert!(!texts.iter().any(|t| t.contains("写入位置")), "{texts:?}");
    }

    #[test]
    fn viewport_layout_shape() {
        let mut st = state();
        st.editor.set_text("hi");
        let frame = viewport(&st, None, 0, 6, 8);
        // 空闲:无预览、无状态行,两行间隔 + 编辑区(上下内边距 2 + 编辑行 1)
        // + footer 2 = 7
        assert_eq!(
            frame.lines.len(),
            7,
            "{:?}",
            frame.lines.iter().map(line_text).collect::<Vec<_>>()
        );
        assert_eq!(frame.height, 7);
        // 光标在编辑器行(idx 3 = 间隔 2 + 顶部内边距),列 = 2(无前缀)
        assert_eq!(frame.cursor, Some((2, 3)));
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts[3].starts_with("hi"), "{texts:?}");
        // 输出→输入框之间恒定两行间隔
        for row in &texts[..2] {
            assert!(row.trim().is_empty(), "间隔应为空行: {texts:?}");
        }
    }

    #[test]
    fn tail_height_follows_content() {
        // 全帧差分下尾部高度逐帧自由变化:空闲最紧,流式按实际行数增长,
        // 不再恒定占位补空
        let st = state();
        // 流式 2 行:预览 2 + 状态 1 + 间隔 2 + 编辑区 3 + footer 2
        assert_eq!(viewport(&st, None, 4, 6, 8).height, 2 + 3 + 2);
                let mut st = state();
        st.status = Status::Thinking;
        st.stream_text = "line1\nline2".into();
        let frame = viewport(&st, None, 4, 6, 8);
        assert_eq!(frame.height, 2 + 1 + 2 + 3 + 2);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(texts[..2].iter().any(|t| t.contains("line2")), "{texts:?}");
        // 超过 4 行取尾部窗口
        let mut st = state();
        st.status = Status::Thinking;
        st.stream_text = (1..=9)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let texts: Vec<String> = viewport(&st, None, 4, 6, 8)
            .lines
            .iter()
            .map(line_text)
            .collect();
        assert!(texts[..4].iter().any(|t| t.contains("line9")), "{texts:?}");
        assert!(!texts[..4].iter().any(|t| t.contains("line1")), "{texts:?}");
    }

    #[test]
    fn viewport_shows_full_tool_command_card() {
        // 工具命令本身完整显示:超长命令折成多行卡片,不受预览 4 行上限
        // 约束;状态行紧贴卡片下方
        let mut st = state();
        st.status = Status::Tool("bash".into());
        let long_cmd = format!(r#"{{"command":"{}"}}"#, "x".repeat(200));
        st.pending_tools = vec![("t1".into(), "bash".into(), long_cmd)];
        let frame = viewport(&st, None, 4, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        assert!(frame.height > 10, "命令应完整折行显示: {texts:?}");
        assert!(
            texts.iter().map(String::as_str).collect::<String>().matches('x').count() >= 200,
            "命令字符不得截断"
        );
        let card_end = texts.iter().rposition(|t| t.contains('x')).unwrap();
        // 卡片底部内边距空行之后紧跟状态行
        assert!(texts[card_end + 2].contains("Running bash"), "{texts:?}");
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
        // idle:状态行 + 固定空行占位(无预览)
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
    fn viewport_status_line_shows_writing_hint_while_streaming() {
        let mut state = state();
        state.status = Status::Thinking;
        // 纯思考:无输出提示
        let texts: Vec<String> = viewport(&state, None, 0, 6, 8)
            .lines
            .iter()
            .map(line_text)
            .collect();
        assert!(!texts[0].contains("writing"), "{texts:?}");
        // 正文流式中:状态行右侧追加 writing… 提示
        state.stream_text = "partial answer".into();
        let texts: Vec<String> = viewport(&state, None, 0, 6, 8)
            .lines
            .iter()
            .map(line_text)
            .collect();
        assert!(texts[0].contains("writing…"), "{texts:?}");
    }

    #[test]
    fn viewport_empty_editor_shows_placeholder() {
        let state = state();
        let frame = viewport(&state, None, 0, 6, 8);
        // 状态行空占位 → 固定空行 idx 1,顶部内边距 idx 2,编辑器首行 idx 3
        let text = line_text(&frame.lines[3]);
        assert!(text.starts_with("Ask latent to do anything"), "{text}");
    }

    #[test]
    fn viewport_editor_row_has_full_width_background() {
        let mut st = state();
        st.editor.set_text("hi");
        let frame = viewport(&st, None, 0, 6, 8);
        // 状态行空占位 → 固定空行 idx 1,顶部内边距 idx 2,编辑行 idx 3
        let row = &frame.lines[3];
        // 内容 + 行尾补齐,两个 span 共用 user_bg(整行无底色缝隙)
        assert_eq!(row.spans.len(), 2, "{row:?}");
        for span in &row.spans {
            assert_eq!(span.style.bg, Some(st.theme.user_bg), "{span:?}");
        }
        assert_eq!(latent_tui::display_width(&line_text(row)), 80);
    }

    #[test]
    fn viewport_shows_slash_popup_above_composer() {
        let mut state = state();
        state.editor.set_text("/theme");
        state.sync_slash_popup();
        assert!(state.slash_popup.visible());
        let frame = viewport(&state, None, 0, 6, 8);
        let texts: Vec<String> = frame.lines.iter().map(line_text).collect();
        // 弹窗(边框 + 1 个 /theme 匹配行)紧贴编辑器区上方
        let popup_top = texts.iter().position(|t| t.starts_with('╭')).unwrap();
        assert!(texts[popup_top + 1].contains("/theme"), "{texts:?}");
        assert!(texts[popup_top + 2].starts_with('╰'), "{texts:?}");
        // 弹窗下方是编辑器区顶部内边距行(空),再往下才是输入行
        assert!(texts[popup_top + 3].trim().is_empty(), "{texts:?}");
        assert!(texts[popup_top + 4].starts_with("/theme"), "{texts:?}");
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

