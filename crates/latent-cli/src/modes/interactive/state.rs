//! interactive 模式的 UI 状态机:编辑器、会话状态、用量、选择列表、
//! 转录模型(TranscriptItem)。全部与终端 I/O 解耦 —— 渲染在 view.rs
//! 按 `theme + width + expanded` 从状态推导,handlers.rs 只改状态。

use std::collections::VecDeque;
use std::time::Instant;

use latent_tui::{CommandPopup, Editor, FilePopup, Key, SelectList, Theme, UiLine};

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
    /// assistant 定稿正文(markdown 裸渲染,无外框无背景色)。
    Assistant { markdown: String },
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
    /// 计划模式产出的 `<proposed_plan>` 块(边框卡片,13 文档 §8.3)
    Plan { markdown: String },
    Blank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Success,
    Error,
}

impl From<ToolStatus> for latent_tui::tool_card::ToolStatus {
    fn from(status: ToolStatus) -> Self {
        match status {
            ToolStatus::Pending => latent_tui::tool_card::ToolStatus::Pending,
            ToolStatus::Success => latent_tui::tool_card::ToolStatus::Success,
            ToolStatus::Error => latent_tui::tool_card::ToolStatus::Error,
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
        models: Vec<latent_ai::Model>,
    },
    /// 内部选择器(/thinking)
    Thinking,
    /// 内部选择器(/theme):携带候选主题枚举
    Theme { names: Vec<latent_tui::ThemeName> },
    /// 内部选择器(/setting):全屏模式 / 复制快捷键 / 鼠标选中复制
    Setting,
    /// 内部选择器(/subagent):候选 agent 定义
    SubagentAgent { defs: Vec<latent_core::AgentDef> },
    /// 内部选择器(/session):候选历史会话(mtime 倒序)
    Session { files: Vec<latent_session::SessionSummary> },
    /// 权限审批(13 文档 §10.3):四决策经 oneshot 回传 ApprovalHooks
    Approval {
        responder: tokio::sync::oneshot::Sender<latent_core::ApprovalDecision>,
    },
    /// /model「添加模型」表单中途的 api 协议选择(选择列表覆盖在表单上,
    /// Enter 后写回 `state.model_form`)
    ModelApiChoice,
    /// /model「添加模型」表单第一步的写入位置选择(项目/全局)
    ModelTargetChoice,
}

/// /model「添加模型」表单当前步骤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFormStep {
    /// 第一步:写入项目还是全局 models.json(选择列表)
    Target,
    Provider,
    /// 仅新 provider:api 协议(选择列表)
    Api,
    /// 仅新 provider:baseUrl
    BaseUrl,
    /// 仅新 provider:apiKey 环境变量名
    ApiKeyEnv,
    ModelId,
}

/// 配置写回目标(项目 `.latent/models.json` 或全局 `~/.latent/models.json`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFormTarget {
    Project,
    Global,
}

/// /model「添加模型」交互式表单:主编辑器作输入,Enter 提交当前步骤,
/// Esc 取消整张表单。provider 已存在(builtin 或 models.json 已声明)时
/// 跳过 api/baseUrl/apiKey 步骤。
pub struct ModelForm {
    pub target: Option<ModelFormTarget>,
    pub provider_id: Option<String>,
    pub api: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub model_id: Option<String>,
    pub known_provider: bool,
    pub step: ModelFormStep,
}

impl ModelForm {
    pub fn new() -> Self {
        ModelForm {
            target: None,
            provider_id: None,
            api: None,
            base_url: None,
            api_key_env: None,
            model_id: None,
            known_provider: false,
            step: ModelFormStep::Target,
        }
    }

    /// 当前步骤的提问文本(view 渲染)。
    pub fn question(&self) -> String {
        match self.step {
            ModelFormStep::Target => "添加模型 · 写入位置(项目/全局)".into(),
            ModelFormStep::Provider => {
                "添加模型 · provider id(内置名或自定义名,Esc 取消)".into()
            }
            ModelFormStep::Api => "添加模型 · api 协议".into(),
            ModelFormStep::BaseUrl => "添加模型 · baseUrl(如 https://host/v1)".into(),
            ModelFormStep::ApiKeyEnv => {
                "添加模型 · apiKey(输入环境变量名,或直接粘贴密钥;留空跳过)".into()
            }
            ModelFormStep::ModelId => "添加模型 · 模型 id(写入 models.json 并切换)".into(),
        }
    }

