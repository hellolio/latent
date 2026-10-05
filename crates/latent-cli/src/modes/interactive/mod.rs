//! interactive 模式(pi modes/interactive 的对应物):latent-tui 搭建的聊天界面。
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

use latent_tui::{Key, Theme, TuiApp};
use tokio::sync::mpsc;

use crate::assembly::BuiltSession;
pub use events::{create_tui_ui, TuiApprovalUi, TuiUi, UiEvent};

use handlers::InteractiveCtx;
use state::{InteractiveState, ScrollRequest};

/// 主题解析(三层优先级):`--theme` / `/theme` 传入值 → settings.json 的
/// `theme` 字段 → 终端能力自动探测(默认 ratatui-themes Tokyo Night,
/// 仅 16 色终端降级 ANSI 兜底)。未知的显式名字告警后继续走默认链路。
/// 返回 (主题, 主题 slug;ANSI 兜底/自动探测时 None)。
fn resolve_theme(explicit: Option<&str>) -> (Theme, Option<String>) {
    let named = |name: &str, source: &str| match name.trim().parse::<latent_tui::ThemeName>() {
        Ok(parsed) => Some((Theme::from_theme_name(parsed), Some(parsed.slug().to_string()))),
        Err(_) => {
            eprintln!("[latent] {source} 的主题 `{name}` 无法识别,已回退默认主题");
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
    let dir = latent_core::latent_dir(home.as_deref());
    if let Some(name) = latent_core::load_theme_setting(cwd.as_deref(), dir.as_deref()) {
        if let Some(resolved) = named(&name, "settings.json") {
            return resolved;
        }
    }
    (Theme::detect(), None)
}

/// busy 时的 spinner 帧间隔。
const SPINNER_INTERVAL: Duration = Duration::from_millis(120);

/// TUI 渲染模式解析(三层优先级):`--tui-mode` → settings.json `tuiMode`
/// → 默认 fullscreen(pi 同款)。非法值告警后继续走默认链路。
/// 返回 true = fullscreen(alternate screen)。
fn resolve_tui_mode(explicit: Option<&str>) -> bool {
    let parse = |name: &str, source: &str| match name.trim() {
        "fullscreen" => Some(true),
        "regular" => Some(false),
        _ => {
            eprintln!("[latent] {source} 的 tuiMode `{name}` 无效(fullscreen|regular),已回退默认");
            None
        }
    };
    if let Some(name) = explicit.map(str::trim).filter(|n| !n.is_empty()) {
        if let Some(resolved) = parse(name, "参数") {
            return resolved;
        }
    }
    let cwd = std::env::current_dir().ok();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let dir = latent_core::latent_dir(home.as_deref());
    if let Some(name) = latent_core::load_tui_mode_setting(cwd.as_deref(), dir.as_deref()) {
        if let Some(resolved) = parse(&name, "settings.json") {
            return resolved;
        }
    }
    true
}

pub async fn run_interactive_mode(
    built: BuiltSession,
    ui: TuiUi,
    mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    theme_override: Option<String>,
    tui_mode_override: Option<String>,
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

    let fullscreen = resolve_tui_mode(tui_mode_override.as_deref());
    let mut app = TuiApp::open_with_mode(fullscreen).map_err(|e| e.to_string())?;

    // /setting 可持久化设置:选中后自动复制(默认关)与 Ctrl+X 复制
    // 开关(默认开)
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let dir = latent_core::latent_dir(home.as_deref());
    let copy_on_select =
        latent_core::load_copy_on_select_setting(Some(&cwd), dir.as_deref()).unwrap_or(false);
    let ctrl_x_copy =
        latent_core::load_ctrl_x_copy_setting(Some(&cwd), dir.as_deref()).unwrap_or(true);

    // 键盘线程暂停开关:TUI 挂起跑外部编辑器($EDITOR)期间置位,线程
    // 停止读取 crossterm 事件,把终端输入让给子进程。
    let keys_paused = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // 键盘线程:crossterm 事件 → 归一 Key → channel。
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Key>();
    {
        let keys_paused = Arc::clone(&keys_paused);
        std::thread::spawn(move || {
            use ratatui::crossterm::event;
            loop {
                if keys_paused.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => match event::read() {
                        Ok(event) => {
                            // 协议终端的修饰 Enter 已由 from_event 归一;裸 Enter
                            // 经本地修饰键兜底(macOS,见 normalize_native_enter)
                            let Some(key) =
                                latent_tui::from_event(&event).map(latent_tui::normalize_native_enter)
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
    }

    // session 事件与扩展 UI 调用共用同一事件通道(主循环统一渲染)
    session.subscribe(Arc::new(events::SessionToUiSubscriber {
        tx: ui.tx.clone(),
    }));

    // /model 解析与主流程同源:models.json + 内置 provider 默认表。
    // RwLock:/model 配置入口(编辑 models.json/表单)运行期热重载
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver_from_config(
        Some(&cwd),
        latent_core::latent_dir(home.as_deref()).as_deref(),
    ));
    // 打字门控:编辑器非空 = 用户正在输入,supervisor 的后台 subagent 结算
    // 唤醒延迟(有上限),避免抢在用户提交前拉起新 turn;帧循环每轮回写
    let user_composing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(registry) = &subagent_registry {
        let gate = user_composing.clone();
        registry.set_wake_gate(Arc::new(move || gate.load(std::sync::atomic::Ordering::Relaxed)));
    }

    let router = crate::modes::interactive::handlers::SessionRouter::new(session.clone());
    let ctx = InteractiveCtx {
        session: &router,
        subagent_factory: subagent_factory.as_ref(),
        manager_holder: Some(&manager_holder),
        resolver: &resolver,
        compaction_config: &compaction_config,
        subagent_registry: subagent_registry.as_ref(),
        user_composing: Some(&user_composing),
        ui_tx: ui.tx.clone(),
    };

    let mut state = InteractiveState::new(theme, app.width());
    state.theme_name = theme_name;
    state.fullscreen = fullscreen;
    state.copy_on_select = copy_on_select;
    state.ctrl_x_copy = ctrl_x_copy;
    state.cwd_display = latent_tui::footer::abbreviate_home(
        &cwd.display().to_string(),
        home.as_deref().and_then(|p| p.to_str()),
    );
    state.git_branch = detect_git_branch(&cwd);
    // `@` 文件弹窗的候选采集:cwd 与检索忽略规则与工具装配同源(`.latentignore`
    // 全局+项目;本模块按惯例自行加载,与 theme/tuiMode 一致)
    state.cwd = cwd;
    state.mention_ignore = Arc::new(crate::assembly::load_search_ignore());
    refresh_footer_fields(&ctx, &mut state);

    // 启动区 + 回放(pi 语义:恢复/续聊时回放当前转录)
    commit_startup(&ctx, &mut state);
    replay::replay_history(&ctx, &mut state);

    let result =
        event_loop(&ctx, &mut state, &mut app, &mut key_rx, &mut ui_rx, &keys_paused).await;

    app.finish().map_err(|e| e.to_string())?;
    result
}

async fn event_loop(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    app: &mut TuiApp,
    key_rx: &mut mpsc::UnboundedReceiver<Key>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    keys_paused: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let partial = ctx.session.current().agent().partial_message();
    render_tick(state, app, partial.as_ref()).map_err(|e| e.to_string())?;

    loop {
        // 每轮取 toast 到期时刻(拥有值,避免 select 分支间借用冲突)
        let toast_deadline = app.toast_deadline();
        // 每轮同步宽度(Resize 经 crossterm 事件/逐帧尺寸查询进入全量重绘)
        state.width = app.width();
        state.fullscreen = app.is_fullscreen();
        app.set_auto_copy_on_select(state.copy_on_select);

        // 全屏滚动请求:handlers 只置标记,这里转交 TuiApp(下一次
        // render 按新视口位置差分输出)
        if let Some(request) = state.scroll_request.take() {
            match request {
                ScrollRequest::PageUp => app.scroll_page_up(),
                ScrollRequest::PageDown => app.scroll_page_down(),
                ScrollRequest::Top => app.scroll_top(),
                ScrollRequest::Bottom => app.scroll_bottom(),
                ScrollRequest::Lines(n) => app.scroll_lines(n),
            }
        }

        // 全屏 ↔ regular 切换(/fullscreen):写终端序列后走全文重绘
        // (切回 regular 时主屏仍是进入前的旧内容,必须重打整份文档)
        if let Some(target) = state.tui_mode_switch.take() {
            app.set_fullscreen(target).map_err(|e| e.to_string())?;
            state.fullscreen = target;
            state.needs_full_redraw = true;
        }

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
        // 打字门控回写(supervisor 唤醒延迟依据)+ 异步 subagent 卡片结算
        // 翻转(run 结算 → 标题终态着色,全文重绘至多一次)
        handlers::sync_wake_gate(ctx.user_composing, state);
        handlers::flush_settled_subagent_cards(ctx.subagent_registry, state);
        let partial = ctx.session.current().agent().partial_message();
        render_tick(state, app, partial.as_ref()).map_err(|e| e.to_string())?;

        tokio::select! {
            key = key_rx.recv() => {
                let Some(key) = key else { break };
                // 鼠标选区手势与复制请求:纯屏幕层操作,直接交 TuiApp,
                // 不经编辑器/命令状态机(highlight 变化由下一帧差分上屏)
                match key {
                    // 鼠标手势:全屏模式下恒可用(拖选/高亮/扩展),
                    // 自动复制与否由 app 侧 auto_copy_on_select 决定
                    latent_tui::Key::Mouse(action) => {
                        app.on_mouse(action).map_err(|e| e.to_string())?;
                    }
                    // 有选区时 Ctrl+X = 复制并清除高亮(/setting 可关;复制
                    // 成功由右上角 Copied! toast 反馈)。Ctrl 组合键任何终端
                    // 都转发,避开 cmd 键被终端占用的问题;无选区时 Ctrl+X
                    // 原样落编辑器(当前无操作),Ctrl+C 保持中断/双击退出
                    latent_tui::Key::Ctrl('x') if state.ctrl_x_copy && app.has_selection() => {
                        app.copy_selection().map_err(|e| e.to_string())?;
                        app.clear_selection();
                    }
                    key => {
                        if handlers::handle_key(ctx, state, key).await {
                            break;
                        }
                    }
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
            // 右上角 toast 到期:触发一轮重绘,差分消除提示框
            _ = async {
                match toast_deadline {
                    Some(deadline) => {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                    }
                    None => futures::future::pending::<()>().await,
                }
            } => {}
        }

        // handlers 置位的挂起动作(如 $EDITOR 编辑 models.json):TUI 挂起
        // 期间同步执行,返回后恢复渲染
        match state.suspend_action.take() {
            Some(state::SuspendAction::EditModelsJson) => {
                run_models_editor(ctx, state, app, keys_paused)?;
            }
            None => {}
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

/// /model 配置入口「$EDITOR 编辑 models.json」:TUI 挂起 → 子进程继承终端
/// 跑编辑器(缺文件先写模板)→ 恢复并热重载 → 重开 /model 选择器。
/// 阻塞事件循环是有意的:编辑期间键盘线程已暂停、UI 不渲染。
fn run_models_editor(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    app: &mut TuiApp,
    keys_paused: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let dir = latent_core::latent_dir(state.home.as_deref());
    let path = latent_core::preferred_models_path(Some(&state.cwd), dir.as_deref());
    if let Err(error) = latent_core::write_models_template_if_absent(&path) {
        state.commit_ephemeral(view::error_line(
            &format!("无法创建 {}: {error}", path.display()),
            &state.theme,
        ));
        return Ok(());
    }
    // LATENT_EDITOR 便于测试注入;否则走通用 VISUAL/EDITOR,兜底 vi
    let editor = std::env::var("LATENT_EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    keys_paused.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Err(e) = app.suspend() {
        keys_paused.store(false, std::sync::atomic::Ordering::Relaxed);
        return Err(e.to_string());
    }
    // `editor "$1"`:编辑器串可带参数(如 `code -w`),路径含空格也安全
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status();
    keys_paused.store(false, std::sync::atomic::Ordering::Relaxed);
    let document = full_redraw_lines(state);
    app.resume(&document).map_err(|e| e.to_string())?;
    state.needs_full_redraw = false;
    match status {
        Ok(status) if !status.success() => {
            let code = status.code().map(|c| c.to_string()).unwrap_or_default();
            state.commit_ephemeral(handlers::warning_line_theme(
                &format!("编辑器异常退出({code}),models.json 可能未保存"),
                &state.theme,
            ));
        }
        Err(e) => {
            state.commit_ephemeral(view::error_line(
                &format!("无法启动编辑器 `{editor}`: {e}"),
                &state.theme,
            ));
        }
        Ok(_) => {}
    }
    // 无论编辑成败都重载(幂等);诊断由 latent-core 打 stderr
    handlers::reload_model_resolver(ctx, state);
    let count = ctx.resolver.read().unwrap().available_models().len();
    state.commit_ephemeral(handlers::warning_line_theme(
        &format!("models.json 已重载({count} 个候选模型)"),
        &state.theme,
    ));
    handlers::open_model_selector(ctx, state);
    Ok(())
}

/// 单帧渲染:① 尺寸变化检测(宽度变化须重折行,转全量重绘)→
/// ② flush 待提交定稿行(并入屏幕侧 committed 缓存)→ ③ 计算尾部帧 →
/// ④ 差分输出。全帧差分下尾部高度逐帧自由变化,无预留行数约束。
fn render_tick(
    state: &mut InteractiveState,
    app: &mut TuiApp,
    partial: Option<&latent_ai::AssistantMessage>,
) -> std::io::Result<()> {
    if app.take_needs_reshape() {
        state.needs_full_redraw = true;
        return Ok(());
    }
    if !state.pending.is_empty() {
        let pending = std::mem::take(&mut state.pending);
        app.append_committed(&pending);
    }
    // follow 态在滚动请求消费后可能已翻转,渲染前重新同步(view 据此
    // 决定是否构建预览全量行)
    state.following = app.is_following();
    let frame = build_frame(state, app, partial);
    // 预览滚动上下文随帧注入:全屏非 follow 布局据此把流式预览并入
    // 滚动视口(滚动数学用全量行数,渲染用全量行)
    app.set_scroll_tail(
        frame.preview_window,
        frame.preview_full_len,
        &frame.scroll_extra,
    );
    app.render(&frame.lines, frame.cursor)
}

/// 计算尾部帧(终端过矮时收缩流式预览/补全弹窗/编辑器行数;尾部总高
/// 不得超过屏幕行数,否则差分必须全量重绘兜底)。
fn build_frame(
    state: &mut InteractiveState,
    app: &TuiApp,
    partial: Option<&latent_ai::AssistantMessage>,
) -> view::ViewportFrame {
    let budget = usize::from(app.screen_rows().max(1));
    // 流式内容(正文/thinking/工具实时输出)全量滚动:预览预算 = 屏高 −
    // 状态行/间隔/编辑器/footer 保留行数(超出仍由下方 while 循环收缩
    // 兜底);空闲帧保持 4 行兜底
    let live_streaming = !state.stream_text.is_empty()
        || state
            .pending_thinking
            .as_ref()
            .is_some_and(|t| !t.trim().is_empty())
        || state.pending_tool_output.is_some();
    let mut preview_cap = if live_streaming {
        budget.saturating_sub(view::LIVE_PREVIEW_RESERVED)
    } else {
        view::STREAM_PREVIEW_ROWS
    };
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
        .min(latent_tui::command_popup::MAX_VISIBLE_ROWS as u16) as usize
}

/// 启动区:横幅 + 分隔线 + 已加载资源分节([Skills]/[Subagents]/
/// [Extensions],ctrl+o 展开)。只进待提交缓冲;展开态变化时由 redraw
/// 路径按当前状态重组。
fn commit_startup(ctx: &InteractiveCtx<'_>, state: &mut InteractiveState) {
    state.commit_startup(view::welcome_lines(
        env!("CARGO_PKG_VERSION"),
        state.expanded,
        &state.theme,
        state.width,
    ));
    // 可加载资源分节(pi 语义):skill 与 subagent 定义是数据文件,与
    // 装配期(load_skill / subagent 工具)同一发现来源同一目录优先级;
    // 发现诊断已由装配期打 stderr,这里不重复
    if let Ok(cwd) = std::env::current_dir() {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let dir = latent_core::latent_dir(home.as_deref());
        state
            .resources
            .extend(discover_resource_sections(&cwd, dir.as_deref()));
    }
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
        state.commit_startup(vec![latent_tui::UiLine::raw("")]);
    }
    state.commit_startup(vec![
        latent_tui::header_view::separator(state.width, &state.theme),
        latent_tui::UiLine::raw(""),
    ]);
}

/// 启动区可加载资源分节(纯函数,可测):`[Skills]`/`[Subagents]`,条目
/// 只取名称(横向逗号排列由 resources 渲染折叠态完成)。空节由调用方
/// 跳过(`resources` 渲染同样过滤)。
fn discover_resource_sections(
    cwd: &std::path::Path,
    latent_dir: Option<&std::path::Path>,
) -> Vec<(String, Vec<String>)> {
    let mut sections = Vec::new();
    let (skills, _) = latent_core::discover_skill_defs(cwd, latent_dir);
    if !skills.is_empty() {
        sections.push((
            "Skills".into(),
            skills.iter().map(|s| s.name.clone()).collect(),
        ));
    }
    let (agents, _) = latent_core::discover_agent_defs(cwd, latent_dir);
    if !agents.is_empty() {
        sections.push((
            "Subagents".into(),
            agents.iter().map(|a| a.name.clone()).collect(),
        ));
    }
    sections
}

/// 资源分节行(按当前展开态)。
fn resources_lines(state: &InteractiveState) -> Vec<latent_tui::UiLine> {
    let sections: Vec<(&str, Vec<String>)> = state
        .resources
        .iter()
        .map(|(title, items)| (title.as_str(), items.clone()))
        .collect();
    latent_tui::header_view::resources(&sections, state.expanded, state.width, &state.theme)
}

/// ctrl+o 全文重绘的数据源:启动区(按当前展开态)+ 转录。
fn full_redraw_lines(state: &InteractiveState) -> Vec<latent_tui::UiLine> {
    let mut full = view::welcome_lines(
        env!("CARGO_PKG_VERSION"),
        state.expanded,
        &state.theme,
        state.width,
    );
    if !state.resources.is_empty() {
        full.extend(resources_lines(state));
        full.push(latent_tui::UiLine::raw(""));
    }
    full.push(latent_tui::header_view::separator(state.width, &state.theme));
    full.push(latent_tui::UiLine::raw(""));
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
