//! interactive 模式(08 文档 §1):rpi-tui 搭建的聊天界面。
//!
//! 布局:终端底部固定视口(状态行 + footer + 流式预览/选择列表 + 输入行),
//! 定稿内容经 `commit_lines` 追加进 scrollback(08 文档主缓冲模型:保留
//! scrollback,视口差分重绘)。扩展 UI(接缝 #5):notify 上屏,confirm/select
//! 渲染为视口内的选择列表。
//!
//! pi 对齐:错误回合渲染红字(assistant-message.ts 的 stopReason 呈现)、
//! 启动回放当前转录(renderSessionItems)、斜杠命令子集(BUILTIN_SLASH_
//! COMMANDS 核心)、footer(model/thinking/context% 阈值变色)、双击 Ctrl+C
//! 退出。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rpi_tui::{Editor, Key, KeyParser, SelectList, Tui};
use tokio::sync::{mpsc, oneshot};

use rpi_ai::{ContentBlock, StopReason};
use rpi_core::{AgentSession, AgentSessionEvent, ExtensionUi, SessionSubscriber};

use crate::assembly::BuiltSession;
use crate::modes::slash;

/// 视口布局:0 = 状态行,1 = footer,中间 = 流式预览/选择列表,末行 = 输入行。
const INPUT_PREFIX: &str = "❯ ";
const STREAM_PREVIEW_ROWS: usize = 2;
/// pi 退出语义:500ms 内双击 Ctrl+C 退出(interactive-mode.ts:4121)。
const DOUBLE_CTRL_C_WINDOW: Duration = Duration::from_millis(500);

pub enum UiEvent {
    Session(AgentSessionEvent),
    Notify(String),
    /// 后台任务回写状态行(compact 等不再内联 await 的命令)
    Status(String),
    /// /compact 完成(结果经后台任务回流;成功后 ctx% 估计重置)
    CompactDone(Result<usize, String>),
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

/// 键盘/命令处理共用的会话上下文(状态 + TUI 之外的全部依赖)。
struct InteractiveCtx<'a> {
    session: &'a Arc<AgentSession>,
    /// None = 内存会话(无 SessionManager)
    session_manager: Option<&'a Arc<rpi_session::SessionManager>>,
    /// `/model` 的候选与解析(models.json + 内置 provider 默认表)
    resolver: &'a rpi_core::ModelResolver,
    /// prompt 错误兜底回 UI 通道(事件流之外的装配/并发错误)
    ui_tx: mpsc::UnboundedSender<UiEvent>,
}

struct InteractiveState {
    editor: Editor,
    status: String,
    /// 流式预览的未定稿文本(已在视口展示、尚未 commit)
    stream_buffer: String,
    /// 当前 assistant 消息流式期间累积的 thinking(文本增量到达时或定稿时
    /// 提交进 scrollback —— 只在预览区展示会随帧消失,用户要求可回看)
    pending_thinking: Option<String>,
    /// 用量追踪(T4):单回合用量行 + 会话累计
    usage: UsageTracker,
    /// 活动选择列表;None = 无交互请求
    select: Option<SelectRequest>,
    /// 并发 UI 请求排队(选择列表同时只有一个在展示;先到的先渲染,
    /// 不会互相覆盖)
    select_queue: VecDeque<SelectRequest>,
    /// footer:当前模型(provider/id)
    model_label: String,
    /// footer:当前 thinking 级别名("off" = 未启用)
    thinking_label: String,
    /// footer:当前模型上下文窗口(0 = 未知,不显示 ctx%)
    context_window: u64,
    /// footer:最后一条 assistant 回合的上下文 token 估计
    context_tokens: u64,
    /// 双击 Ctrl+C 退出:上一次 Ctrl+C 时刻(非流式期间)
    last_ctrl_c: Option<Instant>,
}

impl InteractiveState {
    fn new() -> Self {
        InteractiveState {
            editor: Editor::new(),
            status: "idle".into(),
            stream_buffer: String::new(),
            pending_thinking: None,
            usage: UsageTracker::default(),
            select: None,
            select_queue: VecDeque::new(),
            model_label: "—".into(),
            thinking_label: "off".into(),
            context_window: 0,
            context_tokens: 0,
            last_ctrl_c: None,
        }
    }
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

    /// 会话累计摘要(footer;无用量时空串)。
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

/// 最后一次请求的完整上下文规模(footer ctx% 的分母侧估计):pi footer 同源
/// 算法(usage.input + output + cacheRead + cacheWrite)。
fn context_tokens_of(usage: &rpi_ai::Usage) -> u64 {
    usage.input + usage.output + usage.cache_read + usage.cache_write
}

/// ctx 段:>70% warning 色、>90% error 色(pi footer 阈值);窗口未知或无
/// 用量时不显示。
fn format_ctx_segment(window: u64, tokens: u64) -> String {
    if window == 0 || tokens == 0 {
        return String::new();
    }
    let pct = (tokens * 100 / window).min(100);
    let style = if pct > 90 {
        rpi_tui::ansi::style::RED
    } else if pct > 70 {
        rpi_tui::ansi::style::YELLOW
    } else {
        rpi_tui::ansi::style::DIM
    };
    format!(" · ctx {style}{pct}%{}", rpi_tui::ansi::style::RESET)
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
    /// 内部选择器(/model):Enter 应用选择,无 responder
    Model { models: Vec<rpi_ai::Model> },
    /// 内部选择器(/thinking):选项序 = thinking_level_options()
    Thinking,
}

/// /thinking 选择器选项("off" = 关闭,其后为 ThinkingLevel::ALL 顺序)。
fn thinking_level_options() -> Vec<&'static str> {
    let mut names = vec!["off"];
    names.extend(rpi_ai::ThinkingLevel::ALL.iter().map(|level| level.as_str()));
    names
}

/// /thinking 参数 → 设置值("off" = None;其余走装配期解析)。
fn parse_thinking_input(name: &str) -> Option<Option<rpi_ai::ThinkingLevel>> {
    match name.to_ascii_lowercase().as_str() {
        "off" | "none" => Some(None),
        other => crate::assembly::parse_thinking_level(other).map(Some),
    }
}

/// 从 Agent 状态快照刷新 footer 的模型/thinking/窗口字段。
fn refresh_footer(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    let snapshot = ctx.session.agent().state_snapshot();
    if let Some(model) = snapshot.model {
        state.model_label = format!("{}/{}", model.provider, model.id);
        state.context_window = model.context_window;
    }
    state.thinking_label =
        snapshot.thinking_level.map(|level| level.as_str().to_string()).unwrap_or_else(|| "off".into());
}

