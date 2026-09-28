//! interactive 模式(pi modes/interactive 的对应物):rpi-tui 搭建的聊天界面。
//!
//! 布局(pi 风格):启动区(横幅 + 分隔线 + 已加载资源)与消息区提交进终端
//! 原生 scrollback;底部 Inline 视口承载预览区/状态行(spinner 与等待信息)/
//! 编辑器区(带背景色)/三行 footer(cwd · 用量 · 模型)。扩展 UI(接缝 #5):
//! notify 上屏,confirm/select 渲染为视口内的选择列表。
//!
//! 本模块只做装配与事件循环;状态在 state.rs、渲染在 view.rs、事件处理在
//! handlers.rs、bash 透传在 bash.rs。

mod bash;
mod events;
mod handlers;
mod replay;
mod state;
mod usage;
mod view;

use std::sync::Arc;
use std::time::Duration;

use rpi_tui::{Key, Theme, TuiApp};
use tokio::sync::mpsc;

use crate::assembly::BuiltSession;
pub use events::{create_tui_ui, TuiApprovalUi, TuiUi, UiEvent};

use handlers::InteractiveCtx;
use state::InteractiveState;

/// 主题解析(三层优先级):`--theme` / `/theme` 传入值 → settings.json 的
/// `theme` 字段 → 终端能力自动探测(默认 ratatui-themes Tokyo Night,
/// 仅 16 色终端降级 ANSI 兜底)。未知的显式名字告警后继续走默认链路。
/// 返回 (主题, 主题 slug;ANSI 兜底/自动探测时 None)。
fn resolve_theme(explicit: Option<&str>) -> (Theme, Option<String>) {
    let named = |name: &str, source: &str| match name.trim().parse::<rpi_tui::ThemeName>() {
        Ok(parsed) => Some((Theme::from_theme_name(parsed), Some(parsed.slug().to_string()))),
        Err(_) => {
            eprintln!("[rpi] {source} 的主题 `{name}` 无法识别,已回退默认主题");
            None
        }
    };
    if let Some(name) = explicit.map(str::trim).filter(|n| !n.is_empty()) {
        if let Some(resolved) = named(name, "参数") {
            return resolved;
        }
    }
    let cwd = std::env::current_dir().ok();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if let Some(name) = rpi_core::load_theme_setting(cwd.as_deref(), home.as_deref()) {
        if let Some(resolved) = named(&name, "settings.json") {
            return resolved;
        }
    }
    (Theme::detect(), None)
}

/// busy 时的 spinner 帧间隔。
const SPINNER_INTERVAL: Duration = Duration::from_millis(120);

pub async fn run_interactive_mode(
    built: BuiltSession,
    ui: TuiUi,
    mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    theme_override: Option<String>,
) -> Result<(), String> {
    let BuiltSession {
        session,
        manager_holder,
        compaction_config,
        ..
    } = built;
    let (theme, theme_name) = resolve_theme(theme_override.as_deref());

    let mut app = TuiApp::open(6).map_err(|e| e.to_string())?;

    // 键盘线程:crossterm 事件 → 归一 Key → channel。
    // 循环边界必须走 reader_checkpoint:Inline 视口重建/resize 期间要发
    // 光标位置查询,读取线程须暂停让路(fd 单读者,见 rpi_tui::app)。
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Key>();
    std::thread::spawn(move || {
        use ratatui::crossterm::event;
        loop {
            rpi_tui::reader_checkpoint();
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        let Some(key) = rpi_tui::from_event(&event) else {
                            continue;
                        };
                        if key_tx.send(key).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                },
                Ok(false) => {}
                Err(_) => return,
            }
        }
    });

    // session 事件与扩展 UI 调用共用同一事件通道(主循环统一渲染)
    session.subscribe(Arc::new(events::SessionToUiSubscriber {
        tx: ui.tx.clone(),
    }));

    // /model 解析与主流程同源:models.json + 内置 provider 默认表
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let resolver = rpi_core::create_model_resolver_from_config(Some(&cwd), home.as_deref());
    let ctx = InteractiveCtx {
        session: &session,
        manager_holder: Some(&manager_holder),
        resolver: &resolver,
        compaction_config: &compaction_config,
        ui_tx: ui.tx.clone(),
    };

    let mut state = InteractiveState::new(theme, app.width());
    state.theme_name = theme_name;
    state.cwd_display = rpi_tui::footer::abbreviate_home(
        &cwd.display().to_string(),
        home.as_deref().and_then(|p| p.to_str()),
    );
    state.git_branch = detect_git_branch(&cwd);
    refresh_footer_fields(&ctx, &mut state);

    // 启动区 + 回放(pi 语义:恢复/续聊时回放当前转录)
    commit_startup(&ctx, &mut state);
    replay::replay_history(&ctx, &mut state);

    let result = event_loop(&ctx, &mut state, &mut app, &mut key_rx, &mut ui_rx).await;

    app.finish().map_err(|e| e.to_string())?;
    result
}

