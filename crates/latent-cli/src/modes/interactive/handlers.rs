//! 事件处理:按键、斜杠命令、session/UI 事件 → 状态变更 + 转录提交。
//! 全部通过 `InteractiveState` 间接渲染(纯状态机,可直接单测)。

use std::time::{Duration, Instant};

use latent_ai::StopReason;
use latent_core::{AgentSessionEvent, ModelResolver};
use latent_tui::{SelectList, Theme};
use tokio::sync::mpsc;

use crate::modes::slash;

use super::bash;
use super::events::UiEvent;
use super::replay;
use super::state::{
    instant_from_ms, InteractiveState, ScrollRequest, SelectKind, SelectRequest, Status, ToolStatus,
    TranscriptItem,
};
use super::usage::{context_tokens_of, usage_line};
use super::view;

/// 键盘/命令处理共用的会话上下文(状态 + TUI 之外的全部依赖)。
/// 会话路由(/subagent 平行会话):`current()` 恒返回当前激活会话,主会话
/// 为初值。切换只换指针 —— 各会话上下文完全隔离,平行会话按名缓存
/// (切走再切回,上下文保留)。
pub struct SessionRouter {
    main: Arc<latent_core::AgentSession>,
    sessions: std::sync::Mutex<std::collections::HashMap<String, Arc<latent_core::AgentSession>>>,
    current: std::sync::RwLock<Arc<latent_core::AgentSession>>,
    active_agent: std::sync::Mutex<Option<String>>,
}

impl SessionRouter {
    pub fn new(main: Arc<latent_core::AgentSession>) -> Self {
        SessionRouter {
            current: std::sync::RwLock::new(main.clone()),
            main,
            sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
            active_agent: std::sync::Mutex::new(None),
        }
    }
    pub fn current(&self) -> Arc<latent_core::AgentSession> {
        self.current.read().unwrap().clone()
    }
    pub fn main(&self) -> Arc<latent_core::AgentSession> {
        self.main.clone()
    }
    pub fn is_main(&self) -> bool {
        self.active_agent.lock().unwrap().is_none()
    }
    pub fn active_agent(&self) -> Option<String> {
        self.active_agent.lock().unwrap().clone()
    }
    pub fn cached(&self, name: &str) -> Option<Arc<latent_core::AgentSession>> {
        self.sessions.lock().unwrap().get(name).cloned()
    }
    pub fn switch(&self, name: Option<String>, session: Arc<latent_core::AgentSession>) {
        if let Some(name) = &name {
            self.sessions
                .lock()
                .unwrap()
                .insert(name.clone(), session.clone());
        }
        *self.active_agent.lock().unwrap() = name;
        *self.current.write().unwrap() = session;
    }
}

pub struct InteractiveCtx<'a> {
    /// 会话路由:经 `current()` 访问当前激活会话(主会话或平行子 agent 会话)
    pub session: &'a SessionRouter,
    /// /subagent 平行会话工厂(与主会话同源依赖;None = 未装配)
    pub subagent_factory: Option<&'a Arc<latent_core::SubagentSessionFactory>>,
    /// 子会话落盘句柄注册表(tag → 私有 holder):/session list 与切换在
    /// 子会话谱系内操作用;None = 未装配(测试/纯内存)
    pub child_stores: Option<&'a crate::assembly::ChildStoreRegistry>,
    /// None = 内存会话(无 SessionManager);经 holder 读取当前值(/new 可切换)
    pub manager_holder: Option<&'a crate::assembly::SessionManagerHolder>,
    /// `/model` 的候选与解析(models.json + 内置 provider 默认表)。
    /// RwLock 支持配置入口热重载(编辑 models.json 后整体替换)。
    pub resolver: &'a std::sync::RwLock<ModelResolver>,
    /// 装配生效的压缩配置(/session 展示)
    pub compaction_config: &'a crate::assembly::CompactionConfig,
    /// 后台 subagent 运行注册表(footer 计数与 /new、退出清理;None = 未装配)
    pub subagent_registry: Option<&'a Arc<latent_core::SubagentRegistry>>,
    /// 打字门控共享标志(与 supervisor 的唤醒门控同源):编辑器非空 = 用户
    /// 正在输入,帧循环每轮回写。None = 未装配(测试)。
    pub user_composing: Option<&'a Arc<std::sync::atomic::AtomicBool>>,
    /// prompt 错误兜底回 UI 通道(事件流之外的装配/并发错误)
    pub ui_tx: mpsc::UnboundedSender<UiEvent>,
}

impl InteractiveCtx<'_> {
    /// 当前会话管理器(/new 切换后返回新会话)。
    pub fn current_manager(
        &self,
    ) -> Option<Arc<latent_session::SessionManager>> {
        self.manager_holder.and_then(|holder| holder.get())
    }
}

use std::sync::atomic::Ordering;

/// 帧循环每轮调用:异步 subagent 卡片(runId 绑定)对应的后台运行结算后,
/// 把卡片标题从 pending 翻为终态(成功绿/失败红)并触发全文重绘 ——
/// 已定稿转录行帧间不可变,状态翻转只能走整屏重绘(每次结算至多一次)。
pub(crate) fn flush_settled_subagent_cards(
    registry: Option<&Arc<latent_core::SubagentRegistry>>,
    state: &mut InteractiveState,
) {
    let Some(registry) = registry else {
        return;
    };
    if state.subagent_run_cards.is_empty() {
        return;
    }
    let mut flipped = false;
    state.subagent_run_cards.retain(|(run_id, index)| {
        match registry.run_state(run_id) {
            latent_core::RunState::Active => true,
            latent_core::RunState::Settled { success } => {
                if let Some(TranscriptItem::ToolCall { status, .. }) =
                    state.transcript.get_mut(*index)
                {
                    *status = if success {
                        ToolStatus::Success
                    } else {
                        ToolStatus::Error
                    };
                }
                flipped = true;
                false
            }
            // 记录被淘汰(单会话 50+ 运行)等异常:按完成兜底翻绿,不留
            // 永久 pending 的死卡片
            latent_core::RunState::Unknown => {
                if let Some(TranscriptItem::ToolCall { status, .. }) =
                    state.transcript.get_mut(*index)
                {
                    *status = ToolStatus::Success;
                }
                flipped = true;
                false
            }
        }
    });
    if flipped {
        state.needs_full_redraw = true;
    }
}

/// 帧循环每轮回写打字门控标志:编辑器非空 = 用户正在输入,supervisor 的
/// 结算唤醒延迟(有上限),避免抢在用户提交前拉起新 turn。
pub(crate) fn sync_wake_gate(
    flag: Option<&Arc<std::sync::atomic::AtomicBool>>,
    state: &InteractiveState,
) {
    if let Some(flag) = flag {
        flag.store(!state.editor.is_empty(), Ordering::Relaxed);
    }
}

use std::sync::Arc;

/// pi 退出语义:500ms 内双击 Ctrl+C 退出(interactive-mode.ts:4121)。
const DOUBLE_CTRL_C_WINDOW: Duration = Duration::from_millis(500);

