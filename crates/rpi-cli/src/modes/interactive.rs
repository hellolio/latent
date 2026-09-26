//! interactive 模式(08 文档 §1):rpi-tui 搭建的聊天界面。
//!
//! 布局:终端底部固定视口(状态行 + 流式预览/选择列表 + 输入行),定稿
//! 内容经 `commit_lines` 追加进 scrollback(08 文档主缓冲模型:保留
//! scrollback,视口差分重绘)。扩展 UI(接缝 #5):notify 上屏,
//! confirm/select 渲染为视口内的选择列表,input 以状态行提示(M6 简化)。

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use rpi_tui::{Editor, Key, KeyParser, SelectList, Tui};
use tokio::sync::{mpsc, oneshot};

use rpi_core::{AgentSession, AgentSessionEvent, ExtensionUi, SessionSubscriber};

use crate::assembly::BuiltSession;

/// 视口布局:0 = 状态行,中间 = 流式预览/选择列表,末行 = 输入行。
const INPUT_PREFIX: &str = "❯ ";
const STREAM_PREVIEW_ROWS: usize = 2;

pub enum UiEvent {
    Session(AgentSessionEvent),
    Notify(String),
    Confirm { message: String, responder: oneshot::Sender<bool> },
    Select { message: String, options: Vec<String>, responder: oneshot::Sender<Option<usize>> },
}

/// interactive 模式的 `ExtensionUi` 真实现:把 UI 调用发进主循环渲染。
/// 通道在装配期创建(扩展 init 可能就会调 UI),主循环消费。
#[derive(Clone)]
pub struct TuiUi {
    pub(crate) tx: mpsc::UnboundedSender<UiEvent>,
}

/// 创建 interactive 模式的 UI 通道:装配期把 `TuiUi` 传给 build_session,
/// 运行期把 receiver 交给 `run_interactive_mode`。
pub fn create_tui_ui() -> (TuiUi, mpsc::UnboundedReceiver<UiEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (TuiUi { tx }, rx)
}

#[async_trait]
impl ExtensionUi for TuiUi {
    async fn notify(&self, message: &str) {
        let _ = self.tx.send(UiEvent::Notify(message.to_string()));
    }

    async fn confirm(&self, message: &str) -> bool {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(UiEvent::Confirm { message: message.to_string(), responder: tx });
        // 应答者被取消(客户端断开/请求被丢弃)= 未获确认 → 默认拒绝,
        // 不 fail-open
        rx.await.unwrap_or(false)
    }

    async fn select(&self, message: &str, options: &[String]) -> Option<usize> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(UiEvent::Select {
            message: message.to_string(),
            options: options.to_vec(),
            responder: tx,
        });
        rx.await.unwrap_or(None)
    }

    async fn input(&self, message: &str) -> Option<String> {
        // 自由文本输入以状态行提示(M6 简化;选择类 UI 已完整)
        let _ = self.tx.send(UiEvent::Notify(format!("[input] {message}")));
        None
    }
}

struct InteractiveState {
    editor: Editor,
    status: String,
    /// 流式预览的未定稿文本(已在视口展示、尚未 commit)
    stream_buffer: String,
    /// 用量追踪(T4):单回合用量行 + 会话累计
    usage: UsageTracker,
    /// 活动选择列表;None = 无交互请求
    select: Option<SelectRequest>,
    /// 并发 UI 请求排队(选择列表同时只有一个在展示;先到的先渲染,
    /// 不会互相覆盖)
    select_queue: VecDeque<SelectRequest>,
}

/// T4 用量追踪:每回合终态(TurnEnd)推送一次 usage,渲染走差分路径
/// (commit 单回合行 + 状态行累计摘要),不做每帧重算。
#[derive(Default)]
struct UsageTracker {
    total: rpi_ai::Usage,
}

impl UsageTracker {
    fn push(&mut self, usage: &rpi_ai::Usage) -> String {
        accumulate(&mut self.total, usage);
        format_usage_line(usage)
    }

