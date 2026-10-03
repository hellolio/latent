//! interactive 模式(pi modes/interactive 的对应物):rpi-tui 搭建的聊天界面。
//!
//! 布局(pi 风格):启动区(横幅 + 分隔线 + 已加载资源)与消息区作为定稿
//! 行提交,经全帧差分屏幕滚入终端原生 scrollback;屏幕底部的活动尾部承载
//! 实时预览(≤4 行)/状态行(spinner,仅 busy)/两行间隔/补全弹窗/编辑器区
//! (带背景色)/三行 footer(cwd · 用量 · 模型)。扩展 UI(接缝 #5):
//! notify 上屏,confirm/select 渲染为尾部帧内的选择列表。
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
        subagent_registry,
        subagent_factory,
        ..
    } = built;
    let (theme, theme_name) = resolve_theme(theme_override.as_deref());

    let mut app = TuiApp::open().map_err(|e| e.to_string())?;

    // 键盘线程:crossterm 事件 → 归一 Key → channel。
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Key>();
    std::thread::spawn(move || {
        use ratatui::crossterm::event;
        loop {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        // 协议终端的修饰 Enter 已由 from_event 归一;裸 Enter
                        // 经本地修饰键兜底(macOS,见 normalize_native_enter)
                        let Some(key) =
                            rpi_tui::from_event(&event).map(rpi_tui::normalize_native_enter)
                        else {
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
    let router = crate::modes::interactive::handlers::SessionRouter::new(session.clone());
    let ctx = InteractiveCtx {
        session: &router,
        subagent_factory: subagent_factory.as_ref(),
        manager_holder: Some(&manager_holder),
        resolver: &resolver,
        compaction_config: &compaction_config,
        subagent_registry: subagent_registry.as_ref(),
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
    let partial = ctx.session.current().agent().partial_message();
    render_tick(state, app, partial.as_ref()).map_err(|e| e.to_string())?;

    loop {
        // 每轮同步宽度(Resize 经 crossterm 事件/逐帧尺寸查询进入全量重绘)
        state.width = app.width();

        // ctrl+o / 主题 / /new / 尺寸变化后的全文重绘:按当前展开态重组
        // 启动区与转录,整屏重打;挂起的瞬态行(如 theme → … 确认)并入
        // 本次文档(与旧 redraw_full 先 flush 的语义一致)
        if state.needs_full_redraw {
            state.needs_full_redraw = false;
            let mut transcript = full_redraw_lines(state);
            if !state.pending.is_empty() {
                let pending = std::mem::take(&mut state.pending);
                transcript.extend(pending);
            }
            let partial = ctx.session.current().agent().partial_message();
            let frame = build_frame(state, app, partial.as_ref());
            app.redraw_all(&transcript).map_err(|e| e.to_string())?;
            app.render(&frame.lines, frame.cursor)
                .map_err(|e| e.to_string())?;
            continue;
        }

        // 后台 subagent 计数(footer 状态段;活跃时驱动 spinner 帧)
        state.subagent_active = ctx
            .subagent_registry
            .map(|registry| registry.active_count())
            .unwrap_or(0);
        let partial = ctx.session.current().agent().partial_message();
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
            // busy 或有后台 subagent 时驱动 spinner 动画(两者皆空闲时
            // pending 永不就绪,零开销;14 文档 §4.3 进度可见)
            _ = async {
                if state.status.is_busy() || state.subagent_active > 0 {
                    tokio::time::sleep(SPINNER_INTERVAL).await;
                } else {
                    futures::future::pending::<()>().await;
                }
            } => {
                state.spin = state.spin.wrapping_add(1);
            }
        }
    }

    // 退出清理:停掉全部存活后台 subagent(抑制完成通知);当前会话与主
    // 会话(平行会话可能仍在 run)一并中止
    if let Some(registry) = ctx.subagent_registry {
        registry.abort_all();
    }
    ctx.session.current().abort();
    if !ctx.session.is_main() {
        ctx.session.main().abort();
        ctx.session.main().wait_idle().await;
    }
    ctx.session.current().wait_idle().await;
    Ok(())
}

/// 单帧渲染:① 尺寸变化检测(宽度变化须重折行,转全量重绘)→
/// ② flush 待提交定稿行(并入屏幕侧 committed 缓存)→ ③ 计算尾部帧 →
/// ④ 差分输出。全帧差分下尾部高度逐帧自由变化,无预留行数约束。
fn render_tick(
    state: &mut InteractiveState,
    app: &mut TuiApp,
    partial: Option<&rpi_ai::AssistantMessage>,
) -> std::io::Result<()> {
    if app.take_needs_reshape() {
        state.needs_full_redraw = true;
        return Ok(());
    }
    if !state.pending.is_empty() {
        let pending = std::mem::take(&mut state.pending);
        app.append_committed(&pending);
    }
    let frame = build_frame(state, app, partial);
    app.render(&frame.lines, frame.cursor)
}

/// 计算尾部帧(终端过矮时收缩流式预览/补全弹窗/编辑器行数;尾部总高
/// 不得超过屏幕行数,否则差分必须全量重绘兜底)。
fn build_frame(
    state: &mut InteractiveState,
    app: &TuiApp,
    partial: Option<&rpi_ai::AssistantMessage>,
) -> view::ViewportFrame {
    let budget = usize::from(app.screen_rows().max(1));
    let mut preview_cap = view::STREAM_PREVIEW_ROWS;
    let mut editor_cap = view::MAX_EDITOR_ROWS;
    let mut popup_cap = default_popup_cap(budget as u16);
    let mut frame = view::viewport(state, partial, preview_cap, editor_cap, popup_cap);
    while frame.height as usize > budget && preview_cap > 0 {
        preview_cap = preview_cap.saturating_sub(2);
        frame = view::viewport(state, partial, preview_cap, editor_cap, popup_cap);
    }
    if frame.height as usize > budget {
        popup_cap = 1;
        editor_cap = 1;
        frame = view::viewport(state, partial, 0, editor_cap, popup_cap);
    }
    frame
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
    for diagnostic in ctx.session.main().extension_diagnostics() {
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