/// 键盘处理:返回 true 表示退出。
pub async fn handle_key(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    key: latent_tui::Key,
) -> bool {
    // 选择列表激活时,键盘由列表接管
    if state.select.is_some() {
        handle_select_key(ctx, state, key).await;
        return false;
    }

    // 「添加模型」表单激活:Enter 提交当前步骤,Esc 取消,其余键进编辑器
    if state.model_form.is_some() {
        return handle_model_form_key(ctx, state, key).await;
    }

    // 斜杠补全弹窗跟随编辑器内容(直接 set_text 的路径也同步)
    state.sync_slash_popup();
    // `@` 文件弹窗同理(两者互斥:斜杠要求缓冲以 / 开头,提及要求
    // 光标 token 以 @ 开头;state 层还做了让位规则)
    state.sync_mention_popup();

    // 弹窗可见时的 Codex 交互:↑/↓ 选择、Tab 补全、Enter 补全并直接执行、
    // Esc 关闭;查询已与命令名完全一致时 Enter 不拦截,落入提交分支直接执行
    if state.slash_popup.visible() {
        match key {
            latent_tui::Key::Up => {
                state.slash_popup.move_up();
                return false;
            }
            latent_tui::Key::Down => {
                state.slash_popup.move_down();
                return false;
            }
            latent_tui::Key::Tab if !state.slash_popup.is_exact_match() => {
                if let Some(text) = state.slash_popup.complete_text() {
                    state.editor.set_text(&text);
                }
                state.sync_slash_popup();
                return false;
            }
            // Enter 分派(非精确匹配):
            // - 选中行是变体(`命令 参数`):补全并直接执行(一次回车);
            // - 选中行是带变体的裸命令(含部分输入如 /sess):只补全命令名并
            //   展开变体选择页、不执行 —— 方向键选定后回车执行;
            // - 普通命令:补全后落到底部提交分支执行(一次回车直达)。
            latent_tui::Key::Enter if !state.slash_popup.is_exact_match() => {
                let Some(text) = state.slash_popup.complete_text() else {
                    return false;
                };
                let completed = text.trim_end().to_string();
                let is_variant = completed.contains(char::is_whitespace);
                let has_variants = state
                    .slash_popup
                    .selected_entry()
                    .is_some_and(|entry| !entry.variants.is_empty());
                state.editor.set_text(&completed);
                if !is_variant && has_variants {
                    // query 变为命令名 → 弹窗展开该命令的变体子项(选择页)
                    state.sync_slash_popup();
                    return false;
                }
            }
            latent_tui::Key::Esc => {
                state.slash_popup.dismiss();
                return false;
            }
            _ => {}
        }
    }

    // `@` 文件弹窗(非模态):↑/↓ 选择、Tab/Enter 补全选中项、Esc 关闭;
    // 其余键进编辑器(继续打字即继续过滤)。补全经 token 级替换落进编辑器:
    // 文件带尾随空格 → token 终结、弹窗退场(再按一次 Enter 提交);
    // 目录带 `/` 尾缀 → 弹窗保持,继续下钻。
    if state.mention_popup.visible() {
        match key {
            latent_tui::Key::Up => {
                state.mention_popup.move_up();
                return false;
            }
            latent_tui::Key::Down => {
                state.mention_popup.move_down();
                return false;
            }
            latent_tui::Key::Tab | latent_tui::Key::Enter => {
                if let Some((text, _)) = state.mention_popup.complete_text() {
                    state.editor.replace_token_before_cursor(&text);
                    state.sync_mention_popup();
                }
                return false;
            }
            latent_tui::Key::Esc => {
                state.mention_popup.dismiss();
                return false;
            }
            _ => {}
        }
    }

    // Shift+Tab 循环 Plan → Confirm → FullAccess → Plan(13 文档 §10.2)
    if key == latent_tui::Key::BackTab {
        state.last_ctrl_c = None;
        let next = ctx.session.current().mode().next();
        return cycle_mode(ctx, state, next).await;
    }

    match key {
        latent_tui::Key::Enter => {
            // Enter 与 Ctrl+O 同普通键:重置双击 Ctrl+C 窗口
            state.last_ctrl_c = None;
            let Some(text) = state.take_input() else {
                return false;
            };
            state.sync_slash_popup();
            state.sync_mention_popup();
            // 提交可能请求退出(/quit):必须向上传递,否则 /quit 静默失效
            return submit_input(ctx, state, text).await;
        }
        latent_tui::Key::Ctrl('o') => {
            state.last_ctrl_c = None;
            state.expanded = !state.expanded;
            state.needs_full_redraw = true;
            false
        }
        latent_tui::Key::Ctrl('c') => {
            if ctx.session.current().agent().is_streaming() {
                ctx.session.current().abort();
                state.status = Status::Aborted;
                state.last_ctrl_c = None;
                false
            } else {
                // pi 退出语义:双击 Ctrl+C(500ms 内)退出,首按提示
                let now = Instant::now();
                match state.last_ctrl_c {
                    Some(at) if now.duration_since(at) < DOUBLE_CTRL_C_WINDOW => true,
                    _ => {
                        state.last_ctrl_c = Some(now);
                        state.status = Status::Idle;
                        state.commit_ephemeral(warning_line_theme(
                            "press Ctrl+C again to exit",
                            &state.theme,
                        ));
                        false
                    }
                }
            }
        }
        latent_tui::Key::Esc => {
            state.last_ctrl_c = None;
            if ctx.session.current().agent().is_streaming() {
                ctx.session.current().abort();
                state.status = Status::Aborted;
            }
            false
        }
        latent_tui::Key::Ctrl('d') if state.editor.is_empty() => true,
        // 全屏模式滚动按键(选择列表/弹窗接管时不达此处;regular 模式
        // 交给终端 scrollback,保持原生行为)
        latent_tui::Key::PageUp if state.fullscreen => {
            state.scroll_request = Some(ScrollRequest::PageUp);
            false
        }
        latent_tui::Key::PageDown if state.fullscreen => {
            state.scroll_request = Some(ScrollRequest::PageDown);
            false
        }
        latent_tui::Key::Home if state.fullscreen => {
            state.scroll_request = Some(ScrollRequest::Top);
            false
        }
        latent_tui::Key::End if state.fullscreen => {
            state.scroll_request = Some(ScrollRequest::Bottom);
            false
        }
        latent_tui::Key::ScrollUp if state.fullscreen => {
            state.scroll_request = Some(ScrollRequest::Lines(-3));
            false
        }
        latent_tui::Key::ScrollDown if state.fullscreen => {
            state.scroll_request = Some(ScrollRequest::Lines(3));
            false
        }
        key => {
            state.last_ctrl_c = None;
            state.editor_key(&key);
            state.sync_slash_popup();
            state.sync_mention_popup();
            false
        }
    }
}

/// 选择列表激活时的按键分派。
async fn handle_select_key(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    key: latent_tui::Key,
) {
    // 审批 overlay 的数字快捷键:1/2/3/4 直达决策(13 文档 §10.3)
    if let Some(select) = state.select.as_ref() {
        if matches!(select.kind, SelectKind::Approval { .. }) {
            let index = match key {
                latent_tui::Key::Char('1') => Some(0usize),
                latent_tui::Key::Char('2') => Some(1),
                latent_tui::Key::Char('3') => Some(2),
                latent_tui::Key::Char('4') => Some(3),
                _ => None,
            };
            if let Some(index) = index {
                if let Some(request) = state.select.take() {
                    if let SelectKind::Approval { responder } = request.kind {
                        let decision = match index {
                            0 => latent_core::ApprovalDecision::Approve,
                            1 => latent_core::ApprovalDecision::ApproveForSession,
                            2 => latent_core::ApprovalDecision::Deny,
                            _ => latent_core::ApprovalDecision::Abort,
                        };
                        let _ = responder.send(decision);
                        state.status = Status::Idle;
                    }
                }
                state.promote_next_select();
            }
            if matches!(key, latent_tui::Key::Char('1'..='4')) {
                return;
            }
        }
    }
    match key {
        latent_tui::Key::Up => {
            if let Some(select) = state.select.as_mut() {
                select.list.move_up();
            }
        }
        latent_tui::Key::Down => {
            if let Some(select) = state.select.as_mut() {
                select.list.move_down();
            }
        }
        latent_tui::Key::Enter => {
            if let Some(request) = state.select.take() {
                let index = request.list.selected;
                match request.kind {
                    SelectKind::Confirm(responder) => {
                        let _ = responder.send(true);
                    }
                    SelectKind::Select(responder) => {
                        let _ = responder.send(Some(index));
                    }
                    SelectKind::SubagentAgent { defs } => {
                        if index < defs.len() {
                            if let Some(def) = defs.get(index) {
                                switch_to_subagent(ctx, state, def.clone()).await;
                            }
                        } else {
                            // 末位的 off 选项:退出子 agent,回到主会话
                            exit_subagent(ctx, state);
                        }
                    }
                    SelectKind::Session { files } => {
                        if let Some(summary) = files.get(index) {
                            switch_to_session(ctx, state, &summary.path).await;
                        }
                    }
                    SelectKind::Model { models } => {
                        if index < models.len() {
                            if let Some(model) = models.get(index) {
                                ctx.session.current().set_model(model.clone()).await;
                                refresh_footer(ctx, state);
                                state.status = Status::Idle;
                                state.commit_ephemeral(warning_line_theme(
                                    &format!("model → {}", state.model_label),
                                    &state.theme,
                                ));
                            }
                        } else if index == models.len() {
                            // 末尾配置条目 1:交互式添加模型(第一步选写入位置)
                            state.model_form = Some(super::state::ModelForm::new());
                            let home_str = state.home.as_deref().and_then(|p| p.to_str());
                            let project = state.cwd.join(".latent/models.json");
                            let options = vec![
                                format!(
                                    "当前目录  {}(仅本项目)",
                                    latent_tui::footer::abbreviate_home(
                                        &project.display().to_string(),
                                        home_str
                                    )
                                ),
                                match latent_core::latent_dir(state.home.as_deref()) {
                                    Some(dir) => format!(
                                        "全局  {}(所有项目)",
                                        latent_tui::footer::abbreviate_home(
                                            &dir.join("models.json").display().to_string(),
                                            home_str
                                        )
                                    ),
                                    None => "全局(未检测到 HOME / LATENT_HOME)".to_string(),
                                },
                            ];
                            state.select = Some(SelectRequest {
                                prompt: "添加模型 · 写入哪个 models.json?".into(),
                                list: SelectList::new(options),
                                kind: SelectKind::ModelTargetChoice,
                            });
                            state.status = Status::Idle;
                        } else {
                            // 末尾配置条目 2:$EDITOR 打开 models.json(事件循环代办)
                            state.suspend_action = Some(super::state::SuspendAction::EditModelsJson);
                            state.status = Status::Idle;
                        }
                    }
                    SelectKind::ModelApiChoice => {
                        // api 协议选择列表(0/1);Esc 分支已把表单整体取消
                        let apis = ["openai-completions", "anthropic-messages"];
                        if let Some(form) = state.model_form.as_mut() {
                            form.api = apis.get(index).map(|s| (*s).to_string());
                            form.step = super::state::ModelFormStep::BaseUrl;
                        }
                        state.status = Status::Idle;
                    }
                    SelectKind::ModelTargetChoice => {
                        // 写入位置选择列表(0=项目 1=全局);Esc 分支取消表单
                        if let Some(form) = state.model_form.as_mut() {
                            form.target = Some(if index == 0 {
                                super::state::ModelFormTarget::Project
                            } else {
                                super::state::ModelFormTarget::Global
                            });
                            form.step = super::state::ModelFormStep::Provider;
                        }
                        state.status = Status::Idle;
                    }
                    SelectKind::Thinking => {
                        if let Some(name) = thinking_level_options().get(index) {
                            let level = crate::assembly::parse_thinking_level(name);
                            ctx.session.current().set_thinking_level(level).await;
                            refresh_footer(ctx, state);
                            state.status = Status::Idle;
                        }
                    }
                    SelectKind::Theme { names } => {
                        if let Some(name) = names.get(index) {
                            apply_theme(state, *name);
                        }
                    }
                    SelectKind::Setting => {
                        apply_setting_selection(state, index);
                    }
                    SelectKind::Approval { responder } => {
                        // 13 文档 §10.3:1=批准一次 2=本会话批准 3=拒绝 4=中止
                        let decision = match index {
                            0 => latent_core::ApprovalDecision::Approve,
                            1 => latent_core::ApprovalDecision::ApproveForSession,
                            2 => latent_core::ApprovalDecision::Deny,
                            _ => latent_core::ApprovalDecision::Abort,
                        };
                        let _ = responder.send(decision);
                        state.status = Status::Idle;
                    }
                }
            }
            state.promote_next_select();
        }
        latent_tui::Key::Esc | latent_tui::Key::Ctrl('c') => {
            // 模态期间 Esc/Ctrl+C 取消当前请求(不退出;再按 Ctrl+C 才退出)
            if let Some(request) = state.select.take() {
                match request.kind {
                    SelectKind::Confirm(responder) => {
                        let _ = responder.send(false);
                    }
                    SelectKind::Select(responder) => {
                        let _ = responder.send(None);
                    }
                    SelectKind::SubagentAgent { .. } | SelectKind::Session { .. } => {}
                    SelectKind::Model { .. } | SelectKind::Thinking => {}
                    // Esc 在表单中途的选择列表上 = 取消整张表单
                    SelectKind::ModelApiChoice | SelectKind::ModelTargetChoice => {
                        state.model_form = None;
                    }
                    SelectKind::Theme { .. } | SelectKind::Setting => {}
                    SelectKind::Approval { responder } => {
                        // Esc = 拒绝;Ctrl+C = 中止本次任务(13 文档 §10.3)
                        let decision = if matches!(key, latent_tui::Key::Ctrl('c')) {
                            latent_core::ApprovalDecision::Abort
                        } else {
                            latent_core::ApprovalDecision::Deny
                        };
                        let _ = responder.send(decision);
                    }
                }
            }
            state.promote_next_select();
        }
        _ => {}
    }
}