    /// 会话累计摘要(状态行;无用量时空串)。
    fn summary(&self) -> String {
        if self.total.total_tokens == 0 && self.total.cost.total == 0.0 {
            return String::new();
        }
        format!(
            " · Σ {} tok ${:.6}",
            self.total.total_tokens, self.total.cost.total
        )
    }
}

fn accumulate(total: &mut rpi_ai::Usage, usage: &rpi_ai::Usage) {
    total.input += usage.input;
    total.output += usage.output;
    total.cache_read += usage.cache_read;
    total.cache_write += usage.cache_write;
    total.cache_write_1h = match (total.cache_write_1h, usage.cache_write_1h) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    total.reasoning = match (total.reasoning, usage.reasoning) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    total.total_tokens += usage.total_tokens;
    total.cost.input += usage.cost.input;
    total.cost.output += usage.cost.output;
    total.cost.cache_read += usage.cost.cache_read;
    total.cost.cache_write += usage.cost.cache_write;
    total.cost.total += usage.cost.total;
}

/// 单回合用量行(T4):输入/输出/缓存读/缓存写(+reasoning)token 与费用。
fn format_usage_line(usage: &rpi_ai::Usage) -> String {
    let mut line = format!(
        "{}[tokens] in {} · out {} · cache {}/{}",
        rpi_tui::ansi::style::DIM,
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write
    );
    if let Some(reasoning) = usage.reasoning {
        line.push_str(&format!(" · reasoning {reasoning}"));
    }
    line.push_str(&format!(" · ${:.6}{}", usage.cost.total, rpi_tui::ansi::style::RESET));
    line
}

/// 从 partial 快照提取 thinking 预览(T2 快照读口的读侧)。
fn thinking_preview(partial: &rpi_ai::AssistantMessage) -> Option<String> {
    let thinking: String = partial
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect();
    (!thinking.is_empty()).then_some(thinking)
}

/// 从 partial 快照提取 toolcall 参数预览(T2:参数逐块增长)。
fn toolcall_preview(partial: &rpi_ai::AssistantMessage) -> Option<String> {
    let args: String = partial
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::ContentBlock::ToolCall { name, arguments, .. } => Some(format!(
                "{}{}",
                if name.is_empty() { String::new() } else { format!("{name} ") },
                arguments
            )),
            _ => None,
        })
        .collect();
    (!args.is_empty()).then_some(args)
}

struct SelectRequest {
    prompt: String,
    list: SelectList,
    kind: SelectKind,
}

enum SelectKind {
    Confirm(oneshot::Sender<bool>),
    Select(oneshot::Sender<Option<usize>>),
}

pub async fn run_interactive_mode(
    built: BuiltSession,
    ui: TuiUi,
    mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
) -> Result<(), String> {
    let BuiltSession { session, .. } = built;
    let mut terminal = rpi_tui::Terminal::open().map_err(|e| e.to_string())?;

    let (rows, cols) = rpi_tui::Terminal::size();
    let viewport_height = rpi_tui::DEFAULT_VIEWPORT_HEIGHT.min(rows.saturating_sub(1)).max(1);
    let width = cols.saturating_sub(1).max(1);

    let mut tui =
        rpi_tui::create_main_screen_tui(Box::new(std::io::stdout()), viewport_height, width);

    // 锚定不变量:视口占据屏幕底部,构造渲染器前先把光标滚到底
    // (shell 提示符后启动时光标可能在屏幕中部,不锚定则首帧错位)
    terminal.write_raw(&"\r\n".repeat(rows.saturating_sub(1)));

    // 键盘线程:raw stdin 字节 → KeyParser → channel
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Key>();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut parser = KeyParser::new();
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for key in parser.feed(&buf[..n]) {
                        if key_tx.send(key).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });

    // session 事件与扩展 UI 调用共用同一事件通道(主循环统一渲染)
    session.subscribe(Arc::new(SessionToUiSubscriber { tx: ui.tx.clone() }));

    let mut state = InteractiveState {
        editor: Editor::new(),
        status: "idle".into(),
        stream_buffer: String::new(),
        usage: UsageTracker::default(),
        select: None,
        select_queue: VecDeque::new(),
    };

    welcome(tui.as_mut(), width);

    let result = event_loop(
        &mut state,
        &session,
        tui.as_mut(),
        &mut key_rx,
        &mut ui_rx,
        viewport_height,
        width,
    )
    .await;

    tui.finish();
    terminal.restore();
    result
}

