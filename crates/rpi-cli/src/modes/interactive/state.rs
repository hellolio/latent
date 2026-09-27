//! interactive 模式的 UI 状态机:编辑器、会话状态、用量、选择列表、
//! 转录模型(TranscriptItem)。全部与终端 I/O 解耦 —— 渲染在 view.rs
//! 按 `theme + width + expanded` 从状态推导,handlers.rs 只改状态。

use std::collections::VecDeque;
use std::time::Instant;

use rpi_tui::{CommandEntry, CommandPopup, Editor, Key, SelectList, Theme, UiLine};

use super::usage::UsageTracker;

/// 会话运行状态(状态行样式与 spinner 动画的依据)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Thinking,
    Tool(String),
    Aborted,
    Compacting,
    /// `!` bash 执行中(标题=命令)
    Bash(String),
}

impl Status {
    pub fn is_busy(&self) -> bool {
        !matches!(self, Status::Idle)
    }
}

/// ctrl+o 可切换展开状态的转录条目(pi renderSessionItems 的对应物)。
#[derive(Debug, Clone)]
pub enum TranscriptItem {
    /// 预渲染行(横幅、用量、错误、通知等一次性内容)
    Line(UiLine),
    User {
        content: String,
    },
    /// assistant 定稿正文(markdown 渲染)。`boxed` = 纯面向用户的输出
    /// (消息不含工具调用)才包 AI 输出框;后续要执行命令的不加框。
    Assistant {
        markdown: String,
        boxed: bool,
    },
    Thinking {
        text: String,
    },
    /// 工具调用标题行(状态着色)
    ToolCall {
        name: String,
        args: String,
        status: ToolStatus,
    },
    /// 工具输出(默认折叠 COLLAPSED_OUTPUT_ROWS 行,ctrl+o 展开)
    ToolResult {
        output: String,
        is_error: bool,
    },
    /// `!` bash 透传记录
    Bash {
        command: String,
        output: String,
        is_error: bool,
    },
    Blank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Success,
    Error,
}

impl From<ToolStatus> for rpi_tui::tool_card::ToolStatus {
    fn from(status: ToolStatus) -> Self {
        match status {
            ToolStatus::Pending => rpi_tui::tool_card::ToolStatus::Pending,
            ToolStatus::Success => rpi_tui::tool_card::ToolStatus::Success,
            ToolStatus::Error => rpi_tui::tool_card::ToolStatus::Error,
        }
    }
}

pub struct SelectRequest {
    pub prompt: String,
    pub list: SelectList,
    pub kind: SelectKind,
}

pub enum SelectKind {
    Confirm(tokio::sync::oneshot::Sender<bool>),
    Select(tokio::sync::oneshot::Sender<Option<usize>>),
    /// 内部选择器(/model):Enter 应用选择,无 responder
    Model {
        models: Vec<rpi_ai::Model>,
    },
    /// 内部选择器(/thinking)
    Thinking,
    /// 内部选择器(/theme):携带候选主题枚举
    Theme { names: Vec<rpi_tui::ThemeName> },
}

pub struct InteractiveState {
    pub theme: Theme,
    /// 当前主题名(kebab-case;ANSI 兜底时为 None,`/theme` 列表定位用)
    pub theme_name: Option<String>,
    /// 终端显示宽度(Resize 时刷新;渲染/折行的唯一宽度来源)
    pub width: usize,
    pub editor: Editor,
    /// 斜杠命令补全弹窗(Codex 风格):输入 `/xxx` 时跟随编辑器内容过滤,
    /// 可见性与选中态由 `sync_slash_popup` 从编辑器文本推导。
    pub slash_popup: CommandPopup,
    pub status: Status,
    /// spinner 拍数(busy 时每 120ms 自增)
    pub spin: usize,
    /// 流式中的 assistant 文本(预览区尾部展示,定稿时整体转 Assistant)
    pub stream_text: String,
    /// 流式中的 thinking 累积(预览尾部;首个文本 delta 时提交进转录)
    pub pending_thinking: Option<String>,
    pub usage: UsageTracker,
    /// 活动选择列表;None = 无交互请求
    pub select: Option<SelectRequest>,
    /// 并发 UI 请求排队(先到先渲染)
    pub select_queue: VecDeque<SelectRequest>,
    pub model_label: String,
    pub thinking_label: String,
    pub context_window: u64,
    pub context_tokens: u64,
    /// 双击 Ctrl+C 退出:上一次 Ctrl+C 时刻(非流式期间)
    pub last_ctrl_c: Option<Instant>,
    /// ctrl+o 全局展开(工具输出 + 启动帮助/资源)
    pub expanded: bool,
    /// 全文重绘请求(ctrl+o 切换后由事件循环消费)
    pub needs_full_redraw: bool,
    /// 转录模型(redraw_full 重渲染的数据源)
    pub transcript: Vec<TranscriptItem>,
    /// 待提交进 scrollback 的行(事件循环每轮 flush 后清空)
    pub pending: Vec<UiLine>,
    /// footer:auto-compact 开关(/compact 后由上层设置)
    pub auto_compact: bool,
    /// footer:cwd(已做 ~ 缩写)
    pub cwd_display: String,
    /// footer:git 分支(启动时探测)
    pub git_branch: Option<String>,
    /// 已加载资源分节(启动区;ctrl+o 重渲染数据源)
    pub resources: Vec<(String, Vec<String>)>,
    /// 执行中的工具(ToolExecutionStart → 结果到达时标题随终态着色落盘)
    pub current_tool: Option<(String, String)>,
    /// 最近一次工具执行的错误标记
    pub last_tool_error: bool,
}