/// 提交输入:`!`/`!!` bash 透传、`/` 斜杠命令、普通 prompt。
async fn submit_input(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    text: String,
) -> bool {
    // `!` bash 透传(优先于斜杠解析)
    if let Some(rest) = text.strip_prefix('!') {
        let (bang_bang, command) = match rest.strip_prefix('!') {
            Some(command) => (true, command.trim()),
            None => (false, rest.trim()),
        };
        if command.is_empty() {
            state.commit_ephemeral(warning_line_theme("usage: !<command>", &state.theme));
            return false;
        }
        state.status = Status::Bash(command.to_string());
        let ui_tx = ctx.ui_tx.clone();
        let command = command.to_string();
        tokio::spawn(async move {
            let (output, is_error) = bash::run_command(&command).await;
            let _ = ui_tx.send(UiEvent::BashDone {
                command,
                output,
                exit_code: None,
                is_error,
                inject: !bang_bang,
            });
        });
        return false;
    }

    match slash::parse(&text) {
        slash::SlashInput::NotACommand(text) => {
            state.status = Status::Thinking;
            state.stream_text.clear();
            state.pending_thinking = None;
            let session = ctx.session.current();
            let ui_tx = ctx.ui_tx.clone();
            // prompt 任务在后台跑;事件经订阅者回流上屏,stdin 保持可响应
            // (run 期间的输入经 session.prompt 自动转 steer)
            tokio::spawn(async move {
                if let Err(error) = session.prompt(text).await {
                    let _ = ui_tx.send(UiEvent::Notify(format!("Error: {error}")));
                }
            });
            false
        }
        slash::SlashInput::Unknown(name) => {
            // latent 无动态命令源:未知 /xxx 本地警告,不发给模型
            state.commit_ephemeral(warning_line_theme(
                &format!("Unknown command: {name}(输入 /help 查看可用命令)"),
                &state.theme,
            ));
            false
        }
        slash::SlashInput::Command(action) => {
            // 退出类命令(/quit)的信号必须向上传递
            return execute_command(ctx, state, action).await;
        }
    }
}

/// 切换会话模式:set_mode + footer 刷新 + 系统提示(13 文档 §10.2)。
/// 流式期间允许(下一工具调用生效;正在流式的 turn 不打断)。
async fn cycle_mode(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    mode: latent_core::SessionMode,
) -> bool {
    match ctx.session.current().set_mode(mode).await {
        Ok(()) => {
            // 不打转录提示:footer 状态栏已显示当前模式标记
            refresh_footer(ctx, state);
            state.status = Status::Idle;
        }
        Err(error) => {
            state.commit_ephemeral(view::error_line(
                &format!("模式切换失败: {error}"),
                &state.theme,
            ));
        }
    }
    false
}

/// 切换到平行子 agent 会话:已存在(按名缓存)直接切回,否则现场创建并挂
/// TUI 事件订阅(转录/审批照常渲染)。
/// /subagent off(或选择器末位 off 项):切回主会话(子 agent 上下文按名
/// 缓存保留,可再次 /subagent 切回)。
fn exit_subagent(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    if ctx.session.is_main() {
        state.commit_ephemeral(warning_line_theme("当前已是主会话", &state.theme));
    } else {
        let name = ctx.session.active_agent();
        ctx.session.switch(None, ctx.session.main());
        state.active_agent = None;
        refresh_footer(ctx, state);
        state.commit_line(plain_dim(
            &format!(
                "已切回主会话({} 的上下文保留,可再次 /subagent 切回)",
                name.as_deref().unwrap_or("?")
            ),
            &state.theme,
        ));
    }
}

async fn switch_to_subagent(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    def: latent_core::AgentDef,
) {
    let Some(factory) = ctx.subagent_factory else {
        return;
    };
    if let Some(existing) = ctx.session.cached(&def.name) {
        ctx.session.switch(Some(def.name.clone()), existing);
        state.active_agent = Some(def.name.clone());
        refresh_footer(ctx, state);
        state.commit_line(plain_dim(
            &format!("切回 subagent: {}(上下文保留)", def.name),
            &state.theme,
        ));
        return;
    }
    let Some(fallback_model) = ctx.session.current().agent().state_snapshot().model else {
        state.commit_ephemeral(view::error_line("主会话无模型,无法创建子 agent 会话", &state.theme));
        return;
    };
    let subscribers = Arc::new(std::sync::Mutex::new(Vec::new()));
    match factory.create(&def, fallback_model, subscribers).await {
        Ok(session) => {
            session.subscribe(Arc::new(crate::modes::interactive::events::SessionToUiSubscriber {
                tx: ctx.ui_tx.clone(),
            }));
            ctx.session.switch(Some(def.name.clone()), session);
            state.active_agent = Some(def.name.clone());
            refresh_footer(ctx, state);
            state.commit_line(plain_dim(
                &format!(
                    "已切换到 subagent: {}(上下文与主会话隔离;/subagent off 返回)",
                    def.name
                ),
                &state.theme,
            ));
        }
        Err(error) => {
            state.commit_ephemeral(view::error_line(
                &format!("subagent 会话创建失败: {error}"),
                &state.theme,
            ));
        }
    }
}

/// 命令信息输出统一管线:markdown 源文本 → 与 assistant 正文同一渲染
/// (标题/列表/行内代码着色)→ 逐行入转录。
fn commit_markdown(state: &mut InteractiveState, markdown: &str) {
    let rendered = view::assistant_markdown(markdown, &state.theme, state.width.max(1));
    for line in rendered {
        state.commit_line(line);
    }
}