fn welcome(tui: &mut dyn Tui, width: usize) {
    let markdown = rpi_tui::Markdown::new(
        "# rpi\nRust 重写的 pi agent。输入消息开始对话;`Ctrl+C` 中断/退出,`Esc` 中止当前 run。",
    );
    tui.commit_lines(&rpi_tui::Component::render(&markdown, width));
}

async fn event_loop(
    state: &mut InteractiveState,
    session: &Arc<AgentSession>,
    tui: &mut dyn Tui,
    key_rx: &mut mpsc::UnboundedReceiver<Key>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    viewport_height: usize,
    width: usize,
) -> Result<(), String> {
    let partial = session.agent().partial_message();
    render(state, tui, partial.as_ref(), viewport_height, width);

    loop {
        tokio::select! {
            key = key_rx.recv() => {
                let Some(key) = key else { break };
                if handle_key(state, session, key).await {
                    break;
                }
            }
            event = ui_rx.recv() => {
                let Some(event) = event else { break };
                handle_ui_event(state, tui, event, viewport_height, width);
            }
        }
        // T2 快照读口:UI 随帧读取"到目前为止"的 partial,不做每 delta 克隆
        let partial = session.agent().partial_message();
        render(state, tui, partial.as_ref(), viewport_height, width);
    }

    session.abort();
    session.wait_idle().await;
    Ok(())
}

/// 键盘处理:返回 true 表示退出。
async fn handle_key(
    state: &mut InteractiveState,
    session: &Arc<AgentSession>,
    key: Key,
) -> bool {
    // 选择列表激活时,键盘由列表接管
    if state.select.is_some() {
        match key {
            Key::Up => {
                if let Some(select) = state.select.as_mut() {
                    select.list.move_up();
                }
            }
            Key::Down => {
                if let Some(select) = state.select.as_mut() {
                    select.list.move_down();
                }
            }
            Key::Enter => {
                if let Some(request) = state.select.take() {
                    let index = request.list.selected;
                    match request.kind {
                        SelectKind::Confirm(responder) => {
                            let _ = responder.send(true);
                        }
                        SelectKind::Select(responder) => {
                            let _ = responder.send(Some(index));
                        }
                    }
                }
                promote_next_select(state);
            }
            Key::Escape | Key::Ctrl('c') => {
                // 模态期间 Esc/Ctrl+C 取消当前请求(不退出;再按 Ctrl+C 才退出)
                if let Some(request) = state.select.take() {
                    match request.kind {
                        SelectKind::Confirm(responder) => {
                            let _ = responder.send(false);
                        }
                        SelectKind::Select(responder) => {
                            let _ = responder.send(None);
                        }
                    }
                }
                promote_next_select(state);
            }
            _ => {}
        }
        return false;
    }

    match key {
        Key::Enter => {
            let text = state.editor.text();
            if text.trim().is_empty() {
                return false;
            }
            state.editor.commit_history();
            state.editor.clear();
            state.status = "thinking …".into();
            let session = session.clone();
            // prompt 任务在后台跑;事件经订阅者回流上屏,stdin 保持可响应
            // (run 期间的输入经 session.prompt 自动转 steer)
            tokio::spawn(async move {
                let _ = session.prompt(text).await;
            });
            false
        }
        Key::Ctrl('c') => {
            if session.agent().is_streaming() {
                session.abort();
                state.status = "aborted".into();
                false
            } else {
                true
            }
        }
        Key::Escape => {
            if session.agent().is_streaming() {
                session.abort();
                state.status = "aborted".into();
            }
            false
        }
        Key::Ctrl('d') if state.editor.is_empty() => true,
        key => {
            state.editor.handle_key(&key);
            false
        }
    }
}