    /// 表单字段固定清单(view 渲染):全部字段恒显示,未配置为 None。
    pub fn fields(&self) -> Vec<(&'static str, Option<&str>)> {
        vec![
            ("provider", self.provider_id.as_deref()),
            ("api", self.api.as_deref()),
            ("baseUrl", self.base_url.as_deref()),
            ("apiKey", self.api_key_env.as_deref()),
            ("model", self.model_id.as_deref()),
        ]
    }
}

/// 事件循环代办的挂起动作(handlers 不直接持有 TuiApp,置标记由事件循环
/// 在 TUI 挂起期间执行)。
pub enum SuspendAction {
    /// 用 $EDITOR 打开 models.json,返回后热重载并重开 /model 选择器
    EditModelsJson,
}

/// 全屏模式的滚动请求(handlers 不持有 TuiApp,置标记由事件循环转交)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollRequest {
    PageUp,
    PageDown,
    Top,
    Bottom,
    /// 有向滚动 n 行(正数向下、负数向上;鼠标滚轮)
    Lines(isize),
}

/// 流式文本增量折行缓存(全屏滚动视口的预览全量行数据源)。正文流式
/// 期间文本 append-only,而 `wrap_to_width` 对各源行独立折行,故追加时
/// 只需重折最后一个未完成源行;宽度变化或文本收缩(flush/新回合)时
/// 全量重建。空文本 = 空行集(区别于 `wrap_to_width` 的单行空串)。
#[derive(Default)]
pub struct StreamWrapCache {
    /// 已折完的源字节偏移(其后是待续的最后一个源行;恒位于行边界)
    done: usize,
    width: usize,
    lines: Vec<String>,
    /// `lines` 末尾属于待续行的行数(待续行可折成多行,追加时整体截断重折)
    pending_lines: usize,
}