/// 斜杠命令执行(解析在 slash.rs)。
pub async fn execute_command(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    action: slash::SlashAction,
) -> bool {
    match action {
        slash::SlashAction::Help => {
            // 与上文留一行间隔(commit_blank 去重,不会双写)。命令输出是
            // latent 自己"创作"的内容,与 assistant 正文同走 markdown 渲染管线
            state.commit_blank();
            commit_markdown(state, &slash::help_markdown());
        }
        slash::SlashAction::Quit => return true,
        slash::SlashAction::Setting => open_setting_selector(state),
        slash::SlashAction::Session { arg } => match arg.as_deref() {
            Some("list") => open_session_selector(ctx, state).await,
            Some("info") => {
                // 与上文留一行间隔(commit_blank 去重,不会双写)
                state.commit_blank();
                commit_markdown(state, &session_info_markdown(ctx, state));
            }
            Some(other) => {
                state.commit_ephemeral(view::error_line(
                    &format!("未知参数 /session {other}(list = 切换历史会话;info = 会话信息)"),
                    &state.theme,
                ));
            }
            None => {
                state.commit_ephemeral(warning_line_theme(
                    "用法: /session list(切换历史会话)| info(会话信息)",
                    &state.theme,
                ));
            }
        },
        slash::SlashAction::Compact { arg } => {
            // 流式期间压缩会让 SessionCompactor 先落盘 Compaction entry、
            // 随后 set_messages 失败;摘要 LLM 调用内联 await 会冻结事件循环
            if ctx.session.current().agent().is_streaming() {
                state.commit_ephemeral(view::error_line(
                    "run 进行中不能压缩;等待 run 结束或 Esc 中止后再试",
                    &state.theme,
                ));
                return false;
            }
            if arg.is_some() {
                // 阻断式警告:让用户明确知道参数不会生效,不会误以为自定义指令参与压缩
                state.commit_ephemeral(warning_line_theme(
                    "自定义压缩指令暂不支持,本次压缩将使用默认模板",
                    &state.theme,
                ));
            }
            state.status = Status::Compacting;
            let session = ctx.session.current();
            let ui_tx = ctx.ui_tx.clone();
            tokio::spawn(async move {
                let _ = ui_tx.send(UiEvent::CompactDone(session.compact().await));
            });
        }
        slash::SlashAction::Mode { arg } => match arg {
            Some(name) => match latent_core::SessionMode::parse(&name) {
                Some(mode) => {
                    cycle_mode(ctx, state, mode).await;
                }
                None => {
                    state.commit_ephemeral(view::error_line(
                        &format!("未知模式: {name}(plan|confirm|full-access)"),
                        &state.theme,
                    ));
                }
            },
            None => {
                // 带变体命令的裸调用不做任何事,只提示用法(统一规范)
                state.commit_ephemeral(warning_line_theme(
                    "用法: /mode <plan|confirm|full-access>(Shift+Tab 循环切换)",
                    &state.theme,
                ));
            }
        },
        slash::SlashAction::Subagent { arg } => match arg.as_deref() {
            Some("off") => exit_subagent(ctx, state),
            // 无参数与 select 等价:打开选择器(候选 = 可用 agent + off 回主会话)
            Some("select") | None => {
                if ctx.session.current().agent().is_streaming() {
                    state.commit_ephemeral(view::error_line(
                        "run 进行中不能切换;等待 run 结束或 Esc 中止后再试",
                        &state.theme,
                    ));
                    return false;
                }
                let Some(factory) = ctx.subagent_factory else {
                    state.commit_ephemeral(view::error_line("subagent 会话工厂未装配", &state.theme));
                    return false;
                };
                let defs = factory.discover();
                // 主会话下无任何 agent 定义才无意义;子 agent 会话至少还能选 off 回主
                if defs.is_empty() && ctx.session.is_main() {
                    state.commit_ephemeral(view::error_line(
                        "没有可用的 subagent 定义(.latent/agents/*.md)",
                        &state.theme,
                    ));
                    return false;
                }
                let mut options: Vec<String> = defs
                    .iter()
                    .map(|d| {
                        if d.description.is_empty() {
                            d.name.clone()
                        } else {
                            format!("{} — {}", d.name, d.description)
                        }
                    })
                    .collect();
                options.push("off — 退出子 agent,回到主会话".into());
                let mut list = SelectList::new(options);
                if let Some(current) = ctx.session.active_agent() {
                    if let Some(index) = defs.iter().position(|d| d.name == current) {
                        list.selected = index;
                    }
                }
                state.select = Some(SelectRequest {
                    prompt: "选择 subagent(平行会话,上下文与主会话完全隔离)".into(),
                    list,
                    kind: SelectKind::SubagentAgent { defs },
                });
            }
            Some(other) => {
                state.commit_ephemeral(view::error_line(
                    &format!("未知参数 /subagent {other}(off = 回主会话;无参数 = 选择 subagent)"),
                    &state.theme,
                ));
            }
        },
        slash::SlashAction::New => {
            // 流式期间切换会让进行中的 run 写入错误的 session 文件
            if ctx.session.current().agent().is_streaming() {
                state.commit_ephemeral(view::error_line(
                    "run 进行中不能新建会话;等待 run 结束或 Esc 中止后再试",
                    &state.theme,
                ));
                return false;
            }
            // 平行子 agent 会话:有落盘句柄时在**本会话谱系**内开新文件(tag =
            // agent 名,旧文件原样保留可 /session 切回)——原地 reset 会让同一
            // 文件里混进两段会话;纯内存会话才直接清空转录
            if !ctx.session.is_main() {
                let name = ctx.session.active_agent().unwrap_or_default();
                let holder = ctx
                    .child_stores
                    .and_then(|registry| registry.lock().unwrap().get(&name).cloned());
                match holder {
                    Some(holder) => {
                        match crate::assembly::switch_new_session(
                            &ctx.session.current(),
                            &holder,
                            Some(&name),
                        )
                        .await
                        {
                            Ok(path) => {
                                state.reset_for_new_session();
                                refresh_footer(ctx, state);
                                let message = match &path {
                                    Some(path) => format!(
                                        "new session started: {}(subagent {name})",
                                        path.display()
                                    ),
                                    None => "new session started".to_string(),
                                };
                                state.commit_ephemeral(plain_dim(&message, &state.theme));
                            }
                            Err(error) => {
                                state.commit_ephemeral(view::error_line(
                                    &format!("新建会话失败: {error}"),
                                    &state.theme,
                                ));
                            }
                        }
                    }
                    None => {
                        let _ = ctx.session.current().agent().reset();
                        state.reset_for_new_session();
                        refresh_footer(ctx, state);
                        state.commit_ephemeral(plain_dim(
                            &format!("new session (subagent {name} 内存会话,无文件)"),
                            &state.theme,
                        ));
                    }
                }
                return false;
            }
            // 旧会话的后台 subagent 一并终止(抑制完成通知,不唤醒新会话)
            if let Some(registry) = ctx.subagent_registry {
                registry.abort_all();
            }
            match ctx.manager_holder {
                Some(holder) => {
                    match crate::assembly::switch_new_session(&ctx.session.current(), holder, None)
                        .await
                    {
                        Ok(path) => {
                            // 清空转录区/用量,底部提示新会话(旧会话原样保留在原文件)
                            state.reset_for_new_session();
                            refresh_footer(ctx, state);
                            let message = match &path {
                                Some(path) => {
                                    format!("new session started: {}", path.display())
                                }
                                None => "new session started".to_string(),
                            };
                            // ephemeral:reset 已触发全文重绘,转录重渲染 + 待提交
                            // 合并会把 commit_line 的条目画两遍
                            state.commit_ephemeral(plain_dim(&message, &state.theme));
                        }
                        Err(error) => {
                            state.commit_ephemeral(view::error_line(
                                &format!("新建会话失败: {error}"),
                                &state.theme,
                            ));
                        }
                    }
                }
                None => {
                    state.commit_ephemeral(warning_line_theme(
                        "当前无会话存储,无法新建会话",
                        &state.theme,
                    ));
                }
            }
        }
        slash::SlashAction::Model { arg } => match arg {
            Some(spec) => {
                let resolved = ctx.resolver.read().unwrap().resolve(&spec);
                match resolved {
                    Ok(model) => {
                        ctx.session.current().set_model(model).await;
                        refresh_footer(ctx, state);
                        state.status = Status::Idle;
                    }
                    Err(error) => {
                        state.commit_ephemeral(view::error_line(
                            &format!("model 切换失败: {error}"),
                            &state.theme,
                        ));
                    }
                }
            }
            None => open_model_selector(ctx, state),
        },
        slash::SlashAction::Theme { arg } => match arg {
            Some(name) => match name.parse::<latent_tui::ThemeName>() {
                Ok(theme_name) => apply_theme(state, theme_name),
                Err(_) => {
                    state.commit_ephemeral(view::error_line(
                        &format!("未知主题: {name}(输入 /theme 查看主题列表)"),
                        &state.theme,
                    ));
                }
            },
            None => open_theme_selector(state),
        },
        slash::SlashAction::Thinking { arg } => match arg {
            Some(name) => match parse_thinking_input(&name) {
                Some(level) => {
                    ctx.session.current().set_thinking_level(level).await;
                    refresh_footer(ctx, state);
                    state.status = Status::Idle;
                }
                None => {
                    state.commit_ephemeral(view::error_line(
                        &format!(
                            "未知 thinking 级别: {name}(off|minimal|low|medium|high|xhigh|max)"
                        ),
                        &state.theme,
                    ));
                }
            },
            None => open_thinking_selector(state),
        },
    }
    false
}