fn handle_ui_event(
    state: &mut InteractiveState,
    tui: &mut dyn Tui,
    event: UiEvent,
    viewport_height: usize,
    width: usize,
) {
    match event {
        UiEvent::Session(session_event) => match session_event {
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::MessageDelta { delta }) => match delta {
                rpi_agent::MessageDeltaPayload::Text { delta } => {
                    append_stream(state, tui, &delta, viewport_height, width);
                }
                rpi_agent::MessageDeltaPayload::Thinking { .. } => {
                    state.status = "thinking …".into();
                }
                rpi_agent::MessageDeltaPayload::ToolCallArgs { .. } => {
                    state.status = "tool call …".into();
                }
            },
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::MessageEnd { message }) => {
                // assistant 定稿:未定稿残余落盘,后随空行分段
                if matches!(&*message, rpi_agent::AgentMessage::Assistant(_)) {
                    flush_stream(state, tui, width);
                    tui.commit_lines(&[String::new()]);
                }
            }
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::ToolExecutionStart {
                tool_name,
                ..
            }) => {
                flush_stream(state, tui, width);
                tui.commit_lines(&[format!(
                    "{}⏺ {}{}",
                    rpi_tui::ansi::style::DIM,
                    tool_name,
                    rpi_tui::ansi::style::RESET
                )]);
                state.status = format!("tool: {tool_name}");
            }
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::ToolExecutionEnd { .. }) => {
                state.status = "thinking …".into();
            }
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::TurnEnd { message, .. }) => {
                // T4:每回合终态展示一行用量,并累计进会话总量
                let line = state.usage.push(&message.usage);
                commit_wrapped(tui, &line, width);
            }
            AgentSessionEvent::AgentSettled => {
                state.status = "idle".into();
            }
            AgentSessionEvent::AutoRetryStart { attempt, delay_ms, reason } => {
                tui.commit_lines(&[format!(
                    "{}[retry #{attempt} in {delay_ms}ms] {reason}{}",
                    rpi_tui::ansi::style::YELLOW,
                    rpi_tui::ansi::style::RESET
                )]);
            }
            _ => {}
        },
        UiEvent::Notify(message) => {
            commit_wrapped(tui, &message, width);
        }
        UiEvent::Confirm { message, responder } => {
            state.select_queue.push_back(SelectRequest {
                prompt: message,
                list: SelectList::new(vec!["yes".into(), "no".into()]),
                kind: SelectKind::Confirm(responder),
            });
            promote_next_select(state);
        }
        UiEvent::Select { message, options, responder } => {
            state.select_queue.push_back(SelectRequest {
                prompt: message,
                list: SelectList::new(options),
                kind: SelectKind::Select(responder),
            });
            promote_next_select(state);
        }
    }
}

/// 无活动选择列表时,把队首请求提升为活动(并发请求按序渲染)。
fn promote_next_select(state: &mut InteractiveState) {
    if state.select.is_none() {
        state.select = state.select_queue.pop_front();
    }
}