async fn event_loop(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    app: &mut TuiApp,
    key_rx: &mut mpsc::UnboundedReceiver<Key>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
) -> Result<(), String> {
    let partial = ctx.session.agent().partial_message();
    render_tick(state, app, partial.as_ref()).map_err(|e| e.to_string())?;

    loop {
        // ctrl+o 切换后的全文重绘(先 flush 挂起的瞬态行,再整体重打)
        if state.needs_full_redraw {
            state.needs_full_redraw = false;
            if !state.pending.is_empty() {
                let pending = std::mem::take(&mut state.pending);
                app.commit_lines(&pending).map_err(|e| e.to_string())?;
            }
            let transcript = full_redraw_lines(state);
            let partial = ctx.session.agent().partial_message();
            let cap = preview_cap_for(state)
                .min(usize::from(app.viewport_height_cap().saturating_sub(8)));
            let frame = view::viewport(
                state,
                partial.as_ref(),
                cap,
                view::MAX_EDITOR_ROWS,
                default_popup_cap(app.viewport_height_cap()),
            );
            app.redraw_full(&transcript, &frame.lines, frame.cursor)
                .map_err(|e| e.to_string())?;
            continue;
        }

        let partial = ctx.session.agent().partial_message();
        render_tick(state, app, partial.as_ref()).map_err(|e| e.to_string())?;

        tokio::select! {
            key = key_rx.recv() => {
                let Some(key) = key else { break };
                if handlers::handle_key(ctx, state, key).await {
                    break;
                }
            }
            event = ui_rx.recv() => {
                let Some(event) = event else { break };
                handlers::handle_ui_event(ctx, state, event).await;
            }
            // busy 时驱动 spinner 动画(idle 时 pending 永不就绪,零开销)
            _ = async {
                if state.status.is_busy() {
                    tokio::time::sleep(SPINNER_INTERVAL).await;
                } else {
                    futures::future::pending::<()>().await;
                }
            } => {
                state.spin = state.spin.wrapping_add(1);
            }
        }
        // 每轮同步宽度(Resize 由 crossterm 事件驱动 TuiApp,状态侧跟随)
        state.width = app.width();
    }

    ctx.session.abort();
    ctx.session.wait_idle().await;
    Ok(())
}

/// 单帧渲染:① 计算帧 → ② 同步视口高度 → ③ flush 待提交转录 →
/// ④ 绘制视口。高度同步必须在 flush 之前:视口收缩释放的行会形成空带
/// (终端无法从 scrollback 拉回内容),flush 的定稿内容经 `insert_before`
/// 从视口上方往下填,正好把空带回填——回合结束后输出紧贴输入区,
/// 不再留下大片空白等下一次提交。
fn render_tick(
    state: &mut InteractiveState,
    app: &mut TuiApp,
    partial: Option<&rpi_ai::AssistantMessage>,
) -> std::io::Result<()> {
    let frame = build_frame(state, app, partial);
    app.set_viewport_height(frame.height)?;
    if !state.pending.is_empty() {
        let pending = std::mem::take(&mut state.pending);
        app.commit_lines(&pending)?;
    }
    app.draw_viewport(&frame.lines, frame.cursor)
}

/// 计算视口帧(终端过矮时收缩预览区/补全弹窗/编辑器行数)。
fn build_frame(
    state: &mut InteractiveState,
    app: &TuiApp,
    partial: Option<&rpi_ai::AssistantMessage>,
) -> view::ViewportFrame {
    let budget = app.viewport_height_cap();
    let mut preview_cap = preview_cap_for(state);
    let mut editor_cap = view::MAX_EDITOR_ROWS;
    let mut popup_cap = default_popup_cap(budget);
    let mut frame = view::viewport(state, partial, preview_cap, editor_cap, popup_cap);
    while frame.height > budget && preview_cap > 0 {
        preview_cap = preview_cap.saturating_sub(2);
        frame = view::viewport(state, partial, preview_cap, editor_cap, popup_cap);
    }
    if frame.height > budget {
        popup_cap = 1;
        editor_cap = 1;
        frame = view::viewport(state, partial, 0, editor_cap, popup_cap);
    }
    frame
}