/// /model 选择器末尾的两个配置入口(index 越界 = 特殊条目,同 /subagent
/// 的 off 项模式)。
pub const MODEL_FORM_ENTRY: &str = "＋ 添加模型…";
pub const MODEL_EDITOR_ENTRY: &str = "⚙ 编辑 models.json…";

pub(crate) fn open_model_selector(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    let models = ctx.resolver.read().unwrap().available_models();
    let mut options: Vec<String> = models
        .iter()
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect();
    options.push(MODEL_FORM_ENTRY.into());
    options.push(MODEL_EDITOR_ENTRY.into());
    let mut list = SelectList::new(options);
    if let Some(index) = list
        .options
        .iter()
        .position(|spec| *spec == state.model_label)
    {
        list.selected = index;
    }
    state.select = Some(SelectRequest {
        prompt: "选择模型(↓ 到末尾可配置)".into(),
        list,
        kind: SelectKind::Model { models },
    });
}

/// 「添加模型」表单激活时的按键分派:Enter 提交当前步骤,Esc 取消,
/// 其余键交给编辑器(正常输入体验)。
async fn handle_model_form_key(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    key: latent_tui::Key,
) -> bool {
    match key {
        latent_tui::Key::Esc => {
            state.model_form = None;
            state.commit_ephemeral(warning_line_theme("已取消添加模型", &state.theme));
        }
        latent_tui::Key::Enter => {
            // 不走 take_input(它拦截空输入):可选字段允许直接回车跳过,
            // 必填字段(provider/model)的空提交由 advance_model_form 忽略
            let text = state.editor.expanded_text().trim().to_string();
            state.editor.commit_history();
            state.editor.clear();
            advance_model_form(ctx, state, &text).await;
        }
        key => state.editor_key(&key),
    }
    false
}

/// 提交表单当前步骤:逐步收集 provider/api/baseUrl/apiKey/model,收齐后
/// upsert 进 models.json 并热重载、切换到新模型。
async fn advance_model_form(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState, text: &str) {
    use super::state::ModelFormStep;
    let Some(form) = state.model_form.as_mut() else {
        return;
    };
    match form.step {
        ModelFormStep::Target => unreachable!("写入位置经选择列表写入,不经文本提交"),
        ModelFormStep::Provider => {
            if text.is_empty() {
                return; // 空输入不推进,等有效 provider id
            }
            // 已知 provider(builtin 表或 models.json 声明)→ 跳过 provider 级步骤
            let probe = ctx.resolver.read().unwrap().resolve(&format!("{text}/_probe"));
            form.provider_id = Some(text.to_string());
            form.known_provider = probe.is_ok();
            if form.known_provider {
                form.step = ModelFormStep::ModelId;
            } else {
                form.step = ModelFormStep::Api;
                let apis: Vec<String> = ["openai-completions", "anthropic-messages"]
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect();
                state.select = Some(SelectRequest {
                    prompt: "添加模型 · 选择 api 协议".into(),
                    list: SelectList::new(apis),
                    kind: SelectKind::ModelApiChoice,
                });
            }
        }
        ModelFormStep::Api => unreachable!("api 协议经选择列表写入,不经文本提交"),
        ModelFormStep::BaseUrl => {
            form.base_url = if text.is_empty() { None } else { Some(text.to_string()) };
            form.step = ModelFormStep::ApiKeyEnv;
        }
        ModelFormStep::ApiKeyEnv => {
            form.api_key_env = if text.is_empty() { None } else { Some(text.to_string()) };
            form.step = ModelFormStep::ModelId;
        }
        ModelFormStep::ModelId => {
            if text.is_empty() {
                return;
            }
            // 先收走表单(upsert/reload 期间不再渲染问题行)
            let mut form = state.model_form.take().unwrap();
            form.model_id = Some(text.to_string());
            finish_model_form(ctx, state, form, text).await;
        }
    }
}

/// 表单收尾:写 models.json → 热重载 resolver → 切到新模型。失败时提示
/// 并放弃(表单已关闭,可从 /model 重新进入)。
async fn finish_model_form(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    form: super::state::ModelForm,
    model_id: &str,
) {
    use latent_core::NewModelEntry;
    // 写回位置按表单第一步的选择;缺省(异常路径)回退项目优先
    let path = match form.target {
        Some(super::state::ModelFormTarget::Project) => state.cwd.join(".latent/models.json"),
        Some(super::state::ModelFormTarget::Global) => {
            latent_core::latent_dir(state.home.as_deref())
                .map(|dir| dir.join("models.json"))
                .unwrap_or_else(|| state.cwd.join(".latent/models.json"))
        }
        None => {
            let dir = latent_core::latent_dir(state.home.as_deref());
            latent_core::preferred_models_path(Some(&state.cwd), dir.as_deref())
        }
    };
    let entry = NewModelEntry {
        provider_id: form.provider_id.clone().unwrap_or_default(),
        api: form.api.clone(),
        base_url: form.base_url.clone(),
        api_key_env: form.api_key_env.clone(),
        model_id: model_id.to_string(),
    };
    if let Err(error) = latent_core::upsert_models_json_entry(&path, &entry) {
        state.commit_ephemeral(view::error_line(
            &format!("写入 {} 失败: {error}", path.display()),
            &state.theme,
        ));
        return;
    }
    reload_model_resolver(ctx, state);
    // 新模型已注册,直接切换(热重载只刷新本侧 resolver,见 reload 注释)
    let spec = format!("{}/{}", entry.provider_id, entry.model_id);
    let resolved = ctx.resolver.read().unwrap().resolve(&spec);
    match resolved {
        Ok(model) => {
            ctx.session.current().set_model(model).await;
            refresh_footer(ctx, state);
            state.commit_ephemeral(warning_line_theme(
                &format!("已添加 {spec} 并切换(models.json:{})", path.display()),
                &state.theme,
            ));
        }
        Err(error) => {
            state.commit_ephemeral(view::error_line(
                &format!("{spec} 写入成功但解析失败: {error}"),
                &state.theme,
            ));
        }
    }
}

/// 热重载 models.json → 整体替换 interactive 侧 resolver。已知限制:
/// build_session 装配期为 web 工具/subagent 建的 resolver 快照不跟随,
/// 重启后生效。
pub(crate) fn reload_model_resolver(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    let dir = latent_core::latent_dir(state.home.as_deref());
    let reloaded =
        latent_core::create_model_resolver_from_config(Some(&state.cwd), dir.as_deref());
    *ctx.resolver.write().unwrap() = reloaded;
}

fn open_thinking_selector(state: &mut InteractiveState) {
    let names: Vec<String> = thinking_level_options()
        .into_iter()
        .map(String::from)
        .collect();
    let mut list = SelectList::new(names.clone());
    if let Some(index) = names.iter().position(|name| *name == state.thinking_label) {
        list.selected = index;
    }
    state.select = Some(SelectRequest {
        prompt: "选择 thinking 级别".into(),
        list,
        kind: SelectKind::Thinking,
    });
}

/// 应用主题切换:更新状态并请求全文重绘(转录按新主题重新着色)。
/// 只在会话内生效;持久化请写 settings.json 的 `theme` 字段。
fn apply_theme(state: &mut InteractiveState, name: latent_tui::ThemeName) {
    state.theme = latent_tui::Theme::from_theme_name(name);
    state.theme_name = Some(name.slug().to_string());
    state.needs_full_redraw = true;
    state.commit_ephemeral(warning_line_theme(
        &format!("theme → {}", name.display_name()),
        &state.theme,
    ));
}

/// `/setting`:打开设置选择器(当前值内联在条目里,Enter 切换/循环)。
fn open_setting_selector(state: &mut InteractiveState) {
    let options = vec![
        format!(
            "全屏模式: {}(输入区钉底,屏幕内滚动)",
            if state.fullscreen { "开" } else { "关" }
        ),
        format!(
            "Ctrl+X 复制: {}(有选区时 Ctrl+X 复制到剪贴板)",
            if state.ctrl_x_copy { "开" } else { "关" }
        ),
        format!(
            "选中后自动复制: {}(松开拖选入剪贴板;选择/快捷键复制不受影响)",
            if state.copy_on_select { "开" } else { "关" }
        ),
    ];
    // 写回目标动态展示(数据目录可经 LATENT_HOME 自定义)
    let home_str = state.home.as_deref().and_then(|p| p.to_str());
    let target = match latent_core::latent_dir(state.home.as_deref()) {
        Some(dir) => latent_tui::footer::abbreviate_home(
            &dir.join("settings.json").display().to_string(),
            home_str,
        ),
        None => "全局 settings.json".to_string(),
    };
    state.select = Some(SelectRequest {
        prompt: format!("设置(Enter 切换 · Esc 关闭 · 写入 {target})"),
        list: SelectList::new(options),
        kind: SelectKind::Setting,
    });
}