/// 流式增量 → 预览区:按行切分,完整行落盘;预览区超容时最早的部分落盘。
fn append_stream(
    state: &mut InteractiveState,
    tui: &mut dyn Tui,
    delta: &str,
    viewport_height: usize,
    width: usize,
) {
    state.stream_buffer.push_str(delta);
    // 预览行数受视口约束:状态行 + 输入行之外最多 STREAM_PREVIEW_ROWS 行
    let max_preview = if viewport_height >= STREAM_PREVIEW_ROWS + 2 {
        STREAM_PREVIEW_ROWS
    } else {
        1
    };
    while let Some(pos) = state.stream_buffer.find('\n') {
        let line: String = state.stream_buffer.drain(..pos + 1).collect();
        commit_wrapped(tui, line.trim_end_matches('\n'), width);
    }
    let wrapped = rpi_tui::wrap_to_width(&state.stream_buffer, width);
    if wrapped.len() > max_preview {
        let overflow = wrapped.len() - max_preview;
        let committed: Vec<String> = wrapped[..overflow].to_vec();
        // 折行按词边界切(丢词间空格);拼回时补空格,避免两词粘连
        let rest = wrapped[overflow..].join(" ");
        for line in committed {
            tui.commit_lines(&[line]);
        }
        state.stream_buffer = rest;
    }
}

/// message 定稿:残余流式文本落盘。
fn flush_stream(state: &mut InteractiveState, tui: &mut dyn Tui, width: usize) {
    if !state.stream_buffer.is_empty() {
        let rest = std::mem::take(&mut state.stream_buffer);
        commit_wrapped(tui, &rest, width);
    }
}

fn commit_wrapped(tui: &mut dyn Tui, line: &str, width: usize) {
    for wrapped in rpi_tui::wrap_to_width(line, width.max(1)) {
        tui.commit_lines(&[wrapped]);
    }
}

/// 视口帧:状态行 + (选择列表/流式预览/thinking 预览/工具参数预览) + 空行填充 + 输入行。
fn render(
    state: &InteractiveState,
    tui: &mut dyn Tui,
    partial: Option<&rpi_ai::AssistantMessage>,
    viewport_height: usize,
    width: usize,
) {
    let status_text = format!("{}{}", state.status, state.usage.summary());
    let status = format!(
        "{}{}{}",
        rpi_tui::ansi::style::DIM,
        truncate(&status_text, width),
        rpi_tui::ansi::style::RESET
    );

    let mut lines = vec![status];
    if let Some(select) = &state.select {
        lines.push(format!("{}{}", rpi_tui::ansi::style::BOLD, truncate(&select.prompt, width)));
        for line in rpi_tui::Component::render(&select.list, width) {
            lines.push(line);
        }
    } else if !state.stream_buffer.is_empty() {
        for line in rpi_tui::wrap_to_width(&state.stream_buffer, width) {
            lines.push(line);
        }
    } else if let Some(partial) = partial {
        // T2:文本未流式时,预览区展示 thinking / 工具参数的逐块增长
        if let Some(thinking) = thinking_preview(partial) {
            let tail: Vec<&str> = thinking.lines().rev().take(2).collect();
            for line in tail.into_iter().rev() {
                lines.push(format!(
                    "{}{}{}",
                    rpi_tui::ansi::style::DIM,
                    truncate(line, width),
                    rpi_tui::ansi::style::RESET
                ));
            }
        } else if let Some(args) = toolcall_preview(partial) {
            lines.push(format!(
                "{}⚙ {}{}",
                rpi_tui::ansi::style::DIM,
                truncate(&args, width),
                rpi_tui::ansi::style::RESET
            ));
        }
    }
    // 输入行固定在末行;中间不足补空行,超出挤掉最早的中间行
    let body_height = viewport_height.saturating_sub(1).max(1);
    lines.resize(body_height, String::new());
    while lines.len() > body_height {
        lines.remove(1);
    }
    let (input_line, cursor_col) = input_line_and_cursor(&state.editor, width);
    lines.push(input_line);

    tui.render_viewport(&lines, cursor_col);
}