pub async fn run_interactive_mode(
    built: BuiltSession,
    ui: TuiUi,
    mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
) -> Result<(), String> {
    let BuiltSession { session, session_manager } = built;
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

    // /model 解析与主流程同源:models.json + 内置 provider 默认表
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let resolver = rpi_core::create_model_resolver_from_config(Some(&cwd), home.as_deref());
    let ctx = InteractiveCtx {
        session: &session,
        session_manager: session_manager.as_ref(),
        resolver: &resolver,
        ui_tx: ui.tx.clone(),
    };

    let mut state = InteractiveState::new();
    refresh_footer(&ctx, &mut state);

    welcome(tui.as_mut());
    // pi 语义:恢复/续聊时回放当前转录(user/assistant/工具/压缩摘要)
    replay_history(&ctx, &mut state, tui.as_mut(), width);

    let result = event_loop(
        &ctx,
        &mut state,
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

fn welcome(tui: &mut dyn Tui) {
    tui.commit_lines(&[format!(
        "{}rpi v{}{}",
        rpi_tui::ansi::style::BOLD,
        env!("CARGO_PKG_VERSION"),
        rpi_tui::ansi::style::RESET
    )]);
    tui.commit_lines(&[format!(
        "{}输入消息开始对话 · /help 查看命令 · Ctrl+C 双击退出 · Esc 中止当前 run{}",
        rpi_tui::ansi::style::DIM,
        rpi_tui::ansi::style::RESET
    )]);
    tui.commit_lines(&[String::new()]);
}

/// 启动回放(pi renderSessionItems 语义):渲染当前转录 —— 即压缩感知的
/// 当前分支上下文(session 装配时已从 projection 回填),含 user/assistant/
/// 工具调用与结果/压缩摘要。会话累计上下文 token 估计同步初始化。
fn replay_history(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    tui: &mut dyn Tui,
    width: usize,
) {
    let messages = ctx.session.agent().messages();
    for message in &messages {
        replay_message(tui, message, width);
    }
    if let Some(tokens) = messages.iter().rev().find_map(|message| {
        message.as_assistant().map(|assistant| context_tokens_of(&assistant.usage))
    }) {
        state.context_tokens = tokens;
    }
    if let Some(manager) = ctx.session_manager {
        let compactions = manager
            .branch_entries()
            .iter()
            .filter(|entry| matches!(entry, rpi_session::Entry::Compaction { .. }))
            .count();
        if compactions > 0 {
            tui.commit_lines(&[format!(
                "{}Session compacted {compactions} times{}",
                rpi_tui::ansi::style::DIM,
                rpi_tui::ansi::style::RESET
            )]);
        }
    }
    tui.commit_lines(&[String::new()]);
}

/// 单条历史消息的回放渲染(样式对齐 pi:user 反色块、assistant 正文、
/// 工具调用一行摘要 + 结果行、压缩/分支摘要 dim 行)。
fn replay_message(tui: &mut dyn Tui, message: &rpi_agent::AgentMessage, width: usize) {
    use rpi_tui::ansi::style;
    match message {
        rpi_agent::AgentMessage::System { .. } => {}
        rpi_agent::AgentMessage::User { content, .. } => {
            commit_user_block(tui, content, width);
            tui.commit_lines(&[String::new()]);
        }
        rpi_agent::AgentMessage::Assistant(assistant) => {
            for block in &assistant.content {
                match block {
                    ContentBlock::Text { text, .. } => {
                        for line in text.lines() {
                            commit_wrapped(tui, line, width);
                        }
                    }
                    ContentBlock::ToolCall { name, arguments, .. } => {
                        let args = serde_json::to_string(arguments).unwrap_or_default();
                        tui.commit_lines(&[format!(
                            "{}⏺ {name} {}{}",
                            style::DIM,
                            truncate(&args, width.saturating_sub(4).max(1)),
                            style::RESET
                        )]);
                    }
                    ContentBlock::Thinking { thinking, .. } => {
                        commit_thinking_block(tui, thinking, width);
                    }
                    ContentBlock::Image { .. } => {}
                }
            }
            match assistant.stop_reason {
                StopReason::Error => {
                    let message = assistant
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Unknown error".into());
                    commit_wrapped(tui, &format!("{}Error: {message}{}", style::RED, style::RESET), width);
                }
                StopReason::Aborted => {
                    commit_wrapped(tui, &format!("{}Operation aborted{}", style::RED, style::RESET), width);
                }
                StopReason::Length => {
                    commit_wrapped(
                        tui,
                        &format!(
                            "{}Response was truncated before completion.{}",
                            style::RED,
                            style::RESET
                        ),
                        width,
                    );
                }
                _ => {}
            }
            tui.commit_lines(&[String::new()]);
        }
        rpi_agent::AgentMessage::ToolResult { tool_name, is_error, .. } => {
            let text = message.tool_result_content().unwrap_or_default();
            let first = text.lines().next().unwrap_or("");
            let style = if *is_error { style::RED } else { style::DIM };
            tui.commit_lines(&[format!(
                "{style}  └ {tool_name}: {}{}",
                truncate(first, width.saturating_sub(6).max(1)),
                style::RESET
            )]);
        }
        rpi_agent::AgentMessage::BashExecution { command, .. } => {
            tui.commit_lines(&[format!(
                "{}! {}{}",
                style::DIM,
                truncate(command, width.max(1)),
                style::RESET
            )]);
        }
        rpi_agent::AgentMessage::CompactionSummary { summary, .. } => {
            let first = summary.lines().next().unwrap_or("");
            tui.commit_lines(&[format!(
                "{}── 压缩摘要: {}{}",
                style::DIM,
                truncate(first, width.saturating_sub(10).max(1)),
                style::RESET
            )]);
        }
        rpi_agent::AgentMessage::BranchSummary { summary, .. } => {
            let first = summary.lines().next().unwrap_or("");
            tui.commit_lines(&[format!(
                "{}── 分支摘要: {}{}",
                style::DIM,
                truncate(first, width.saturating_sub(10).max(1)),
                style::RESET
            )]);
        }
        rpi_agent::AgentMessage::Custom(_) => {}
    }
}

/// user 消息反色块(pi userMessageBg 等价:REVERSE + 补齐行宽)。
fn commit_user_block(tui: &mut dyn Tui, content: &str, width: usize) {
    use rpi_tui::ansi::style;
    let inner = width.saturating_sub(2).max(1);
    for line in rpi_tui::wrap_to_width(content, inner) {
        let pad = inner.saturating_sub(rpi_tui::display_width(&line));
        tui.commit_lines(&[format!(
            "{} {}{} {}",
            style::REVERSE,
            line,
            " ".repeat(pad),
            style::RESET
        )]);
    }
}

async fn event_loop(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    tui: &mut dyn Tui,
    key_rx: &mut mpsc::UnboundedReceiver<Key>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    viewport_height: usize,
    width: usize,
) -> Result<(), String> {
    let partial = ctx.session.agent().partial_message();
    render(state, tui, partial.as_ref(), viewport_height, width);

    loop {
        tokio::select! {
            key = key_rx.recv() => {
                let Some(key) = key else { break };
                if handle_key(ctx, state, tui, key, width).await {
                    break;
                }
            }
            event = ui_rx.recv() => {
                let Some(event) = event else { break };
                handle_ui_event(state, tui, event, viewport_height, width);
            }
        }
        // T2 快照读口:UI 随帧读取"到目前为止"的 partial,不做每 delta 克隆
        let partial = ctx.session.agent().partial_message();
        render(state, tui, partial.as_ref(), viewport_height, width);
    }

    ctx.session.abort();
    ctx.session.wait_idle().await;
    Ok(())
}

/// 键盘处理:返回 true 表示退出。
async fn handle_key(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    tui: &mut dyn Tui,
    key: Key,
    width: usize,
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
                        SelectKind::Model { models } => {
                            if let Some(model) = models.get(index) {
                                ctx.session.set_model(model.clone());
                                refresh_footer(ctx, state);
                                state.status = format!("model → {}", state.model_label);
                            }
                        }
                        SelectKind::Thinking => {
                            if let Some(name) = thinking_level_options().get(index) {
                                let level = crate::assembly::parse_thinking_level(name);
                                ctx.session.set_thinking_level(level);
                                refresh_footer(ctx, state);
                                state.status = format!("thinking → {name}");
                            }
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
                        // 内部选择器:取消即丢弃,无 responder
                        SelectKind::Model { .. } | SelectKind::Thinking => {}
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
            let text = state.editor.text().trim().to_string();
            if text.is_empty() {
                return false;
            }
            state.editor.commit_history();
            state.editor.clear();
            state.last_ctrl_c = None;
            match slash::parse(&text) {
                slash::SlashInput::NotACommand(text) => {
                    state.status = "thinking …".into();
                    let session = ctx.session.clone();
                    let ui_tx = ctx.ui_tx.clone();
                    // prompt 任务在后台跑;事件经订阅者回流上屏,stdin 保持可响应
                    // (run 期间的输入经 session.prompt 自动转 steer)。
                    // 事件流之外的装配错误经 UI 通道兜底上屏(错误可见性)
                    tokio::spawn(async move {
                        if let Err(error) = session.prompt(text).await {
                            let _ = ui_tx.send(UiEvent::Notify(format!(
                                "{}Error: {error}{}",
                                rpi_tui::ansi::style::RED,
                                rpi_tui::ansi::style::RESET
                            )));
                        }
                    });
                }
                slash::SlashInput::Unknown(name) => {
                    // rpi 无动态命令源:未知 /xxx 本地警告,不发给模型
                    commit_wrapped(
                        tui,
                        &format!(
                            "{}Unknown command: {name}(输入 /help 查看可用命令){}",
                            rpi_tui::ansi::style::YELLOW,
                            rpi_tui::ansi::style::RESET
                        ),
                        width,
                    );
                }
                slash::SlashInput::Command(action) => {
                    return execute_command(ctx, state, tui, action, width).await;
                }
            }
            false
        }
        Key::Ctrl('c') => {
            if ctx.session.agent().is_streaming() {
                ctx.session.abort();
                state.status = "aborted".into();
                state.last_ctrl_c = None;
                false
            } else {
                // pi 退出语义:双击 Ctrl+C(500ms 内)退出,首按提示
                let now = Instant::now();
                match state.last_ctrl_c {
                    Some(at) if now.duration_since(at) < DOUBLE_CTRL_C_WINDOW => true,
                    _ => {
                        state.last_ctrl_c = Some(now);
                        state.status = "press Ctrl+C again to exit".into();
                        false
                    }
                }
            }
        }
        Key::Escape => {
            state.last_ctrl_c = None;
            if ctx.session.agent().is_streaming() {
                ctx.session.abort();
                state.status = "aborted".into();
            }
            false
        }
        Key::Ctrl('d') if state.editor.is_empty() => true,
        key => {
            state.last_ctrl_c = None;
            state.editor.handle_key(&key);
            false
        }
    }
}

/// 斜杠命令执行(解析在 slash.rs):返回 true 表示退出。
async fn execute_command(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    tui: &mut dyn Tui,
    action: slash::SlashAction,
    width: usize,
) -> bool {
    use rpi_tui::ansi::style;
    match action {
        slash::SlashAction::Help => {
            for line in slash::help_lines() {
                commit_wrapped(tui, &line, width);
            }
        }
        slash::SlashAction::Quit => return true,
        slash::SlashAction::Session => {
            for line in session_info_lines(ctx, state) {
                commit_wrapped(tui, &line, width);
            }
        }
        slash::SlashAction::Compact { arg } => {
            // 流式期间压缩会让 SessionCompactor 先落盘 Compaction entry、
            // 随后 set_messages 失败 —— JSONL 与内存上下文分叉;且摘要 LLM
            // 调用内联 await 会冻结事件循环。守卫 + spawn(结果经事件回流)
            if ctx.session.agent().is_streaming() {
                commit_wrapped(
                    tui,
                    &format!(
                        "{}run 进行中不能压缩;等待 run 结束或 Esc 中止后再试{}",
                        style::RED,
                        style::RESET
                    ),
                    width,
                );
                return false;
            }
            if arg.is_some() {
                commit_wrapped(
                    tui,
                    &format!("{}自定义压缩指令暂不支持,已忽略{}", style::DIM, style::RESET),
                    width,
                );
            }
            state.status = "compacting …".into();
            let session = ctx.session.clone();
            let ui_tx = ctx.ui_tx.clone();
            tokio::spawn(async move {
                let _ = ui_tx.send(UiEvent::CompactDone(session.compact().await));
            });
        }
        slash::SlashAction::Model { arg } => match arg {
            Some(spec) => match ctx.resolver.resolve(&spec) {
                Ok(model) => {
                    ctx.session.set_model(model);
                    refresh_footer(ctx, state);
                    state.status = format!("model → {}", state.model_label);
                }
                Err(error) => {
                    commit_wrapped(
                        tui,
                        &format!("{}model 切换失败: {error}{}", style::RED, style::RESET),
                        width,
                    );
                }
            },
            None => open_model_selector(ctx, state),
        },
        slash::SlashAction::Thinking { arg } => match arg {
            Some(name) => match parse_thinking_input(&name) {
                Some(level) => {
                    ctx.session.set_thinking_level(level);
                    refresh_footer(ctx, state);
                    state.status = format!("thinking → {}", state.thinking_label);
                }
                None => {
                    commit_wrapped(
                        tui,
                        &format!(
                            "{}未知 thinking 级别: {name}(off|minimal|low|medium|high|xhigh|max){}",
                            style::RED,
                            style::RESET
                        ),
                        width,
                    );
                }
            },
            None => open_thinking_selector(state),
        },
    }
    false
}

fn open_model_selector(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    let models = ctx.resolver.available_models();
    if models.is_empty() {
        state.status = "没有可选模型(models.json 或内置 provider)".into();
        return;
    }
    let specs: Vec<String> =
        models.iter().map(|model| format!("{}/{}", model.provider, model.id)).collect();
    let mut list = SelectList::new(specs);
    if let Some(index) = list.options.iter().position(|spec| *spec == state.model_label) {
        list.selected = index;
    }
    state.select =
        Some(SelectRequest { prompt: "选择模型".into(), list, kind: SelectKind::Model { models } });
}

fn open_thinking_selector(state: &mut InteractiveState) {
    let names: Vec<String> = thinking_level_options().into_iter().map(String::from).collect();
    let mut list = SelectList::new(names.clone());
    if let Some(index) = names.iter().position(|name| *name == state.thinking_label) {
        list.selected = index;
    }
    state.select = Some(SelectRequest { prompt: "选择 thinking 级别".into(), list, kind: SelectKind::Thinking });
}

fn session_info_lines(ctx: &InteractiveCtx<'_>, state: &InteractiveState) -> Vec<String> {
    use rpi_tui::ansi::style;
    let mut lines = vec![format!("{}session{}", style::BOLD, style::RESET)];
    match ctx.session_manager {
        Some(manager) => {
            lines.push(format!("  id:   {}", manager.session_id()));
            lines.push(format!(
                "  file: {}",
                manager
                    .file_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(内存)".into())
            ));
        }
        None => lines.push("  (无会话存储)".into()),
    }
    lines.push(format!("  model:    {}", state.model_label));
    lines.push(format!("  thinking: {}", state.thinking_label));
    lines.push(format!("  messages: {}", ctx.session.agent().messages().len()));
    lines.push(format!(
        "  usage:    {} tok · ${:.6}",
        state.usage.total.total_tokens, state.usage.total.cost.total
    ));
    lines
}

fn handle_ui_event(
    state: &mut InteractiveState,
    tui: &mut dyn Tui,
    event: UiEvent,
    viewport_height: usize,
    width: usize,
) {
    use rpi_tui::ansi::style;
    match event {
        UiEvent::Session(session_event) => match session_event {
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::MessageDelta { delta }) => match delta {
                rpi_agent::MessageDeltaPayload::Text { delta } => {
                    // thinking → text 的交接点:thinking 块先于正文提交进
                    // scrollback(流式期间只在预览区,定稿即消失)
                    commit_pending_thinking(state, tui, width);
                    append_stream(state, tui, &delta, viewport_height, width);
                }
                rpi_agent::MessageDeltaPayload::Thinking { delta } => {
                    state.status = "thinking …".into();
                    state
                        .pending_thinking
                        .get_or_insert_with(String::new)
                        .push_str(&delta);
                }
                rpi_agent::MessageDeltaPayload::ToolCallArgs { .. } => {
                    state.status = "tool call …".into();
                }
            },
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::MessageStart { message, .. }) => {
                // 新 assistant 消息:重置 thinking 累积(上一条的已提交/丢弃)
                if matches!(message.as_ref(), rpi_agent::AgentMessage::Assistant(_)) {
                    state.pending_thinking = None;
                }
            }
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::MessageEnd { message }) => {
                match message.as_ref() {
                    // assistant 定稿:未定稿残余 + thinking 块落盘,后随空行分段
                    rpi_agent::AgentMessage::Assistant(_) => {
                        flush_stream(state, tui, width);
                        commit_pending_thinking(state, tui, width);
                        tui.commit_lines(&[String::new()]);
                    }
                    // 用户自己的消息即时上屏(与回放呈现一致;steering 亦可见)
                    rpi_agent::AgentMessage::User { content, .. } => {
                        flush_stream(state, tui, width);
                        commit_user_block(tui, content.as_str(), width);
                        tui.commit_lines(&[String::new()]);
                    }
                    // 工具结果行(与回放的 `└ name: …` 样式一致;错误红字)
                    rpi_agent::AgentMessage::ToolResult { tool_name, is_error, .. } => {
                        let text = message.tool_result_content().unwrap_or_default();
                        let first = text.lines().next().unwrap_or("");
                        let style = if *is_error { style::RED } else { style::DIM };
                        tui.commit_lines(&[format!(
                            "{style}  └ {tool_name}: {}{}",
                            truncate(first, width.saturating_sub(6).max(1)),
                            style::RESET
                        )]);
                    }
                    _ => {}
                }
            }
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::ToolExecutionStart {
                tool_name,
                ..
            }) => {
                flush_stream(state, tui, width);
                tui.commit_lines(&[format!(
                    "{}⏺ {}{}",
                    style::DIM,
                    tool_name,
                    style::RESET
                )]);
                state.status = format!("tool: {tool_name}");
            }
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::ToolExecutionEnd { .. }) => {
                state.status = "thinking …".into();
            }
            AgentSessionEvent::Agent(rpi_agent::AgentEvent::TurnEnd { message, .. }) => {
                // pi assistant-message.ts 语义:error/aborted 红字上屏且不打
                // 用量行(错误回合无有效 usage);length 先打用量再补截断提示
                match message.stop_reason {
                    StopReason::Error => {
                        let error = message
                            .error_message
                            .clone()
                            .unwrap_or_else(|| "Unknown error".into());
                        commit_wrapped(
                            tui,
                            &format!("{}Error: {error}{}", style::RED, style::RESET),
                            width,
                        );
                    }
                    StopReason::Aborted => {
                        commit_wrapped(
                            tui,
                            &format!("{}Operation aborted{}", style::RED, style::RESET),
                            width,
                        );
                    }
                    StopReason::Length => {
                        let line = state.usage.push(&message.usage);
                        commit_wrapped(tui, &line, width);
                        commit_wrapped(
                            tui,
                            &format!(
                                "{}Response was truncated before completion.{}",
                                style::RED,
                                style::RESET
                            ),
                            width,
                        );
                        state.context_tokens = context_tokens_of(&message.usage);
                    }
                    _ => {
                        let line = state.usage.push(&message.usage);
                        commit_wrapped(tui, &line, width);
                        state.context_tokens = context_tokens_of(&message.usage);
                    }
                }
            }
            AgentSessionEvent::AgentSettled => {
                state.status = "idle".into();
            }
            AgentSessionEvent::QueueUpdate { steering, follow_up } => {
                // run 期间入队的消息给可见反馈(pi 的 pending 队列提示)
                if steering + follow_up > 0 {
                    state.status = format!("queued · steering {steering} · follow-up {follow_up}");
                }
            }
            AgentSessionEvent::AutoRetryStart { attempt, delay_ms, reason } => {
                tui.commit_lines(&[format!(
                    "{}[retry #{attempt} in {delay_ms}ms] {reason}{}",
                    style::YELLOW,
                    style::RESET
                )]);
            }
            // 重试最终失败:红字上屏(此前被吞,错误不可见)
            AgentSessionEvent::AutoRetryEnd { success: false, reason } => {
                commit_wrapped(
                    tui,
                    &format!("{}Retry failed: {reason}{}", style::RED, style::RESET),
                    width,
                );
            }
            AgentSessionEvent::AutoRetryEnd { success: true, .. } => {}
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
        UiEvent::Status(status) => {
            state.status = status;
        }
        UiEvent::CompactDone(result) => match result {
            Ok(count) => {
                state.status = format!("compacted → {count} context messages");
                commit_wrapped(
                    tui,
                    &format!("{}compacted → {count} context messages{}", style::DIM, style::RESET),
                    width,
                );
                // 压缩后上下文重建,旧估计失效;下一回合结束前显示无 ctx%
                state.context_tokens = 0;
            }
            Err(error) => {
                state.status = format!("compact failed: {error}");
                commit_wrapped(
                    tui,
                    &format!("{}compact failed: {error}{}", style::RED, style::RESET),
                    width,
                );
            }
        },
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
    // 预览行数受视口约束:状态行 + footer + 输入行之外最多 STREAM_PREVIEW_ROWS 行
    let max_preview = if viewport_height >= STREAM_PREVIEW_ROWS + 3 {
        STREAM_PREVIEW_ROWS
    } else if viewport_height >= 3 {
        1
    } else {
        0
    };
    while let Some(pos) = state.stream_buffer.find('\n') {
        let line: String = state.stream_buffer.drain(..pos + 1).collect();
        commit_wrapped(tui, line.trim_end_matches('\n'), width);
    }
    let wrapped = rpi_tui::wrap_to_width(&state.stream_buffer, width);
    if wrapped.len() > max_preview {
        // 溢出行按源字符偏移裁剪落盘(wrap_to_width 词边界折行会消费行尾
        // 空格、超宽词硬切不消费):不能用折行结果回拼 —— 硬切(CJK/无空格
        // 长文本)处插入空格会损坏上屏文本
        let overflow = wrapped.len() - max_preview;
        let mut consumed_chars = 0usize;
        for line in &wrapped[..overflow] {
            tui.commit_lines(std::slice::from_ref(line));
            consumed_chars += line.chars().count();
            let byte_pos = char_byte_offset(&state.stream_buffer, consumed_chars);
            if state.stream_buffer[byte_pos..].starts_with(' ') {
                consumed_chars += 1;
            }
        }
        let byte_pos = char_byte_offset(&state.stream_buffer, consumed_chars);
        state.stream_buffer.drain(..byte_pos);
    }
}