/// 应用 /setting 选择:切换后立即写回项目 settings.json 并重开选择器
/// (条目标签反映新值)。
fn apply_setting_selection(state: &mut InteractiveState, index: usize) {
    match index {
        0 => {
            let target = !state.fullscreen;
            state.fullscreen = target;
            state.tui_mode_switch = Some(target);
            persist_setting(
                state,
                "tuiMode",
                serde_json::json!(if target { "fullscreen" } else { "regular" }),
            );
            state.commit_ephemeral(warning_line_theme(
                if target {
                    "TUI → fullscreen(输入区钉底,PageUp/PageDown/滚轮滚动)"
                } else {
                    "TUI → regular(内容滚入终端 scrollback)"
                },
                &state.theme,
            ));
        }
        1 => {
            state.ctrl_x_copy = !state.ctrl_x_copy;
            persist_setting(state, "ctrlXCopy", serde_json::json!(state.ctrl_x_copy));
        }
        2 => {
            state.copy_on_select = !state.copy_on_select;
            persist_setting(
                state,
                "copyOnSelect",
                serde_json::json!(state.copy_on_select),
            );
        }
        _ => {}
    }
    open_setting_selector(state);
}

/// 设置项写回全局数据目录的 settings.json(失败仅提示,不阻断交互)。
fn persist_setting(state: &mut InteractiveState, key: &str, value: serde_json::Value) {
    let dir = latent_core::latent_dir(state.home.as_deref());
    if let Err(e) = latent_core::write_setting_field(None, dir.as_deref(), key, value) {
        state.commit_ephemeral(view::error_line(
            &format!("写入 settings.json 失败: {e}"),
            &state.theme,
        ));
    }
}

fn open_theme_selector(state: &mut InteractiveState) {
    let names: Vec<latent_tui::ThemeName> = latent_tui::ThemeName::all().to_vec();
    let mut list = SelectList::new(
        names
            .iter()
            .map(|name| name.display_name().to_string())
            .collect(),
    );
    if let Some(current) = &state.theme_name {
        if let Some(index) = names.iter().position(|name| name.slug() == current) {
            list.selected = index;
        }
    }
    state.select = Some(SelectRequest {
        prompt: "选择主题".into(),
        list,
        kind: SelectKind::Theme { names },
    });
}

/// /session list:弹出本谱系的历史会话列表(mtime 倒序)供切换。主会话列
/// 主谱系(无 tag 文件);平行子 agent 会话列**自己的**谱系(tag = agent 名)
/// —— 两边互相不可见,恢复也只重定向本谱系的落盘句柄。候选目录 = 当前会话
/// 文件所在目录(同一项目前缀),cwd 过滤与 `--continue` 一致;当前会话预选中。
async fn open_session_selector(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    // 流式期间切换会让进行中的 run 写入错误的 session 文件
    if ctx.session.current().agent().is_streaming() {
        state.commit_ephemeral(view::error_line(
            "run 进行中不能切换会话;等待 run 结束或 Esc 中止后再试",
            &state.theme,
        ));
        return;
    }
    let agent = ctx.session.active_agent();
    let (lineage_tag, current_file) = if let Some(name) = &agent {
        // 子会话谱系:holder 注册表里取本会话的落盘句柄(纯内存会话无存储)
        let Some(registry) = ctx.child_stores else {
            state.commit_ephemeral(warning_line_theme(
                "当前无会话存储,无法切换历史会话",
                &state.theme,
            ));
            return;
        };
        let Some(holder) = registry.lock().unwrap().get(name).cloned() else {
            state.commit_ephemeral(warning_line_theme(
                "当前无会话存储,无法切换历史会话",
                &state.theme,
            ));
            return;
        };
        (
            Some(name.clone()),
            holder.get().and_then(|manager| manager.file_path().map(|p| p.to_path_buf())),
        )
    } else {
        // 主谱系:全局 holder
        match ctx.current_manager() {
            Some(manager) => (
                None,
                manager.file_path().map(|path| path.to_path_buf()),
            ),
            None => {
                state.commit_ephemeral(warning_line_theme(
                    "当前无会话存储,无法切换历史会话",
                    &state.theme,
                ));
                return;
            }
        }
    };
    let Some(dir) = current_file
        .as_ref()
        .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
    else {
        state.commit_ephemeral(warning_line_theme(
            "当前无会话存储,无法切换历史会话",
            &state.theme,
        ));
        return;
    };
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let files = match &lineage_tag {
        Some(tag) => latent_session::list_session_files_with_tag(&dir, Some(&cwd), tag),
        None => latent_session::list_session_files(&dir, Some(&cwd)),
    };
    if files.is_empty() {
        let message = match &lineage_tag {
            Some(tag) => format!("没有 {tag} 的历史会话"),
            None => "没有历史会话(当前目录下还没有已保存的会话文件)".to_string(),
        };
        state.commit_ephemeral(warning_line_theme(&message, &state.theme));
        return;
    }
    let mut list = SelectList::new(
        files
            .iter()
            .map(|summary| format!("{}  {}", summary.local_time(), summary.preview))
            .collect(),
    );
    if let Some(index) = files.iter().position(|summary| Some(&summary.path) == current_file.as_ref()) {
        list.selected = index;
    }
    state.select = Some(SelectRequest {
        prompt: "切换历史会话(Enter 续聊;当前会话已保存,原样保留)".into(),
        list,
        kind: SelectKind::Session { files },
    });
}

/// Enter 选定历史会话:恢复**本谱系**的历史文件 —— 主会话走全局 holder(先
/// 终止旧会话的后台 subagent);平行子 agent 会话走自己的 holder(不碰主会话
/// 上下文与后台运行)。恢复 = 切 manager/重建上下文(assembly)→ 清空转录区
/// 并回放历史 → footer 刷新。
async fn switch_to_session(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    file: &std::path::Path,
) {
    let agent = ctx.session.active_agent();
    let holder = if let Some(name) = &agent {
        // 子会话谱系:重定向自己的 holder,sink 动态取 manager 后续写入新文件
        let Some(registry) = ctx.child_stores else {
            state.commit_ephemeral(warning_line_theme(
                "当前无会话存储,无法切换历史会话",
                &state.theme,
            ));
            return;
        };
        match registry.lock().unwrap().get(name).cloned() {
            Some(holder) => holder,
            None => {
                state.commit_ephemeral(warning_line_theme(
                    "当前无会话存储,无法切换历史会话",
                    &state.theme,
                ));
                return;
            }
        }
    } else {
        // 主谱系:旧会话的后台 subagent 一并终止(抑制完成通知)
        if let Some(registry) = ctx.subagent_registry {
            registry.abort_all();
        }
        match ctx.manager_holder {
            Some(holder) => holder.clone(),
            None => {
                state.commit_ephemeral(warning_line_theme(
                    "当前无会话存储,无法切换历史会话",
                    &state.theme,
                ));
                return;
            }
        }
    };
    match crate::assembly::switch_resume_session(&ctx.session.current(), &holder, file).await {
        Ok(path) => {
            // 清空转录区/用量后回放历史(replay 与启动恢复同源)
            state.reset_for_new_session();
            replay::replay_history(ctx, state);
            refresh_footer(ctx, state);
            let message = match (&path, &agent) {
                (Some(path), Some(name)) => {
                    format!("resumed session: {}(subagent {name})", path.display())
                }
                (Some(path), None) => format!("resumed session: {}", path.display()),
                (None, _) => "resumed session".to_string(),
            };
            // ephemeral:reset 已触发全文重绘,转录重渲染 + 待提交合并会把
            // commit_line 的条目画两遍
            state.commit_ephemeral(plain_dim(&message, &state.theme));
        }
        Err(error) => {
            state.commit_ephemeral(view::error_line(
                &format!("切换会话失败: {error}"),
                &state.theme,
            ));
        }
    }
    state.status = Status::Idle;
}