/// 输入行渲染:超宽文本开一个保证光标可见的窗口(水平滚动);
/// 光标列按**显示宽**计算(CJK 双宽,字符下标会错位)。
fn input_line_and_cursor(editor: &Editor, width: usize) -> (String, usize) {
    let prefix_width = rpi_tui::display_width(INPUT_PREFIX);
    let avail = width.saturating_sub(prefix_width).max(1);
    let chars: Vec<char> = editor.text().chars().collect();
    let cursor_chars = editor.cursor_pos().min(chars.len());

    // 窗口 [start, end):从光标往左扩(给光标本身留 1 列),再从光标往右补满
    let mut start = cursor_chars;
    let mut used = 0usize;
    while start > 0 {
        let w = rpi_tui::char_width(chars[start - 1]);
        if used + w > avail.saturating_sub(1) {
            break;
        }
        used += w;
        start -= 1;
    }
    let mut end = cursor_chars;
    while end < chars.len() {
        let w = rpi_tui::char_width(chars[end]);
        if used + w > avail {
            break;
        }
        used += w;
        end += 1;
    }
    let window: String = chars[start..end].iter().collect();
    let cursor_col = prefix_width
        + rpi_tui::display_width(&chars[start..cursor_chars].iter().collect::<String>());
    (format!("{INPUT_PREFIX}{window}"), cursor_col)
}

fn truncate(text: &str, width: usize) -> String {
    let (cut, _) = rpi_tui::width::truncate_to_width(text, width.max(1));
    cut
}

struct SessionToUiSubscriber {
    tx: mpsc::UnboundedSender<UiEvent>,
}

