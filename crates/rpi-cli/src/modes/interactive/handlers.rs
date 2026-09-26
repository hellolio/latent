//! 事件处理:按键、斜杠命令、session/UI 事件 → 状态变更 + 转录提交。
//! 全部通过 `InteractiveState` 间接渲染(纯状态机,可直接单测)。

use std::time::{Duration, Instant};

use rpi_ai::StopReason;
use rpi_core::{AgentSessionEvent, ModelResolver};
use rpi_tui::{SelectList, Theme};
use tokio::sync::mpsc;

use crate::modes::slash;

use super::bash;
use super::events::UiEvent;
use super::state::{
    InteractiveState, SelectKind, SelectRequest, Status, ToolStatus, TranscriptItem,
};
use super::usage::{context_tokens_of, usage_line};
use super::view;

/// 键盘/命令处理共用的会话上下文(状态 + TUI 之外的全部依赖)。
pub struct InteractiveCtx<'a> {
    pub session: &'a Arc<rpi_core::AgentSession>,
    /// None = 内存会话(无 SessionManager)
    pub session_manager: Option<&'a Arc<rpi_session::SessionManager>>,
    /// `/model` 的候选与解析(models.json + 内置 provider 默认表)
    pub resolver: &'a ModelResolver,
    /// prompt 错误兜底回 UI 通道(事件流之外的装配/并发错误)
    pub ui_tx: mpsc::UnboundedSender<UiEvent>,
}

use std::sync::Arc;

/// pi 退出语义:500ms 内双击 Ctrl+C 退出(interactive-mode.ts:4121)。
const DOUBLE_CTRL_C_WINDOW: Duration = Duration::from_millis(500);

/// 键盘处理:返回 true 表示退出。
pub async fn handle_key(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    key: rpi_tui::Key,
) -> bool {
    // 选择列表激活时,键盘由列表接管
    if state.select.is_some() {
        handle_select_key(ctx, state, key);
        return false;
    }

    // 斜杠补全弹窗跟随编辑器内容(直接 set_text 的路径也同步)
    state.sync_slash_popup();

    // 弹窗可见时的 Codex 交互:↑/↓ 选择、Tab/Enter 补全、Esc 关闭;
    // 查询已与命令名完全一致时 Enter 不拦截,落入提交分支直接执行
    if state.slash_popup.visible() {
        match key {
            rpi_tui::Key::Up => {
                state.slash_popup.move_up();
                return false;
            }
            rpi_tui::Key::Down => {
                state.slash_popup.move_down();
                return false;
            }
            rpi_tui::Key::Tab | rpi_tui::Key::Enter if !state.slash_popup.is_exact_match() => {
                if let Some(text) = state.slash_popup.complete_text() {
                    state.editor.set_text(&text);
                }
                state.sync_slash_popup();
                return false;
            }
            rpi_tui::Key::Esc => {
                state.slash_popup.dismiss();
                return false;
            }
            _ => {}
        }
    }

    match key {
        rpi_tui::Key::Enter => {
            let Some(text) = state.take_input() else {
                return false;
            };
            state.sync_slash_popup();
            submit_input(ctx, state, text).await;
            false
        }
        rpi_tui::Key::Ctrl('o') => {
            state.expanded = !state.expanded;
            state.needs_full_redraw = true;
            false
        }
        rpi_tui::Key::Ctrl('c') => {
            if ctx.session.agent().is_streaming() {
                ctx.session.abort();
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
        rpi_tui::Key::Esc => {
            state.last_ctrl_c = None;
            if ctx.session.agent().is_streaming() {
                ctx.session.abort();
                state.status = Status::Aborted;
            }
            false
        }
        rpi_tui::Key::Ctrl('d') if state.editor.is_empty() => true,
        key => {
            state.last_ctrl_c = None;
            state.editor_key(&key);
            state.sync_slash_popup();
            false
        }
    }
}

/// 选择列表激活时的按键分派。
fn handle_select_key(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState, key: rpi_tui::Key) {
    match key {
        rpi_tui::Key::Up => {
            if let Some(select) = state.select.as_mut() {
                select.list.move_up();
            }
        }
        rpi_tui::Key::Down => {
            if let Some(select) = state.select.as_mut() {
                select.list.move_down();
            }
        }
        rpi_tui::Key::Enter => {
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
                            state.status = Status::Idle;
                            state.commit_ephemeral(warning_line_theme(
                                &format!("model → {}", state.model_label),
                                &state.theme,
                            ));
                        }
                    }
                    SelectKind::Thinking => {
                        if let Some(name) = thinking_level_options().get(index) {
                            let level = crate::assembly::parse_thinking_level(name);
                            ctx.session.set_thinking_level(level);
                            refresh_footer(ctx, state);
                            state.status = Status::Idle;
                        }
                    }
                    SelectKind::Theme { names } => {
                        if let Some(name) = names.get(index) {
                            apply_theme(state, *name);
                        }
                    }
                }
            }
            state.promote_next_select();
        }
        rpi_tui::Key::Esc | rpi_tui::Key::Ctrl('c') => {
            // 模态期间 Esc/Ctrl+C 取消当前请求(不退出;再按 Ctrl+C 才退出)
            if let Some(request) = state.select.take() {
                match request.kind {
                    SelectKind::Confirm(responder) => {
                        let _ = responder.send(false);
                    }
                    SelectKind::Select(responder) => {
                        let _ = responder.send(None);
                    }
                    SelectKind::Model { .. } | SelectKind::Thinking => {}
                    SelectKind::Theme { .. } => {}
                }
            }
            state.promote_next_select();
        }
        _ => {}
    }
}