/// 第 `chars` 个字符的字节偏移(越界 = 文本长度)。
fn char_byte_offset(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map(|(byte, _)| byte).unwrap_or(text.len())
}

/// message 定稿:残余流式文本落盘。
fn flush_stream(state: &mut InteractiveState, tui: &mut dyn Tui, width: usize) {
    if !state.stream_buffer.is_empty() {
        let rest = std::mem::take(&mut state.stream_buffer);
        commit_wrapped(tui, &rest, width);
    }
}

/// 提交流式期间累积的 thinking 块(空则跳过)。
fn commit_pending_thinking(state: &mut InteractiveState, tui: &mut dyn Tui, width: usize) {
    if let Some(thinking) = state.pending_thinking.take() {
        commit_thinking_block(tui, &thinking, width);
    }
}

/// thinking 块渲染(dim;先折行再上样式,避免转义序列被折行截断)。
/// 对齐 pi:thinking 进转录可回看,不是只在流式预览里闪现。
fn commit_thinking_block(tui: &mut dyn Tui, thinking: &str, width: usize) {
    use rpi_tui::ansi::style;
    if thinking.trim().is_empty() {
        return;
    }
    let inner = width.saturating_sub(6).max(1);
    for line in thinking.lines() {
        if line.trim().is_empty() {
            continue;
        }
        for wrapped in rpi_tui::wrap_to_width(line, inner) {
            tui.commit_lines(&[format!("{}  ✻ {}{}", style::DIM, wrapped, style::RESET)]);
        }
    }
}