#[async_trait]
impl SessionSubscriber for SessionToUiSubscriber {
    async fn on_session_event(&self, event: &AgentSessionEvent) {
        let _ = self.tx.send(UiEvent::Session(event.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::MainScreenTui;

    #[derive(Default, Clone)]
    struct SharedVec(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedVec {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedVec {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn test_state() -> InteractiveState {
        InteractiveState {
            editor: Editor::new(),
            status: "idle".into(),
            stream_buffer: String::new(),
            usage: UsageTracker::default(),
            select: None,
            select_queue: VecDeque::new(),
        }
    }

    #[test]
    fn input_line_cursor_uses_display_width_for_cjk() {
        let mut state = test_state();
        state.editor.set_text("中a"); // 显示宽 3
        let (line, col) = input_line_and_cursor(&state.editor, 40);
        assert_eq!(line, "❯ 中a");
        // 前缀 2 列 + "中" 2 列 + "a" 1 列 = 5(字符下标 3 会算出 4)
        assert_eq!(col, 5);
    }

    #[test]
    fn input_line_windows_long_text_to_keep_cursor_visible() {
        let mut state = test_state();
        state.editor.set_text(&"x".repeat(100));
        let (line, col) = input_line_and_cursor(&state.editor, 20);
        // 整行(含前缀)不超过宽度,光标可见
        assert!(rpi_tui::display_width(&line) <= 20);
        assert!(col < 20);
        // 光标仍在行尾
        assert_eq!(col, rpi_tui::display_width(&line));
    }

    #[test]
    fn input_line_empty_is_prefix_only() {
        let state = test_state();
        let (line, col) = input_line_and_cursor(&state.editor, 40);
        assert_eq!(line, "❯ ");
        assert_eq!(col, 2);
    }

    #[tokio::test]
    async fn append_stream_commits_full_lines_and_bounds_preview() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 40);
        let mut state = test_state();

        append_stream(&mut state, &mut tui, "first line\n", 6, 40);
        assert_eq!(state.stream_buffer, "");

        // 一段超长无换行文本:预览行数被限制,溢出部分落盘
        let long = "word ".repeat(60); // 300 字符 → 40 列宽下 8 行
        append_stream(&mut state, &mut tui, &long, 6, 40);
        let wrapped = rpi_tui::wrap_to_width(&state.stream_buffer, 40);
        assert!(wrapped.len() <= 2, "预览最多 2 行,实际 {}", wrapped.len());
        let committed = buffer.text();
        assert!(committed.contains("word"), "溢出内容已落盘");
        // 落盘内容在视口清屏序列之后(进入 scrollback)
        assert!(committed.contains("\x1b[0J"));
    }

    #[tokio::test]
    async fn flush_stream_commits_remaining_partial() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 40);
        let mut state = test_state();
        append_stream(&mut state, &mut tui, "partial text", 6, 40);
        assert_eq!(state.stream_buffer, "partial text");
        flush_stream(&mut state, &mut tui, 40);
        assert_eq!(state.stream_buffer, "");
        assert!(buffer.text().contains("partial text"));
    }

    #[test]
    fn usage_tracker_formats_turn_line_and_accumulates() {
        // T4:Mock 注入已知 usage,单回合行与累计摘要数值一致
        let mut tracker = UsageTracker::default();
        let usage = rpi_ai::Usage {
            input: 100,
            output: 20,
            cache_read: 5,
            cache_write: 10,
            cache_write_1h: None,
            reasoning: Some(8),
            total_tokens: 135,
            cost: rpi_ai::Cost { total: 0.0012, ..Default::default() },
        };
        let line = tracker.push(&usage);
        assert!(line.contains("in 100"), "{line}");
        assert!(line.contains("out 20"), "{line}");
        assert!(line.contains("cache 5/10"), "{line}");
        assert!(line.contains("reasoning 8"), "{line}");
        assert!(line.contains("$0.001200"), "{line}");
        // 第二回合同样数值:累计翻倍
        tracker.push(&usage);
        let summary = tracker.summary();
        assert!(summary.contains("270 tok"), "{summary}");
        assert!(summary.contains("$0.002400"), "{summary}");
    }

    #[test]
    fn usage_tracker_zero_usage_has_empty_summary() {
        let tracker = UsageTracker::default();
        assert_eq!(tracker.summary(), "");
    }

    #[test]
    fn partial_previews_extract_thinking_and_toolcall_args() {
        // T2:partial 快照读口的读侧
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let mut partial = rpi_ai::AssistantMessage::pending(&model);
        assert_eq!(thinking_preview(&partial), None);
        partial.content.push(rpi_ai::ContentBlock::Thinking {
            thinking: "step 1".into(),
            thinking_signature: None,
            redacted: None,
        });
        assert_eq!(thinking_preview(&partial).as_deref(), Some("step 1"));
        partial.content.push(rpi_ai::ContentBlock::ToolCall {
            id: "t".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        });
        let args = toolcall_preview(&partial).unwrap();
        assert!(args.contains("bash"), "{args}");
        assert!(args.contains("ls"), "{args}");
    }

    #[test]
    fn concurrent_select_requests_queue_and_promote() {
        // P1-4 回归:并发 UI 请求不互相覆盖,按序提升
        let mut state = test_state();
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        let (tx3, _rx3) = oneshot::channel();
        handle_ui_event(
            &mut state,
            &mut NullTui,
            UiEvent::Confirm { message: "q1".into(), responder: tx1 },
            6,
            40,
        );
        handle_ui_event(
            &mut state,
            &mut NullTui,
            UiEvent::Confirm { message: "q2".into(), responder: tx2 },
            6,
            40,
        );
        handle_ui_event(
            &mut state,
            &mut NullTui,
            UiEvent::Select { message: "q3".into(), options: vec![], responder: tx3 },
            6,
            40,
        );
        assert_eq!(state.select.as_ref().unwrap().prompt, "q1", "队首先展示");
        assert_eq!(state.select_queue.len(), 2);
        // 回答当前 → 队首提升
        let request = state.select.take().unwrap();
        match request.kind {
            SelectKind::Confirm(responder) => {
                let _ = responder.send(true);
            }
            SelectKind::Select(_) => panic!("应为 confirm"),
        }
        promote_next_select(&mut state);
        assert_eq!(state.select.as_ref().unwrap().prompt, "q2");
    }

    /// 渲染 no-op(tui 断言由其余用例覆盖)。
    struct NullTui;
    impl Tui for NullTui {
        fn commit_lines(&mut self, _lines: &[String]) {}
        fn render_viewport(&mut self, _lines: &[String], _cursor_col: usize) {}
        fn finish(&mut self) {}
    }
}