/// 提交输入:`!`/`!!` bash 透传、`/` 斜杠命令、普通 prompt。
async fn submit_input(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState, text: String) {
    // `!` bash 透传(优先于斜杠解析)
    if let Some(rest) = text.strip_prefix('!') {
        let (bang_bang, command) = match rest.strip_prefix('!') {
            Some(command) => (true, command.trim()),
            None => (false, rest.trim()),
        };
        if command.is_empty() {
            state.commit_ephemeral(warning_line_theme("usage: !<command>", &state.theme));
            return;
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
        return;
    }

    match slash::parse(&text) {
        slash::SlashInput::NotACommand(text) => {
            state.status = Status::Thinking;
            state.stream_text.clear();
            state.pending_thinking = None;
            let session = ctx.session.clone();
            let ui_tx = ctx.ui_tx.clone();
            // prompt 任务在后台跑;事件经订阅者回流上屏,stdin 保持可响应
            // (run 期间的输入经 session.prompt 自动转 steer)
            tokio::spawn(async move {
                if let Err(error) = session.prompt(text).await {
                    let _ = ui_tx.send(UiEvent::Notify(format!("Error: {error}")));
                }
            });
        }
        slash::SlashInput::Unknown(name) => {
            // rpi 无动态命令源:未知 /xxx 本地警告,不发给模型
            state.commit_ephemeral(warning_line_theme(
                &format!("Unknown command: {name}(输入 /help 查看可用命令)"),
                &state.theme,
            ));
        }
        slash::SlashInput::Command(action) => {
            execute_command(ctx, state, action).await;
        }
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
            for line in slash::help_lines() {
                state.commit_line(plain_dim(&line, &state.theme));
            }
        }
        slash::SlashAction::Quit => return true,
        slash::SlashAction::Session => {
            for line in session_info_lines(ctx, state) {
                state.commit_line(plain_dim(&line, &state.theme));
            }
        }
        slash::SlashAction::Compact { arg } => {
            // 流式期间压缩会让 SessionCompactor 先落盘 Compaction entry、
            // 随后 set_messages 失败;摘要 LLM 调用内联 await 会冻结事件循环
            if ctx.session.agent().is_streaming() {
                state.commit_ephemeral(view::error_line(
                    "run 进行中不能压缩;等待 run 结束或 Esc 中止后再试",
                    &state.theme,
                ));
                return false;
            }
            if arg.is_some() {
                state.commit_ephemeral(plain_dim("自定义压缩指令暂不支持,已忽略", &state.theme));
            }
            state.status = Status::Compacting;
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
                    state.status = Status::Idle;
                }
                Err(error) => {
                    state.commit_ephemeral(view::error_line(
                        &format!("model 切换失败: {error}"),
                        &state.theme,
                    ));
                }
            },
            None => open_model_selector(ctx, state),
        },
        slash::SlashAction::Theme { arg } => match arg {
            Some(name) => match name.parse::<rpi_tui::ThemeName>() {
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
                    ctx.session.set_thinking_level(level);
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

fn open_model_selector(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    let models = ctx.resolver.available_models();
    if models.is_empty() {
        state.commit_ephemeral(warning_line_theme(
            "没有可选模型(models.json 或内置 provider)",
            &state.theme,
        ));
        return;
    }
    let specs: Vec<String> = models
        .iter()
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect();
    let mut list = SelectList::new(specs);
    if let Some(index) = list
        .options
        .iter()
        .position(|spec| *spec == state.model_label)
    {
        list.selected = index;
    }
    state.select = Some(SelectRequest {
        prompt: "选择模型".into(),
        list,
        kind: SelectKind::Model { models },
    });
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
fn apply_theme(state: &mut InteractiveState, name: rpi_tui::ThemeName) {
    state.theme = rpi_tui::Theme::from_theme_name(name);
    state.theme_name = Some(name.slug().to_string());
    state.needs_full_redraw = true;
    state.commit_ephemeral(warning_line_theme(
        &format!("theme → {}", name.display_name()),
        &state.theme,
    ));
}

fn open_theme_selector(state: &mut InteractiveState) {
    let names: Vec<rpi_tui::ThemeName> = rpi_tui::ThemeName::all().to_vec();
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

fn session_info_lines(ctx: &InteractiveCtx<'_>, state: &InteractiveState) -> Vec<String> {
    let mut lines = vec!["session".to_string()];
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
    lines.push(format!(
        "  messages: {}",
        ctx.session.agent().messages().len()
    ));
    lines.push(format!(
        "  usage:    {} tok · ${:.6}",
        state.usage.total.total_tokens, state.usage.total.cost.total
    ));
    lines
}

/// 从 Agent 状态快照刷新 footer 的模型/thinking/窗口字段。
pub fn refresh_footer(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    let snapshot = ctx.session.agent().state_snapshot();
    if let Some(model) = snapshot.model {
        state.model_label = format!("{}/{}", model.provider, model.id);
        state.context_window = model.context_window;
    }
    state.thinking_label = snapshot
        .thinking_level
        .map(|level| level.as_str().to_string())
        .unwrap_or_else(|| "off".into());
}

/// /thinking 选择器选项("off" = 关闭,其后为 ThinkingLevel::ALL 顺序)。
pub fn thinking_level_options() -> Vec<&'static str> {
    let mut names = vec!["off"];
    names.extend(
        rpi_ai::ThinkingLevel::ALL
            .iter()
            .map(|level| level.as_str()),
    );
    names
}

/// /thinking 参数 → 设置值("off" = None;其余走装配期解析)。
fn parse_thinking_input(name: &str) -> Option<Option<rpi_ai::ThinkingLevel>> {
    match name.to_ascii_lowercase().as_str() {
        "off" | "none" => Some(None),
        other => crate::assembly::parse_thinking_level(other).map(Some),
    }
}

fn plain_dim(text: &str, theme: &Theme) -> rpi_tui::UiLine {
    ratatui::text::Line::from(ratatui::text::Span::styled(
        text.to_string(),
        ratatui::style::Style::new().fg(theme.muted),
    ))
}

fn warning_line_theme(text: &str, theme: &Theme) -> rpi_tui::UiLine {
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
    _ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    event: AgentSessionEvent,
) {
    match event {
        AgentSessionEvent::Agent(rpi_agent::AgentEvent::MessageDelta { delta }) => match delta {
            rpi_agent::MessageDeltaPayload::Text { delta } => {
                // thinking → text 的交接点:thinking 块先于正文提交进转录
                commit_pending_thinking(state);
                state.stream_text.push_str(&delta);
                state.status = Status::Thinking;
            }
            rpi_agent::MessageDeltaPayload::Thinking { delta } => {
                state.status = Status::Thinking;
                state
                    .pending_thinking
                    .get_or_insert_with(String::new)
                    .push_str(&delta);
            }
            rpi_agent::MessageDeltaPayload::ToolCallArgs { .. } => {
                state.status = state
                    .current_tool
                    .as_ref()
                    .map(|(name, _)| Status::Tool(name.clone()))
                    .unwrap_or(Status::Thinking);
            }
        },
        AgentSessionEvent::Agent(rpi_agent::AgentEvent::MessageStart { message, .. }) => {
            // 新 assistant 消息:重置流式缓冲与 thinking 累积
            if matches!(message.as_ref(), rpi_agent::AgentMessage::Assistant(_)) {
                state.stream_text.clear();
                state.pending_thinking = None;
            }
        }
        AgentSessionEvent::Agent(rpi_agent::AgentEvent::MessageEnd { message }) => {
            match message.as_ref() {
                // assistant 定稿:thinking 块 + 正文(markdown)落盘,后随空行
                rpi_agent::AgentMessage::Assistant(_) => {
                    commit_pending_thinking(state);
                    flush_stream(state);
                    state.commit(TranscriptItem::Blank);
                }
                // 用户消息即时上屏(与回放一致;steering 亦可见)
                rpi_agent::AgentMessage::User { content, .. } => {
                    flush_stream(state);
                    state.commit(TranscriptItem::User {
                        content: content.clone(),
                    });
                    state.commit(TranscriptItem::Blank);
                }
                // 工具结果:标题(按终态着色)+ 输出块
                rpi_agent::AgentMessage::ToolResult {
                    tool_name,
                    is_error,
                    ..
                } => {
                    flush_stream(state);
                    let output = message.tool_result_content().unwrap_or_default();
                    let (name, args) = state
                        .current_tool
                        .take()
                        .unwrap_or_else(|| (tool_name.clone(), String::new()));
                    let status = if *is_error {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Success
                    };
                    state.commit(TranscriptItem::ToolCall { name, args, status });
                    state.commit(TranscriptItem::ToolResult {
                        output,
                        is_error: *is_error,
                    });
                }
                _ => {}
            }
        }
        AgentSessionEvent::Agent(rpi_agent::AgentEvent::ToolExecutionStart {
            tool_name,
            args,
            ..
        }) => {
            flush_stream(state);
            let args = serde_json::to_string(&args).unwrap_or_default();
            state.current_tool = Some((tool_name.clone(), args));
            state.status = Status::Tool(tool_name);
        }
        AgentSessionEvent::Agent(rpi_agent::AgentEvent::ToolExecutionEnd { is_error, .. }) => {
            state.last_tool_error = is_error;
            state.status = Status::Thinking;
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
                    state.commit_ephemeral(view::error_line(&error, &state.theme));
                }
                StopReason::Aborted => {
                    state.commit_ephemeral(view::error_line("Operation aborted", &state.theme));
                }
                StopReason::Length => {
                    state.usage.push(&message.usage);
                    state.commit(usage_item_of(&message.usage, &state.theme));
                    state.commit_ephemeral(view::error_line(
                        "Response was truncated before completion.",
                        &state.theme,
                    ));
                    state.context_tokens = context_tokens_of(&message.usage);
                }
                _ => {
                    state.usage.push(&message.usage);
                    state.commit(usage_item_of(&message.usage, &state.theme));
                    state.context_tokens = context_tokens_of(&message.usage);
                }
            }
        }
        AgentSessionEvent::AgentSettled => {
            // 兜底:工具结果未到达时,标题仍要落盘(状态用终态色)
            if let Some((name, args)) = state.current_tool.take() {
                let status = if state.last_tool_error {
                    ToolStatus::Error
                } else {
                    ToolStatus::Success
                };
                state.commit(TranscriptItem::ToolCall { name, args, status });
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

/// 流式累积 → assistant 定稿(markdown)转录条目。
fn flush_stream(state: &mut InteractiveState) {
    if !state.stream_text.trim().is_empty() {
        let markdown = std::mem::take(&mut state.stream_text);
        state.commit(TranscriptItem::Assistant { markdown });
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

fn usage_item_of(usage: &rpi_ai::Usage, theme: &Theme) -> TranscriptItem {
    TranscriptItem::Line(usage_line(usage, theme))
}