impl StreamWrapCache {
    /// 按当前文本与宽度刷新缓存,返回全量折行(每行 ≤ width 显示宽)。
    pub fn update(&mut self, src: &str, width: usize) -> &[String] {
        let width = width.max(1);
        if self.width != width || src.len() < self.done {
            self.done = 0;
            self.lines.clear();
            self.pending_lines = 0;
            self.width = width;
        }
        if src.is_empty() {
            self.done = 0;
            self.lines.clear();
            self.pending_lines = 0;
            return &self.lines;
        }
        if src.len() > self.done {
            // 追加:从上一轮的待续行起点重折。先截断待续行的旧折行(可能
            // 多行),逐个折出已完成源行(以 '\n' 结尾),最后重折当前待续
            // 行(空也算一行,与 wrap_to_width 的全量折行逐行一致)
            let tail = &src[self.done..];
            let keep = self.lines.len().saturating_sub(self.pending_lines);
            self.lines.truncate(keep);
            let mut consumed = 0usize;
            for piece in tail.split_inclusive('\n') {
                match piece.strip_suffix('\n') {
                    Some(raw) => {
                        for row in latent_tui::wrap_to_width(raw, width) {
                            self.lines.push(row);
                        }
                        consumed += piece.len();
                    }
                    None => break,
                }
            }
            self.done += consumed;
            self.pending_lines = 0;
            for row in latent_tui::wrap_to_width(&tail[consumed..], width) {
                self.lines.push(row);
                self.pending_lines += 1;
            }
        }
        &self.lines
    }
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
    /// `@` 文件选择弹窗(对齐 pi 的 @ autocomplete):光标前 token 以 `@`
    /// 开头时跟随过滤,可见性由 `sync_mention_popup` 推导;候选集在首次
    /// 激活时从 `cwd` 采集(mention_loaded 标记),失活即清缓存。
    pub mention_popup: FilePopup,
    mention_loaded: bool,
    /// @ 弹窗候选采集的检索忽略列表(装配层注入,与 grep/find/ls 同源)。
    pub mention_ignore: std::sync::Arc<latent_tools::SearchIgnore>,
    pub status: Status,
    /// spinner 拍数(busy 时每 120ms 自增)
    pub spin: usize,
    /// 流式中的 assistant 文本(预览区尾部展示,定稿时整体转 Assistant)
    pub stream_text: String,
    /// 流式文本的全量折行缓存(`StreamWrapCache`,RefCell 让 `view::viewport`
    /// 保持 `&state` 签名自刷新;UI 单线程)。文本收缩(flush/新回合)时
    /// 缓存经 len < done 判据自动重建,无需手动清理
    pub stream_wrap: std::cell::RefCell<StreamWrapCache>,
    /// 流式中的 thinking 累积(预览尾部;首个文本 delta 时提交进转录)
    pub pending_thinking: Option<String>,
    pub usage: UsageTracker,
    /// 当前回合流式计时(MessageStart 起点与首个 delta 时刻):TurnEnd 时
    /// 换算 TTFT 与 TPS。无 tokio 依赖,全部本地 Instant。
    pub stream_started: Option<Instant>,
    pub first_delta_at: Option<Instant>,
    /// 最近一回合输出速度(tok/s)与首 token 延迟(秒),用量行展示
    pub last_tps: Option<f64>,
    pub last_ttft: Option<f64>,
    /// 活动选择列表;None = 无交互请求
    pub select: Option<SelectRequest>,
    /// 并发 UI 请求排队(先到先渲染)
    pub select_queue: VecDeque<SelectRequest>,
    pub model_label: String,
    pub thinking_label: String,
    /// footer:会话模式标记(13 文档 §10.3)
    pub mode_label: String,
    pub context_window: u64,
    pub context_tokens: u64,
    /// 双击 Ctrl+C 退出:上一次 Ctrl+C 时刻(非流式期间)
    pub last_ctrl_c: Option<Instant>,
    /// ctrl+o 全局展开(工具输出 + 启动帮助/资源)
    pub expanded: bool,
    /// 当前激活的平行子 agent(/subagent 切换;None = 主会话)
    pub active_agent: Option<String>,
    /// 存活后台 subagent 数(>0 时驱动 tick 并在 footer 显示)
    pub subagent_active: usize,
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
    /// 执行中的工具(ToolExecutionStart → 结果到达时标题随终态着色落盘)。
    /// 按 tool_call_id 配对:并行批的 start 事件全部先于结果消息发出,
    /// 结果消息还可能按完成序到达,单槽"最近一次 start"会配错对。
    /// (tool_call_id, name, args)
    pub pending_tools: Vec<(String, String, String)>,
    /// 执行中工具的实时输出尾窗(ToolExecutionUpdate 持续替换;结果到达
    /// 即清除,由折叠的 ToolResult 定稿)。并行批显示最近更新者。
    /// (tool_call_id, tail)
    pub pending_tool_output: Option<(String, String)>,
    /// 最近一次工具执行的错误标记
    pub last_tool_error: bool,
    /// /model「添加模型」表单(None = 未激活)
    pub model_form: Option<ModelForm>,
    /// 选中后自动复制(/setting 开关,默认关;选择/高亮/快捷键复制恒可用)
    pub copy_on_select: bool,
    /// Ctrl+X 复制开关(/setting 开关,默认开;关闭后有选区时 Ctrl+X 也不复制)
    pub ctrl_x_copy: bool,
    /// 待事件循环挂起 TUI 执行的动作(置位后由事件循环取走)
    pub suspend_action: Option<SuspendAction>,
    /// 当前是否全屏渲染模式(事件循环每轮从 TuiApp 同步;滚动按键与
    /// /fullscreen 的判据)
    pub fullscreen: bool,
    /// 全屏视口是否贴住最新内容(事件循环每轮从 TuiApp 同步;view 据此
    /// 决定是否构建预览全量行 —— follow 帧钉底布局用不到,省 O(n) 重建)
    pub following: bool,
    /// 待事件循环转交 TuiApp 的滚动请求(全屏模式)
    pub scroll_request: Option<ScrollRequest>,
    /// 待事件循环执行的渲染模式切换目标(true = 全屏;Some 时由事件循环
    /// set_fullscreen + 全量重绘)
    pub tui_mode_switch: Option<bool>,
    /// /model 配置入口:工作目录与 HOME(models.json 目标路径与热重载推导)
    pub cwd: std::path::PathBuf,
    pub home: Option<std::path::PathBuf>,
}