/// /session info 内容(markdown 源文本,经 `commit_markdown` 渲染上屏)。
fn session_info_markdown(ctx: &InteractiveCtx<'_>, state: &InteractiveState) -> String {
    let mut out = String::from("## 会话信息\n\n");
    match ctx.current_manager() {
        Some(manager) => {
            out.push_str(&format!("- id: `{}`\n", manager.session_id()));
            let file = manager
                .file_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(内存)".into());
            out.push_str(&format!("- file: `{file}`\n"));
        }
        None => out.push_str("- (无会话存储)\n"),
    }
    out.push_str(&format!("- model: `{}`\n", state.model_label));
    out.push_str(&format!("- thinking: `{}`\n", state.thinking_label));
    out.push_str(&format!(
        "- messages: {}\n",
        ctx.session.current().agent().messages().len()
    ));
    out.push_str(&format!(
        "- usage: {} tok · ${:.6}\n",
        state.usage.total.total_tokens, state.usage.total.cost.total
    ));
    // reserve 显示:< 1.0 是窗口百分比,换算成当前模型的实际 token 数一起展示
    let reserve = &ctx.compaction_config.reserve_tokens;
    let reserve_display = if *reserve > 0.0 && *reserve < 1.0 {
        let resolved = latent_session::reserve_tokens_for_window(*reserve, state.context_window);
        format!(
            "{:.0}%({} tok of {})",
            reserve * 100.0,
            resolved,
            state.context_window
        )
    } else {
        format!("{} tok", reserve)
    };
    out.push_str(&format!(
        "- compact: {}(reserve {}, keep recent {} tok)\n",
        if ctx.compaction_config.enabled {
            "auto"
        } else {
            "off"
        },
        reserve_display,
        ctx.compaction_config.keep_recent_tokens
    ));
    out
}

/// 从 Agent 状态快照刷新 footer 的模型/thinking/窗口字段。
pub fn refresh_footer(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    let snapshot = ctx.session.current().agent().state_snapshot();
    if let Some(model) = snapshot.model {
        state.model_label = format!("{}/{}", model.provider, model.id);
        state.context_window = model.context_window;
    }
    state.thinking_label = snapshot
        .thinking_level
        .map(|level| level.as_str().to_string())
        .unwrap_or_else(|| "off".into());
    state.mode_label = ctx.session.current().mode().label().to_string();
}

/// /thinking 选择器选项("off" = 关闭,其后为 ThinkingLevel::ALL 顺序)。
pub fn thinking_level_options() -> Vec<&'static str> {
    let mut names = vec!["off"];
    names.extend(
        latent_ai::ThinkingLevel::ALL
            .iter()
            .map(|level| level.as_str()),
    );
    names
}

/// /thinking 参数 → 设置值("off" = None;其余走装配期解析)。
fn parse_thinking_input(name: &str) -> Option<Option<latent_ai::ThinkingLevel>> {
    match name.to_ascii_lowercase().as_str() {
        "off" | "none" => Some(None),
        other => crate::assembly::parse_thinking_level(other).map(Some),
    }
}

fn plain_dim(text: &str, theme: &Theme) -> latent_tui::UiLine {
    ratatui::text::Line::from(ratatui::text::Span::styled(
        text.to_string(),
        ratatui::style::Style::new().fg(theme.muted),
    ))
}

pub(crate) fn warning_line_theme(text: &str, theme: &Theme) -> latent_tui::UiLine {
    ratatui::text::Line::from(ratatui::text::Span::styled(
        text.to_string(),
        ratatui::style::Style::new().fg(theme.warning),
    ))
}

/// session/UI 事件 → 状态变更与转录提交。
pub async fn handle_ui_event(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    event: UiEvent,
) {
    match event {
        UiEvent::Session(session_event) => handle_session_event(ctx, state, session_event).await,
        UiEvent::Notify(message) => {
            state.commit_line(plain_dim(&message, &state.theme));
        }
        UiEvent::Confirm { message, responder } => {
            state.select_queue.push_back(SelectRequest {
                prompt: message,
                list: SelectList::new(vec!["yes".into(), "no".into()]),
                kind: SelectKind::Confirm(responder),
            });
            state.promote_next_select();
        }
        UiEvent::Select {
            message,
            options,
            responder,
        } => {
            state.select_queue.push_back(SelectRequest {
                prompt: message,
                list: SelectList::new(options),
                kind: SelectKind::Select(responder),
            });
            state.promote_next_select();
        }
        UiEvent::Approval { request, responder } => {
            // 审批 overlay(13 文档 §10.3):原因文案 + 命令/路径详情 + 四决策
            let mode = ctx.session.current().mode();
            state.select_queue.push_back(SelectRequest {
                prompt: format!(
                    "审批 {} · {}\n{}",
                    request.tool_name,
                    request.reason.message(mode),
                    request.detail
                ),
                list: latent_tui::SelectList::new(vec![
                    "批准一次            (Enter/1)".into(),
                    "本会话批准同类      (2)".into(),
                    "拒绝                (Esc/3)".into(),
                    "中止本次任务        (Ctrl+C/4)".into(),
                ]),
                kind: SelectKind::Approval { responder },
            });
            state.promote_next_select();
        }
        UiEvent::Status(_) => {
            // 兼容保留:后台任务状态以 CompactDone/BashDone 事件为主
        }
        UiEvent::CompactDone(result) => match result {
            Ok(count) => {
                state.status = Status::Idle;
                state.commit_line(plain_dim(
                    &format!("compacted → {count} context messages"),
                    &state.theme,
                ));
                // 压缩后上下文重建,旧估计失效;下一回合结束前显示无 ctx%
                state.context_tokens = 0;
            }
            Err(error) => {
                state.status = Status::Idle;
                state.commit_ephemeral(view::error_line(
                    &format!("compact failed: {error}"),
                    &state.theme,
                ));
            }
        },
        UiEvent::BashDone {
            command,
            output,
            is_error,
            inject,
            ..
        } => {
            // 组前空行:Bash 背景块与上文分隔(commit_blank 去重)
            state.commit_blank();
            state.commit(TranscriptItem::Bash {
                command: command.clone(),
                output: output.clone(),
                is_error,
            });
            state.commit(TranscriptItem::Blank);
            if inject {
                // `!`(单感叹号):输出注入对话上下文(下一轮可见);`!!` 不注入
                if let Err(error) = ctx
                    .session
                    .current()
                    .record_bash_execution(command, output, None)
                    .await
                {
                    state.commit_ephemeral(view::error_line(
                        &format!("bash 上下文注入失败: {error}"),
                        &state.theme,
                    ));
                }
            }
            state.status = Status::Idle;
        }
    }
}