fn commit_wrapped(tui: &mut dyn Tui, line: &str, width: usize) {
    for wrapped in rpi_tui::wrap_to_width(line, width.max(1)) {
        tui.commit_lines(&[wrapped]);
    }
}

/// 视口帧:状态行 + footer + (选择列表/流式预览/thinking 预览/工具参数预览)
/// + 空行填充 + 输入行。
fn render(
    state: &InteractiveState,
    tui: &mut dyn Tui,
    partial: Option<&rpi_ai::AssistantMessage>,
    viewport_height: usize,
    width: usize,
) {
    use rpi_tui::ansi::style;
    let status = format!(
        "{}{}{}",
        style::DIM,
        truncate(&state.status, width),
        style::RESET
    );
    // footer(pi footer 核心信息):model · thinking · ctx% · 会话累计用量
    let footer = format!(
        "{}{} · t:{}{}{}{}",
        style::DIM,
        truncate(&state.model_label, width.max(1)),
        state.thinking_label,
        format_ctx_segment(state.context_window, state.context_tokens),
        state.usage.summary(),
        style::RESET
    );

    let mut lines = vec![status, footer];
    if let Some(select) = &state.select {
        lines.push(format!("{}{}", style::BOLD, truncate(&select.prompt, width)));
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
                    style::DIM,
                    truncate(line, width),
                    style::RESET
                ));
            }
        } else if let Some(args) = toolcall_preview(partial) {
            lines.push(format!(
                "{}⚙ {}{}",
                style::DIM,
                truncate(&args, width),
                style::RESET
            ));
        }
    }
    // 输入行固定在末行;中间不足补空行,超出挤掉最早的中间行(保住两行头部)
    let body_height = viewport_height.saturating_sub(1).max(1);
    lines.resize(body_height, String::new());
    while lines.len() > body_height {
        lines.remove(2.min(lines.len().saturating_sub(1)));
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
        InteractiveState::new()
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
            _ => panic!("应为 confirm"),
        }
        promote_next_select(&mut state);
        assert_eq!(state.select.as_ref().unwrap().prompt, "q2");
    }

    #[test]
    fn ctx_segment_color_thresholds_match_pi_footer() {
        // pi footer:>70% warning、>90% error;窗口未知/无用量不显示
        assert_eq!(format_ctx_segment(0, 1000), "");
        assert_eq!(format_ctx_segment(1000, 0), "");
        assert!(format_ctx_segment(1000, 500).contains("ctx "), "50% 也要显示");
        assert!(!format_ctx_segment(1000, 500).contains(rpi_tui::ansi::style::YELLOW));
        assert!(format_ctx_segment(1000, 750).contains(rpi_tui::ansi::style::YELLOW));
        assert!(format_ctx_segment(1000, 950).contains(rpi_tui::ansi::style::RED));
        assert!(format_ctx_segment(1000, 950).contains("95%"));
    }

    // ---- 错误可见性:TurnEnd stop_reason / AutoRetryEnd 渲染 ----

    fn error_assistant(message: &str) -> rpi_ai::AssistantMessage {
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let mut assistant = rpi_ai::AssistantMessage::pending(&model);
        assistant.stop_reason = StopReason::Error;
        assistant.error_message = Some(message.to_string());
        assistant
    }

    #[tokio::test]
    async fn turn_end_error_renders_red_message_without_usage_line() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 80);
        let mut state = test_state();
        let mut assistant = error_assistant("No API key for provider: anthropic");
        assistant.usage.total_tokens = 0;
        handle_ui_event(
            &mut state,
            &mut tui,
            UiEvent::Session(AgentSessionEvent::Agent(rpi_agent::AgentEvent::TurnEnd {
                message: Box::new(assistant),
                tool_results: vec![],
            })),
            6,
            80,
        );
        let text = buffer.text();
        assert!(
            text.contains(&format!("{}Error: No API key for provider: anthropic{}", rpi_tui::ansi::style::RED, rpi_tui::ansi::style::RESET)),
            "错误信息应以红字上屏: {text}"
        );
        assert!(!text.contains("[tokens]"), "错误回合不打用量行: {text}");
    }

    #[tokio::test]
    async fn turn_end_aborted_renders_operation_aborted() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 80);
        let mut state = test_state();
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let mut assistant = rpi_ai::AssistantMessage::pending(&model);
        assistant.stop_reason = StopReason::Aborted;
        handle_ui_event(
            &mut state,
            &mut tui,
            UiEvent::Session(AgentSessionEvent::Agent(rpi_agent::AgentEvent::TurnEnd {
                message: Box::new(assistant),
                tool_results: vec![],
            })),
            6,
            80,
        );
        assert!(buffer.text().contains("Operation aborted"));
    }

    #[tokio::test]
    async fn turn_end_success_still_prints_usage_and_updates_context() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 80);
        let mut state = test_state();
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let mut assistant = rpi_ai::AssistantMessage::pending(&model);
        assistant.stop_reason = StopReason::Stop;
        assistant.usage = rpi_ai::Usage {
            input: 100,
            output: 10,
            cache_read: 5,
            cache_write: 0,
            total_tokens: 110,
            ..Default::default()
        };
        handle_ui_event(
            &mut state,
            &mut tui,
            UiEvent::Session(AgentSessionEvent::Agent(rpi_agent::AgentEvent::TurnEnd {
                message: Box::new(assistant),
                tool_results: vec![],
            })),
            6,
            80,
        );
        let text = buffer.text();
        assert!(text.contains("[tokens] in 100"), "{text}");
        assert_eq!(state.context_tokens, 115, "ctx 估计 = in+out+cacheRead+cacheWrite");
    }

    #[tokio::test]
    async fn auto_retry_end_failure_renders_red() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 80);
        let mut state = test_state();
        handle_ui_event(
            &mut state,
            &mut tui,
            UiEvent::Session(AgentSessionEvent::AutoRetryEnd {
                success: false,
                reason: "provider retry".into(),
            }),
            6,
            80,
        );
        assert!(
            buffer.text().contains("Retry failed: provider retry"),
            "重试失败应上屏"
        );
    }

    #[tokio::test]
    async fn turn_end_length_prints_usage_and_truncation_notice() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 80);
        let mut state = test_state();
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let mut assistant = rpi_ai::AssistantMessage::pending(&model);
        assistant.stop_reason = StopReason::Length;
        assistant.usage.total_tokens = 55;
        assistant.usage.input = 55;
        handle_ui_event(
            &mut state,
            &mut tui,
            UiEvent::Session(AgentSessionEvent::Agent(rpi_agent::AgentEvent::TurnEnd {
                message: Box::new(assistant),
                tool_results: vec![],
            })),
            6,
            80,
        );
        let text = buffer.text();
        assert!(text.contains("[tokens]"), "{text}");
        assert!(text.contains("Response was truncated"), "{text}");
        assert_eq!(state.context_tokens, 55);
    }

    #[tokio::test]
    async fn append_stream_cjk_hard_cut_overflow_does_not_corrupt_text() {
        // P0 回归:无空格 CJK 长文本溢出落盘不得插入空格(wrap_to_width 硬切)
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 20);
        let mut state = test_state();
        let sentence: String = "汉".repeat(60); // 60 个汉字,20 列宽下每行 10 字
        append_stream(&mut state, &mut tui, &sentence, 6, 20);
        // 溢出的前 4 行(40 字)落盘,预览保留最后 2 行(20 字),无拼接空格
        assert_eq!(state.stream_buffer, "汉".repeat(20), "残余应为最后 20 字");
        let committed = buffer.text();
        assert!(!committed.contains("汉 汉"), "硬切折行处不得插入空格: {committed}");
        assert!(
            committed.matches('汉').count() >= 40,
            "前 40 字应已落盘: {committed}"
        );
        // 余下部分 flush 后同样无空格、全文完整
        flush_stream(&mut state, &mut tui, 20);
        assert!(state.stream_buffer.is_empty());
        assert_eq!(buffer.text().matches('汉').count() - committed.matches('汉').count(), 20);
    }

    #[tokio::test]
    async fn message_end_user_renders_reverse_block_live() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 40);
        let mut state = test_state();
        handle_ui_event(
            &mut state,
            &mut tui,
            UiEvent::Session(AgentSessionEvent::Agent(rpi_agent::AgentEvent::MessageEnd {
                message: Box::new(rpi_agent::AgentMessage::user("实时上屏的这一问")),
            })),
            6,
            40,
        );
        let text = buffer.text();
        assert!(text.contains("实时上屏的这一问"), "{text}");
        assert!(text.contains(rpi_tui::ansi::style::REVERSE), "user 消息应为反色块");
    }

    #[tokio::test]
    async fn compact_done_resets_context_estimate_and_reports() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 80);
        let mut state = test_state();
        state.context_tokens = 9000;
        state.status = "compacting …".into();

        handle_ui_event(
            &mut state,
            &mut tui,
            UiEvent::CompactDone(Ok(12)),
            6,
            80,
        );
        assert_eq!(state.context_tokens, 0, "压缩后 ctx 估计应重置");
        assert!(state.status.contains("compacted → 12"), "{}", state.status);
        assert!(buffer.text().contains("compacted → 12"));

        handle_ui_event(&mut state, &mut tui, UiEvent::CompactDone(Err("boom".into())), 6, 80);
        assert!(state.status.contains("compact failed"), "{}", state.status);
        assert!(buffer.text().contains("compact failed: boom"));
    }

    // ---- thinking 持久化(定稿后可回看,不随预览消失) ----

    fn assistant_start() -> rpi_agent::AgentEvent {
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        rpi_agent::AgentEvent::MessageStart {
            message: Box::new(rpi_agent::AgentMessage::Assistant(Box::new(
                rpi_ai::AssistantMessage::pending(&model),
            ))),
            partial: None,
        }
    }

    #[tokio::test]
    async fn thinking_commits_into_scrollback_when_text_starts() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 60);
        let mut state = test_state();

        handle_ui_event(&mut state, &mut tui, session_event(assistant_start()), 6, 60);
        handle_ui_event(
            &mut state,
            &mut tui,
            session_event(rpi_agent::AgentEvent::MessageDelta {
                delta: rpi_agent::MessageDeltaPayload::Thinking { delta: "思考过程第一步".into() },
            }),
            6,
            60,
        );
        // thinking 阶段:尚未落盘(在预览区)
        assert!(state.pending_thinking.as_deref() == Some("思考过程第一步"));
        handle_ui_event(
            &mut state,
            &mut tui,
            session_event(rpi_agent::AgentEvent::MessageDelta {
                delta: rpi_agent::MessageDeltaPayload::Text { delta: "正文开始".into() },
            }),
            6,
            60,
        );
        // 首个文本 delta:thinking 块落盘(✻ 标记),累积清空
        let text = buffer.text();
        assert!(text.contains("✻ 思考过程第一步"), "thinking 应进 scrollback: {text}");
        assert!(state.pending_thinking.is_none());
        handle_ui_event(
            &mut state,
            &mut tui,
            session_event(rpi_agent::AgentEvent::MessageDelta {
                delta: rpi_agent::MessageDeltaPayload::Text { delta: "更多正文\n".into() },
            }),
            6,
            60,
        );
        // 后续文本 delta 不重复提交 thinking
        let text = buffer.text();
        assert_eq!(text.matches("✻ 思考过程第一步").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn thinking_only_message_commits_at_message_end() {
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 60);
        let mut state = test_state();

        handle_ui_event(&mut state, &mut tui, session_event(assistant_start()), 6, 60);
        handle_ui_event(
            &mut state,
            &mut tui,
            session_event(rpi_agent::AgentEvent::MessageDelta {
                delta: rpi_agent::MessageDeltaPayload::Thinking { delta: "只有思考没有正文".into() },
            }),
            6,
            60,
        );
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let mut assistant = rpi_ai::AssistantMessage::pending(&model);
        assistant.stop_reason = StopReason::Stop;
        handle_ui_event(
            &mut state,
            &mut tui,
            session_event(rpi_agent::AgentEvent::MessageEnd {
                message: Box::new(rpi_agent::AgentMessage::Assistant(Box::new(assistant))),
            }),
            6,
            60,
        );
        assert!(
            buffer.text().contains("✻ 只有思考没有正文"),
            "纯 thinking 消息定稿时也应落盘: {}",
            buffer.text()
        );
        assert!(state.pending_thinking.is_none());
    }

    #[tokio::test]
    async fn replay_renders_thinking_blocks() {
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let provider = Arc::new(rpi_ai::ScriptedProvider::new(&model, vec![]));
        let built = crate::assembly::build_session(crate::assembly::BuildOptions {
            provider,
            model: model.clone(),
            ui: Arc::new(rpi_core::NoopUi),
            extension_specs: vec![],
            spawn_hook: None,
            session_store: crate::assembly::SessionStore::Memory,
        })
        .await
        .unwrap();

        let mut assistant = rpi_ai::AssistantMessage::pending(&model);
        assistant.content = vec![
            ContentBlock::Thinking {
                thinking: "回放中的思考".into(),
                thinking_signature: None,
                redacted: None,
            },
            ContentBlock::text("回放正文"),
        ];
        assistant.stop_reason = StopReason::Stop;
        built
            .session
            .agent()
            .set_messages(vec![rpi_agent::AgentMessage::Assistant(Box::new(assistant))])
            .unwrap();

        let resolver = rpi_core::create_model_resolver();
        let ctx = ctx_of(&built, &resolver);
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 60);
        let mut state = test_state();
        replay_history(&ctx, &mut state, &mut tui, 60);
        assert!(
            buffer.text().contains("✻ 回放中的思考"),
            "回放应渲染 thinking 块: {}",
            buffer.text()
        );
    }

    fn session_event(event: rpi_agent::AgentEvent) -> UiEvent {
        UiEvent::Session(AgentSessionEvent::Agent(event))
    }

    // ---- 历史回放 ----

    #[tokio::test]
    async fn replay_history_renders_transcript_into_scrollback() {
        // ScriptedProvider(无 turn)+ Memory 会话:手工注入转录后回放
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let provider = Arc::new(rpi_ai::ScriptedProvider::new(&model, vec![]));
        let built = crate::assembly::build_session(crate::assembly::BuildOptions {
            provider,
            model: model.clone(),
            ui: Arc::new(rpi_core::NoopUi),
            extension_specs: vec![],
            spawn_hook: None,
            session_store: crate::assembly::SessionStore::Memory,
        })
        .await
        .unwrap();

        let mut assistant = rpi_ai::AssistantMessage::pending(&model);
        assistant.content = vec![ContentBlock::text("回答文本")];
        assistant.stop_reason = StopReason::Stop;
        built
            .session
            .agent()
            .set_messages(vec![
                rpi_agent::AgentMessage::user("第一问"),
                rpi_agent::AgentMessage::Assistant(Box::new(assistant)),
            ])
            .unwrap();

        let resolver = rpi_core::create_model_resolver();
        let ctx = InteractiveCtx {
            session: &built.session,
            session_manager: built.session_manager.as_ref(),
            resolver: &resolver,
            ui_tx: {
                let (tx, _rx) = mpsc::unbounded_channel();
                tx
            },
        };
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 40);
        let mut state = test_state();
        replay_history(&ctx, &mut state, &mut tui, 40);

        let text = buffer.text();
        assert!(text.contains("第一问"), "user 消息应回放: {text}");
        assert!(text.contains(rpi_tui::ansi::style::REVERSE), "user 消息应为反色块");
        assert!(text.contains("回答文本"), "assistant 正文应回放: {text}");
        assert_eq!(state.context_tokens, 0, "无 usage 的转录 ctx 估计为 0");
    }

    #[tokio::test]
    async fn replay_history_renders_toolcall_result_and_error() {
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        let provider = Arc::new(rpi_ai::ScriptedProvider::new(&model, vec![]));
        let built = crate::assembly::build_session(crate::assembly::BuildOptions {
            provider,
            model: model.clone(),
            ui: Arc::new(rpi_core::NoopUi),
            extension_specs: vec![],
            spawn_hook: None,
            session_store: crate::assembly::SessionStore::Memory,
        })
        .await
        .unwrap();

        let mut assistant = rpi_ai::AssistantMessage::pending(&model);
        assistant.content = vec![ContentBlock::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        }];
        assistant.stop_reason = StopReason::ToolUse;
        let mut failed = rpi_ai::AssistantMessage::pending(&model);
        failed.stop_reason = StopReason::Error;
        failed.error_message = Some("boom".into());
        built
            .session
            .agent()
            .set_messages(vec![
                rpi_agent::AgentMessage::user("跑一下"),
                rpi_agent::AgentMessage::Assistant(Box::new(assistant)),
                rpi_agent::AgentMessage::tool_result_text("call-1", "bash", "a.txt\nb.txt", false),
                rpi_agent::AgentMessage::Assistant(Box::new(failed)),
            ])
            .unwrap();

        let resolver = rpi_core::create_model_resolver();
        let ctx = InteractiveCtx {
            session: &built.session,
            session_manager: built.session_manager.as_ref(),
            resolver: &resolver,
            ui_tx: {
                let (tx, _rx) = mpsc::unbounded_channel();
                tx
            },
        };
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 40);
        let mut state = test_state();
        replay_history(&ctx, &mut state, &mut tui, 40);

        let text = buffer.text();
        assert!(text.contains("⏺ bash"), "工具调用应回放: {text}");
        assert!(text.contains("bash: a.txt"), "工具结果首行应回放: {text}");
        assert!(text.contains("Error: boom"), "历史错误消息应回放: {text}");
    }

    // ---- 斜杠命令执行 ----

    async fn built_memory_session() -> crate::assembly::BuiltSession {
        let model = rpi_ai::Model::minimal("m", "mock", "mock");
        crate::assembly::build_session(crate::assembly::BuildOptions {
            provider: Arc::new(rpi_ai::ScriptedProvider::new(&model, vec![])),
            model,
            ui: Arc::new(rpi_core::NoopUi),
            extension_specs: vec![],
            spawn_hook: None,
            session_store: crate::assembly::SessionStore::Memory,
        })
        .await
        .unwrap()
    }

    fn ctx_of<'a>(built: &'a crate::assembly::BuiltSession, resolver: &'a rpi_core::ModelResolver) -> InteractiveCtx<'a> {
        InteractiveCtx {
            session: &built.session,
            session_manager: built.session_manager.as_ref(),
            resolver,
            ui_tx: {
                let (tx, _rx) = mpsc::unbounded_channel();
                tx
            },
        }
    }

    #[tokio::test]
    async fn execute_help_and_session_commit_lines() {
        let built = built_memory_session().await;
        let resolver = rpi_core::create_model_resolver();
        let ctx = ctx_of(&built, &resolver);
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 10, 80);
        let mut state = test_state();

        let quit = execute_command(&ctx, &mut state, &mut tui, slash::SlashAction::Help, 80).await;
        assert!(!quit);
        assert!(buffer.text().contains("/help"));

        let quit = execute_command(&ctx, &mut state, &mut tui, slash::SlashAction::Session, 80).await;
        assert!(!quit);
        let text = buffer.text();
        assert!(text.contains("session"), "{text}");
        assert!(text.contains("model:"), "{text}");
        assert!(text.contains("usage:"), "{text}");
    }

    #[tokio::test]
    async fn execute_thinking_with_arg_updates_footer() {
        let built = built_memory_session().await;
        let resolver = rpi_core::create_model_resolver();
        let ctx = ctx_of(&built, &resolver);
        let mut state = test_state();

        execute_command(
            &ctx,
            &mut state,
            &mut NullTui,
            slash::SlashAction::Thinking { arg: Some("high".into()) },
            80,
        )
        .await;
        assert_eq!(state.thinking_label, "high");
        assert!(state.status.contains("thinking → high"), "{}", state.status);

        execute_command(
            &ctx,
            &mut state,
            &mut NullTui,
            slash::SlashAction::Thinking { arg: Some("off".into()) },
            80,
        )
        .await;
        assert_eq!(state.thinking_label, "off");

        // 非法级别:本地红字,状态不变
        execute_command(
            &ctx,
            &mut state,
            &mut NullTui,
            slash::SlashAction::Thinking { arg: Some("ultra".into()) },
            80,
        )
        .await;
        assert_eq!(state.thinking_label, "off");
    }

    #[tokio::test]
    async fn execute_model_with_arg_updates_footer() {
        let built = built_memory_session().await;
        let resolver = rpi_core::create_model_resolver();
        let ctx = ctx_of(&built, &resolver);
        let mut state = test_state();

        execute_command(
            &ctx,
            &mut state,
            &mut NullTui,
            slash::SlashAction::Model { arg: Some("deepseek/deepseek-chat".into()) },
            80,
        )
        .await;
        assert_eq!(state.model_label, "deepseek/deepseek-chat");
        assert!(state.status.contains("model → deepseek"), "{}", state.status);

        // 未知 provider:本地红字,状态不变
        execute_command(
            &ctx,
            &mut state,
            &mut NullTui,
            slash::SlashAction::Model { arg: Some("nope/model".into()) },
            80,
        )
        .await;
        assert_eq!(state.model_label, "deepseek/deepseek-chat");
    }

    #[tokio::test]
    async fn execute_model_without_arg_opens_selector_at_current_model() {
        let built = built_memory_session().await;
        let resolver = rpi_core::create_model_resolver();
        let ctx = ctx_of(&built, &resolver);
        let mut state = test_state();
        // 先切到内置 provider 的默认模型,选择器应定位高亮当前模型
        execute_command(
            &ctx,
            &mut state,
            &mut NullTui,
            slash::SlashAction::Model { arg: Some("deepseek/deepseek-chat".into()) },
            80,
        )
        .await;

        execute_command(&ctx, &mut state, &mut NullTui, slash::SlashAction::Model { arg: None }, 80).await;
        let select = state.select.as_ref().expect("应打开模型选择器");
        assert!(!select.list.options.is_empty(), "内置 provider 默认模型应入列");
        assert_eq!(
            select.list.options.get(select.list.selected).map(String::as_str),
            Some("deepseek/deepseek-chat"),
            "当前模型应高亮"
        );

        // Enter 应用选择(换到另一个模型)
        let target = if select.list.selected == 0 { 1 } else { 0 };
        let request = state.select.take().unwrap();
        let mut list = request.list;
        list.selected = target;
        match request.kind {
            SelectKind::Model { models } => {
                if let Some(model) = models.get(list.selected) {
                    ctx.session.set_model(model.clone());
                    refresh_footer(&ctx, &mut state);
                }
            }
            _ => panic!("应为 model 选择器"),
        }
        assert_ne!(state.model_label, "deepseek/deepseek-chat", "选择后 footer 应刷新");
    }

    #[tokio::test]
    async fn unknown_slash_input_is_local_warning_not_prompt() {
        let built = built_memory_session().await;
        let resolver = rpi_core::create_model_resolver();
        let ctx = ctx_of(&built, &resolver);
        let buffer = SharedVec::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 6, 80);
        let mut state = test_state();

        state.editor.set_text("/definitely-not-a-command");
        let quit = handle_key(&ctx, &mut state, &mut tui, Key::Enter, 80).await;
        assert!(!quit);
        assert!(
            buffer.text().contains("Unknown command: /definitely-not-a-command"),
            "未知命令应本地警告: {}",
            buffer.text()
        );
        assert_eq!(state.status, "idle", "不应把未知命令发给模型");
    }

    #[tokio::test]
    async fn double_ctrl_c_exits_and_single_press_hints() {
        let built = built_memory_session().await;
        let resolver = rpi_core::create_model_resolver();
        let ctx = ctx_of(&built, &resolver);
        let mut state = test_state();

        // 首按:提示,不退出
        let quit = handle_key(&ctx, &mut state, &mut NullTui, Key::Ctrl('c'), 80).await;
        assert!(!quit);
        assert_eq!(state.status, "press Ctrl+C again to exit");
        // 500ms 内再按:退出
        let quit = handle_key(&ctx, &mut state, &mut NullTui, Key::Ctrl('c'), 80).await;
        assert!(quit);

        // 超过窗口:重新计时
        let mut state = test_state();
        state.last_ctrl_c = Some(Instant::now() - Duration::from_millis(600));
        let quit = handle_key(&ctx, &mut state, &mut NullTui, Key::Ctrl('c'), 80).await;
        assert!(!quit);
    }

    /// 渲染 no-op(tui 断言由其余用例覆盖)。
    struct NullTui;
    impl Tui for NullTui {
        fn commit_lines(&mut self, _lines: &[String]) {}
        fn render_viewport(&mut self, _lines: &[String], _cursor_col: usize) {}
        fn finish(&mut self) {}
    }
}