impl InteractiveState {
    pub fn new(theme: Theme, width: usize) -> Self {
        InteractiveState {
            theme,
            theme_name: None,
            width,
            editor: Editor::new(),
            slash_popup: CommandPopup::new(crate::modes::slash::popup_entries()),
            mention_popup: FilePopup::default(),
            mention_loaded: false,
            mention_ignore: std::sync::Arc::new(latent_tools::SearchIgnore::builtin()),
            status: Status::Idle,
            spin: 0,
            active_agent: None,
            subagent_active: 0,
            stream_text: String::new(),
            stream_wrap: std::cell::RefCell::new(StreamWrapCache::default()),
            pending_thinking: None,
            usage: UsageTracker::default(),
            stream_started: None,
            first_delta_at: None,
            last_tps: None,
            last_ttft: None,
            select: None,
            select_queue: VecDeque::new(),
            model_label: "—".into(),
            thinking_label: "off".into(),
            mode_label: "plan".into(),
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
            pending_tools: Vec::new(),
            pending_tool_output: None,
            last_tool_error: false,
            resources: Vec::new(),
            model_form: None,
            copy_on_select: false,
            ctrl_x_copy: true,
            suspend_action: None,
            fullscreen: false,
            following: false,
            scroll_request: None,
            tui_mode_switch: None,
            cwd: std::env::current_dir().unwrap_or_default(),
            home: std::env::var_os("HOME").map(std::path::PathBuf::from),
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
        self.pending_tools.clear();
        self.pending_tool_output = None;
        self.last_tool_error = false;
        self.usage = UsageTracker::default();
        self.stream_started = None;
        self.first_delta_at = None;
        self.last_tps = None;
        self.last_ttft = None;
        self.context_tokens = 0;
        self.status = Status::Idle;
        self.needs_full_redraw = true;
    }

    /// 消费历史:Enter 提交后的文本(含多行;粘贴占位符展开为完整内容)。
    pub fn take_input(&mut self) -> Option<String> {
        let text = self.editor.expanded_text().trim().to_string();
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

    /// 编辑器内容变化后同步 `@` 文件弹窗:光标前 token 以 `@` 开头时激活
    /// (缓冲以 `/`、`!` 开头时让位——斜杠命令与 bang 透传无提及语义);
    /// 首次激活从 cwd 采集候选并缓存、注入补全根(选中补全绝对全路径,
    /// 手输文本原样),失活即清缓存,下次激活重新采集(感知会话期间的
    /// 文件增删)。
    pub fn sync_mention_popup(&mut self) {
        let text = self.editor.text();
        let token = self.editor.token_before_cursor().map(|(_, token)| token);
        let active = token.as_deref().is_some_and(|t| t.starts_with('@'))
            && !text.starts_with('/')
            && !text.starts_with('!');
        if !active {
            self.mention_popup.sync(None);
            if self.mention_loaded {
                self.mention_popup.clear_entries();
                self.mention_loaded = false;
            }
            return;
        }
        if !self.mention_loaded {
            // 补全根 = cwd:弹窗选中插入绝对全路径(与手输相区分),
            // 过滤时组件自动剥离该前缀
            self.mention_popup.set_base(self.cwd.display().to_string());
            let entries = latent_tools::collect_entries(&self.cwd, &self.mention_ignore);
            self.mention_popup.set_entries(
                entries
                    .into_iter()
                    .map(|entry| latent_tui::FileEntry::new(entry.path, entry.is_dir))
                    .collect(),
            );
            self.mention_loaded = true;
        }
        self.mention_popup.sync(token.as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_wrap_cache_incremental_matches_one_shot() {
        let mut cache = StreamWrapCache::default();
        let full = "first line here\nsecond\n\n fourth with  several words to wrap ok\n";
        // 逐块追加,与一次性全量折行逐行一致
        let mut src = String::new();
        for chunk in full.as_bytes().chunks(3) {
            src.push_str(std::str::from_utf8(chunk).unwrap());
            let lines = cache.update(&src, 12).to_vec();
            assert_eq!(lines, latent_tui::wrap_to_width(&src, 12), "src={src:?}");
        }
    }

    #[test]
    fn stream_wrap_cache_empty_is_empty() {
        let mut cache = StreamWrapCache::default();
        assert!(cache.update("", 40).is_empty());
        // 空文本 ≠ 单行空串(区别于 wrap_to_width)
        cache.update("x", 40);
        assert_eq!(cache.update("", 40).len(), 0);
    }

    #[test]
    fn stream_wrap_cache_rebuilds_on_width_change_and_shrink() {
        let mut cache = StreamWrapCache::default();
        let src = "some reasonably long line to be wrapped at narrow width";
        cache.update(src, 80);
        let wide: Vec<String> = cache.update(src, 80).to_vec();
        let narrow = cache.update(src, 20).to_vec();
        assert_eq!(narrow, latent_tui::wrap_to_width(src, 20));
        assert_ne!(narrow, wide);
        // 收缩(flush take)→ 空
        cache.update("tail text", 20);
        assert_eq!(cache.update("", 20).len(), 0);
        // 重新追加正常工作
        assert_eq!(
            cache.update("tail text", 20),
            latent_tui::wrap_to_width("tail text", 20)
        );
    }
}