async fn handle_session_event(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    event: AgentSessionEvent,
) {
    match event {
            AgentSessionEvent::Agent(latent_agent::AgentEvent::MessageDelta { delta }) => {
                // 首 token 时刻(任一增量类型;TTFT = 首 delta − MessageStart)
                if state.first_delta_at.is_none() {
                    state.first_delta_at = Some(Instant::now());
                }
                match delta {
                latent_agent::MessageDeltaPayload::Text { delta } => {
                    // thinking → text 的交接点:thinking 块先于正文提交进转录
                    commit_pending_thinking(state);
                    state.stream_text.push_str(&delta);
                    state.status = Status::Thinking;
                }
                latent_agent::MessageDeltaPayload::Thinking { delta } => {
                    state.status = Status::Thinking;
                    state
                        .pending_thinking
                        .get_or_insert_with(String::new)
                        .push_str(&delta);
                }
                latent_agent::MessageDeltaPayload::ToolCallArgs { .. } => {
                    state.status = state
                        .pending_tools
                        .last()
                        .map(|(_, name, _)| Status::Tool(name.clone()))
                        .unwrap_or(Status::Thinking);
                }
                }
            },
        AgentSessionEvent::Agent(latent_agent::AgentEvent::MessageStart {
            message,
            started_at_ms,
            ..
        }) => {
            // 新 assistant 消息:重置流式缓冲与 thinking 累积,并作为消息组
            // 起始补一个空行(外框移除后组间靠空行分隔;commit_blank 去重)
            if matches!(message.as_ref(), latent_agent::AgentMessage::Assistant(_)) {
                state.stream_text.clear();
                state.pending_thinking = None;
                // 流式计时起点:重试装饰器把 Start 帧缓冲到首个内容 delta
                // 才放行,事件到达时刻不能作为 TTFT 起点(否则恒 0),用
                // 请求发出时刻回推;非请求路径(注入)回退当下时刻
                state.stream_started = Some(instant_from_ms(started_at_ms));
                state.first_delta_at = None;
                state.commit_blank();
            }
        }
        AgentSessionEvent::Agent(latent_agent::AgentEvent::MessageEnd { message }) => {
            match message.as_ref() {
                // assistant 定稿:thinking 块 + 正文(markdown)落盘;不追加
                // 空行(用量行自身的上下间隔由 usage_item_of 负责)。正文裸
                // 渲染无外框。
                latent_agent::AgentMessage::Assistant(_) => {
                    commit_pending_thinking(state);
                    flush_stream(state);
                }
                // 用户消息即时上屏(与回放一致;steering 亦可见)。
                // 消息组之间保留一个空行(框间靠空行分隔)
                latent_agent::AgentMessage::User { content, .. } => {
                    flush_stream(state);
                    state.commit_blank();
                    state.commit(TranscriptItem::User {
                        content: content.clone(),
                    });
                    state.commit_blank();
                }
                // 工具结果:标题(按终态铺背景色)+ 输出背景块;组前补空行
                // (背景块替代边框承担视觉分隔)
                latent_agent::AgentMessage::ToolResult {
                    tool_call_id,
                    tool_name,
                    is_error,
                    details,
                    ..
                } => {
                    flush_stream(state);
                    let output = message.tool_result_content().unwrap_or_default();
                    // 按 tool_call_id 精确配对(并行批的结果消息按源序/完成序
                    // 到达,不能按"最近一次 start"配对)
                    let (name, args) = state
                        .pending_tools
                        .iter()
                        .position(|(id, _, _)| id == tool_call_id)
                        .map(|index| {
                            let (_, name, args) = state.pending_tools.remove(index);
                            (name, args)
                        })
                        .unwrap_or_else(|| (tool_name.clone(), String::new()));
                    let status = if *is_error {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Success
                    };
                    // 异步 subagent:后台 run 尚未结算时标题保持 pending 态并
                    // 绑定 runId,帧循环在结算后翻终态(成功绿/失败红);
                    // 同步运行此刻已结算,直接终态
                    let run_binding = details
                        .as_ref()
                        .and_then(|details| details.get("runId"))
                        .and_then(|value| value.as_str())
                        .filter(|_| tool_name == latent_core::TOOL_NAME)
                        .filter(|id| {
                            ctx.subagent_registry
                                .is_some_and(|registry| {
                                    registry.run_state(id) == latent_core::RunState::Active
                                })
                        })
                        .map(str::to_string);
                    state.commit_blank();
                    let card_index = state.transcript.len();
                    state.commit(TranscriptItem::ToolCall {
                        name,
                        args,
                        status: if run_binding.is_some() {
                            ToolStatus::Pending
                        } else {
                            status
                        },
                    });
                    if let Some(run_id) = run_binding {
                        state.subagent_run_cards.push((run_id, card_index));
                    }
                    state.commit(TranscriptItem::ToolResult {
                        output,
                        is_error: *is_error,
                    });
                }
                _ => {}
            }
        }
        AgentSessionEvent::Agent(latent_agent::AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
            ..
        }) => {
            flush_stream(state);
            let args = serde_json::to_string(&args).unwrap_or_default();
            state
                .pending_tools
                .push((tool_call_id.clone(), tool_name.clone(), args));
            state.status = Status::Tool(tool_name);
        }
        AgentSessionEvent::Agent(latent_agent::AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial,
            ..
        }) => {
            // bash 等工具的执行期进度:partial 为滚动尾窗快照,直接替换
            // (预览区随帧重渲染;结果到达后由折叠的 ToolResult 定稿)
            state.pending_tool_output = Some((tool_call_id, partial));
        }
        AgentSessionEvent::Agent(latent_agent::AgentEvent::ToolExecutionEnd {
            tool_call_id,
            is_error,
            ..
        }) => {
            state.last_tool_error = is_error;
            if state
                .pending_tool_output
                .as_ref()
                .is_some_and(|(id, _)| *id == tool_call_id)
            {
                state.pending_tool_output = None;
            }
            state.status = Status::Thinking;
        }
        AgentSessionEvent::Agent(latent_agent::AgentEvent::TurnEnd { message, .. }) => {
            record_turn_speed(state, &message.usage);
            // 注意此处保持 busy(Idle 在 AgentSettled):tokens 块必须在视口
            // 收缩**前**按 busy 高度落盘,与上文(assistant 正文/工具背景块)
            // 紧贴;随后收缩释放的 1 行预留空带全部落在 tokens 下方(输入框
            // 一侧)。
            // pi assistant-message.ts 语义:error/aborted 红字上屏且不打
            // 用量行(错误回合无有效 usage);length 先打用量再补截断提示
            match message.stop_reason {
                StopReason::Error => {
                    let error = message
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Unknown error".into());
                    state.commit_ephemeral(view::error_line(&error, &state.theme));
                }
                StopReason::Aborted => {
                    state.commit_ephemeral(view::error_line("Operation aborted", &state.theme));
                }
                StopReason::Length => {
                    state.usage.push(&message.usage);
                    for item in usage_item_of(state, &message.usage) {
                        state.commit(item);
                    }
                    state.commit_ephemeral(view::error_line(
                        "Response was truncated before completion.",
                        &state.theme,
                    ));
                    state.context_tokens = context_tokens_of(&message.usage);
                }
                _ => {
                    state.usage.push(&message.usage);
                    for item in usage_item_of(state, &message.usage) {
                        state.commit(item);
                    }
                    state.context_tokens = context_tokens_of(&message.usage);
                }
            }
        }
        AgentSessionEvent::AgentSettled => {
            // 兜底:工具结果未到达时,标题仍要落盘(状态用终态色)
            let leftover = std::mem::take(&mut state.pending_tools);
            state.pending_tool_output = None;
            for (_, name, args) in leftover {
                let status = if state.last_tool_error {
                    ToolStatus::Error
                } else {
                    ToolStatus::Success
                };
                state.commit_blank();
                state.commit(TranscriptItem::ToolCall { name, args, status });
                // 补空结果条目,让背景块闭合(结果未到达的兜底路径)
                state.commit(TranscriptItem::ToolResult {
                    output: String::new(),
                    is_error: state.last_tool_error,
                });
            }
            state.status = Status::Idle;
        }
        AgentSessionEvent::QueueUpdate {
            steering,
            follow_up,
        } => {
            // run 期间入队的消息给可见反馈(pi 的 pending 队列提示)
            if steering + follow_up > 0 {
                state.commit_ephemeral(plain_dim(
                    &format!("queued · steering {steering} · follow-up {follow_up}"),
                    &state.theme,
                ));
            }
        }
        AgentSessionEvent::AutoRetryStart {
            attempt,
            delay_ms,
            reason,
        } => {
            state.commit_line(plain_dim(
                &format!("[retry #{attempt} in {delay_ms}ms] {reason}"),
                &state.theme,
            ));
        }
        // 重试最终失败:红字上屏(此前被吞,错误不可见)
        AgentSessionEvent::AutoRetryEnd {
            success: false,
            reason,
        } => {
            state.commit_ephemeral(view::error_line(
                &format!("Retry failed: {reason}"),
                &state.theme,
            ));
        }
        AgentSessionEvent::AutoRetryEnd { success: true, .. } => {}
        _ => {}
    }
}

/// 回合结束换算最近一回合的输出速度与首 token 延迟(footer 展示):
/// TTFT = 首 delta − 请求发出时刻(record_turn_speed;起点由 MessageStart
/// 携带的 started_at_ms 回推);TPS = 输出 token ÷ 生成时长
/// (首 delta → TurnEnd;TurnEnd 在工具执行前发出,时长即纯流式生成)。
/// 计时缺失或输出为 0 不更新,保留上一回合数值。
fn record_turn_speed(state: &mut InteractiveState, usage: &latent_ai::Usage) {
    let (started, first) = match (state.stream_started.take(), state.first_delta_at.take()) {
        (Some(started), Some(first)) => (started, first),
        _ => return,
    };
    let generation = (Instant::now() - first).as_secs_f64();
    if usage.output == 0 || generation <= 0.0 {
        return;
    }
    state.last_tps = Some(usage.output as f64 / generation);
    state.last_ttft = Some((first - started).as_secs_f64());
}

/// 流式累积 → assistant 定稿(markdown)转录条目(裸渲染,无外框)。
fn flush_stream(state: &mut InteractiveState) {
    if !state.stream_text.trim().is_empty() {
        let markdown = std::mem::take(&mut state.stream_text);
        // 计划模式产出的 <proposed_plan> 块渲染为紫色背景卡片(13 文档
        // §8.3);渲染只是展示层,原始文本仍按普通 assistant 消息入转录
        if markdown.contains("<proposed_plan>") {
            state.commit(TranscriptItem::Plan { markdown });
        } else {
            state.commit(TranscriptItem::Assistant { markdown });
        }
    } else {
        state.stream_text.clear();
    }
}

/// 提交流式期间累积的 thinking 块(空则跳过)。
fn commit_pending_thinking(state: &mut InteractiveState) {
    if let Some(thinking) = state.pending_thinking.take() {
        if !thinking.trim().is_empty() {
            state.commit(TranscriptItem::Thinking { text: thinking });
        }
    }
}

/// 单回合用量行上下各加一行空行(与正文/后续内容拉开间距,避免挤在一起);
/// 行首带本回合速度段(TPS · TTFT,record_turn_speed 已在 TurnEnd 换算)。
fn usage_item_of(state: &InteractiveState, usage: &latent_ai::Usage) -> Vec<TranscriptItem> {
    let speed = state.last_tps.zip(state.last_ttft);
    vec![
        TranscriptItem::Blank,
        TranscriptItem::Line(latent_tui::UiLine::from(usage_line(
            usage,
            state.usage.cache_ever_reported,
            speed,
            &state.theme,
        ))),
        TranscriptItem::Blank,
    ]
}