impl InteractiveState {
    pub fn new(theme: Theme, width: usize) -> Self {
        InteractiveState {
            theme,
            theme_name: None,
            width,
            editor: Editor::new(),
            slash_popup: CommandPopup::new(
                crate::modes::slash::COMMANDS
                    .iter()
                    .map(|command| CommandEntry::new(command.name, command.description))
                    .collect(),
            ),
            status: Status::Idle,
            spin: 0,
            stream_text: String::new(),
            pending_thinking: None,
            usage: UsageTracker::default(),
            select: None,
            select_queue: VecDeque::new(),
            model_label: "—".into(),
            thinking_label: "off".into(),
            context_window: 0,
            context_tokens: 0,
            last_ctrl_c: None,
            expanded: false,
            needs_full_redraw: false,
            transcript: Vec::new(),
            pending: Vec::new(),
            auto_compact: true, // 自动压缩已在 session 层接线(阈值触发)
            cwd_display: String::new(),
            git_branch: None,
            current_tool: None,
            last_tool_error: false,
            resources: Vec::new(),
        }
    }

    /// 追加转录条目并渲染进待提交缓冲。
    pub fn commit(&mut self, item: TranscriptItem) {
        self.commit_many(vec![item]);
    }

    /// 提交空行(去重:上一条已是空行则跳过,避免排版出现连续空行)。
    pub fn commit_blank(&mut self) {
        self.commit(TranscriptItem::Blank);
    }

    /// 追加多条转录条目(单条历史消息可展开成多个条目)。组间间距由空行
    /// 条目表达(提交方负责插入,commit_blank 去重保证不双写)。
    pub fn commit_many(&mut self, items: Vec<TranscriptItem>) {
        for item in items {
            // 空行去重:上一条已是空行则跳过(实时与回放路径统一生效)
            if matches!(item, TranscriptItem::Blank)
                && matches!(self.transcript.last(), Some(TranscriptItem::Blank))
            {
                continue;
            }
            let lines = super::view::render_item(&item, &self.theme, self.width, self.expanded);
            self.transcript.push(item);
            self.pending.extend(lines);
        }
    }

    /// 直接提交一行(进转录,重绘保留)。
    pub fn commit_line(&mut self, line: UiLine) {
        self.commit(TranscriptItem::Line(line));
    }

    /// 启动区行(只进待提交缓冲,不进转录;ctrl+o 时按展开态重新组装)。
    pub fn commit_startup(&mut self, lines: Vec<UiLine>) {
        self.pending.extend(lines);
    }

    /// 提交一行但不进转录(错误提示等无需参与 ctrl+o 重绘的短消息)。
    pub fn commit_ephemeral(&mut self, line: UiLine) {
        self.pending.push(line);
    }

    /// 无活动选择列表时提升队首请求。
    pub fn promote_next_select(&mut self) {
        if self.select.is_none() {
            self.select = self.select_queue.pop_front();
        }
    }

    /// /new 切换会话后复位会话相关的瞬态:转录区、流式缓冲、用量与 ctx 估计
    /// (footer 字段由调用方 refresh_footer 从 Agent 状态重新读取)。
    pub fn reset_for_new_session(&mut self) {
        self.transcript.clear();
        self.pending.clear();
        self.stream_text.clear();
        self.pending_thinking = None;
        self.current_tool = None;
        self.last_tool_error = false;
        self.usage = UsageTracker::default();
        self.context_tokens = 0;
        self.status = Status::Idle;
        self.needs_full_redraw = true;
    }

    /// 消费历史:Enter 提交后的文本(含多行)。
    pub fn take_input(&mut self) -> Option<String> {
        let text = self.editor.text().trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.editor.commit_history();
        self.editor.clear();
        self.last_ctrl_c = None;
        Some(text)
    }

    /// 未消费按键交给编辑器(便于测试复用)。
    pub fn editor_key(&mut self, key: &Key) {
        self.editor.handle_key(key);
    }

    /// 编辑器内容变化后同步补全弹窗(过滤、可见性、选中复位)。
    pub fn sync_slash_popup(&mut self) {
        let text = self.editor.text();
        self.slash_popup.sync(&text);
    }
}