/// 预览区行数上限:busy 期间(流式输出/思考/工具执行)固定为
/// `STREAM_PREVIEW_ROWS`(1),空闲时不占行(0)。两个设计点:
/// - **busy 一开始就把视口预增高到全程高度**,此后整回合不再增高,输入框
///   不因流式更新而跳动;
/// - **tokens 块在收缩前按 busy 高度落盘**(Idle 在 AgentSettled 才切),
///   与上文 AI 框紧贴;回合末收缩释放的预留空带全部落在 tokens 下方。
///   守恒:tokens→输入行空白 = 预留 + 状态行 + 编辑器内边距 = 3 行。
///
/// 超出终端预算时由 build_frame() 收缩截尾。
fn preview_cap_for(state: &InteractiveState) -> usize {
    if state.status.is_busy()
        || !state.stream_text.is_empty()
        || state
            .pending_thinking
            .as_ref()
            .is_some_and(|t| !t.trim().is_empty())
    {
        view::STREAM_PREVIEW_ROWS
    } else {
        view::MAX_PREVIEW_ROWS
    }
}

/// 补全弹窗的默认行数上限:不超过 8 行,且保证 composer(3 行)+ footer
/// (2 行)在预算内完整可见。
fn default_popup_cap(budget: u16) -> usize {
    budget
        .saturating_sub(5)
        .min(rpi_tui::command_popup::MAX_VISIBLE_ROWS as u16) as usize
}

/// 启动区:横幅 + 分隔线 + 已加载资源分节([Extensions] 等,ctrl+o 展开)。
/// 只进待提交缓冲;展开态变化时由 redraw 路径按当前状态重组。
fn commit_startup(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    state.commit_startup(view::welcome_lines(
        env!("CARGO_PKG_VERSION"),
        state.expanded,
        &state.theme,
        state.width,
    ));
    let mut extensions: Vec<String> = Vec::new();
    for diagnostic in ctx.session.extension_diagnostics() {
        extensions.push(format!(
            "{} (加载失败: {})",
            diagnostic.extension, diagnostic.message
        ));
    }
    if !extensions.is_empty() {
        state.resources.push(("Extensions".into(), extensions));
    }
    if !state.resources.is_empty() {
        state.commit_startup(resources_lines(state));
        state.commit_startup(vec![rpi_tui::UiLine::raw("")]);
    }
    state.commit_startup(vec![
        rpi_tui::header_view::separator(state.width, &state.theme),
        rpi_tui::UiLine::raw(""),
    ]);
}

/// 资源分节行(按当前展开态)。
fn resources_lines(state: &InteractiveState) -> Vec<rpi_tui::UiLine> {
    let sections: Vec<(&str, Vec<String>)> = state
        .resources
        .iter()
        .map(|(title, items)| (title.as_str(), items.clone()))
        .collect();
    rpi_tui::header_view::resources(&sections, state.expanded, state.width, &state.theme)
}

/// ctrl+o 全文重绘的数据源:启动区(按当前展开态)+ 转录。
fn full_redraw_lines(state: &InteractiveState) -> Vec<rpi_tui::UiLine> {
    let mut full = view::welcome_lines(
        env!("CARGO_PKG_VERSION"),
        state.expanded,
        &state.theme,
        state.width,
    );
    if !state.resources.is_empty() {
        full.extend(resources_lines(state));
        full.push(rpi_tui::UiLine::raw(""));
    }
    full.push(rpi_tui::header_view::separator(state.width, &state.theme));
    full.push(rpi_tui::UiLine::raw(""));
    full.extend(view::render_transcript(
        &state.transcript,
        &state.theme,
        state.width,
        state.expanded,
    ));
    full
}

fn refresh_footer_fields(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    handlers::refresh_footer(ctx, state);
}

/// git 分支探测(.git/HEAD,失败静默)。
fn detect_git_branch(cwd: &std::path::Path) -> Option<String> {
    let head = cwd.join(".git").join("HEAD");
    let content = std::fs::read_to_string(head).ok()?;
    let branch = content.trim().strip_prefix("ref: refs/heads/")?;
    Some(branch.to_string())
}

// Tests: handlers/state/view 的纯函数单测在各自模块;这里补装配级回放用例
#[cfg(test)]
mod tests;
