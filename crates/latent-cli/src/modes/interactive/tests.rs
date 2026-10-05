//! interactive 模式的装配级单测:状态机 + 事件处理(无终端 I/O;渲染断言
//! 走转录模型与待提交缓冲的 Line 文本)。

use std::sync::Arc;

use tokio::sync::mpsc;

use latent_ai::{ContentBlock, StopReason};
use latent_core::AgentSessionEvent;
use latent_tui::text::line_text;
use latent_tui::Key;

use crate::modes::slash;

use super::events::UiEvent;
use super::handlers::{handle_key, handle_ui_event, InteractiveCtx};
use super::replay::replay_history;
use super::state::{InteractiveState, SelectKind, Status};

fn test_state() -> InteractiveState {
    InteractiveState::new(latent_tui::Theme::dark_ansi(), 80)
}

async fn built_session_with_store(
    session_store: crate::assembly::SessionStore,
) -> crate::assembly::BuiltSession {
    let model = latent_ai::Model::minimal("m", "mock", "mock");
    crate::assembly::build_session(crate::assembly::BuildOptions {
        provider: Arc::new(latent_ai::ScriptedProvider::new(&model, vec![])),
        model,
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: vec![],
        spawn_hook: None,
        session_store,
        context_snapshot: None,
        active_tools: None,
        search_ignore: Default::default(),
        tool_result_max_chars: None,
        block_images: false,
        compaction: Default::default(),
        session_mode: None,
        default_session_mode: Default::default(),
        sandbox: Default::default(),
        approval: Default::default(),
        subagent_async_approval: Default::default(),
        approval_ui: None,
        rpc_approval: None,
    })
    .await
    .unwrap()
}

async fn built_memory_session() -> crate::assembly::BuiltSession {
    built_session_with_store(crate::assembly::SessionStore::Memory).await
}

fn ctx_of<'a>(
    built: &'a crate::assembly::BuiltSession,
    resolver: &'a std::sync::RwLock<latent_core::ModelResolver>,
    router: &'a crate::modes::interactive::handlers::SessionRouter,
) -> InteractiveCtx<'a> {
    InteractiveCtx {
        session: router,
        subagent_factory: None,
        manager_holder: Some(&built.manager_holder),
        resolver,
        compaction_config: &built.compaction_config,
        subagent_registry: None,
        user_composing: None,
        ui_tx: {
            let (tx, _rx) = mpsc::unbounded_channel();
            tx
        },
    }
}

fn session_event(event: latent_agent::AgentEvent) -> UiEvent {
    UiEvent::Session(AgentSessionEvent::Agent(event))
}

/// 提交缓冲的纯文本(断言用)。
fn committed_text(state: &InteractiveState) -> String {
    state
        .transcript
        .iter()
        .map(|item| {
            super::view::render_item(item, &state.theme, state.width, state.expanded)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- 输入提交与未知命令 ----

#[tokio::test]
async fn unknown_slash_input_is_local_warning_not_prompt() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    state.editor.set_text("/definitely-not-a-command");
    let quit = handle_key(&ctx, &mut state, Key::Enter).await;
    assert!(!quit);
    let ephemeral: String = state
        .pending
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        ephemeral.contains("Unknown command: /definitely-not-a-command"),
        "未知命令应本地警告: {ephemeral}"
    );
    assert_eq!(state.status, Status::Idle, "不应把未知命令发给模型");
}

#[tokio::test]
async fn submit_resets_status_to_thinking() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    state.editor.set_text("你好");
    handle_key(&ctx, &mut state, Key::Enter).await;
    assert_eq!(state.status, Status::Thinking);
    assert!(state.editor.is_empty(), "提交后编辑器应清空");
    // run 立即失败(ScriptedProvider 无 turn)也不影响状态断言
    built.session.wait_idle().await;
}

// ---- 双击 Ctrl+C 退出 ----

#[tokio::test]
async fn double_ctrl_c_exits_and_single_press_hints() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    let quit = handle_key(&ctx, &mut state, Key::Ctrl('c')).await;
    assert!(!quit);
    let ephemeral: String = state
        .pending
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        ephemeral.contains("press Ctrl+C again to exit"),
        "{ephemeral}"
    );

    let quit = handle_key(&ctx, &mut state, Key::Ctrl('c')).await;
    assert!(quit);

    // 超过窗口:重新计时
    let mut state = test_state();
    state.last_ctrl_c = Some(std::time::Instant::now() - std::time::Duration::from_millis(600));
    let quit = handle_key(&ctx, &mut state, Key::Ctrl('c')).await;
    assert!(!quit);
}

// ---- ctrl+o 全局展开 ----

#[tokio::test]
async fn ctrl_o_toggles_expansion_and_requests_full_redraw() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    assert!(!state.expanded);
    handle_key(&ctx, &mut state, Key::Ctrl('o')).await;
    assert!(state.expanded);
    assert!(state.needs_full_redraw, "切换后应请求全文重绘");
    state.needs_full_redraw = false;

    // 展开态下工具输出完整渲染
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::MessageEnd {
            message: Box::new(latent_agent::AgentMessage::tool_result_text(
                "call-1",
                "bash",
                "a\nb\nc\nd\ne",
                false,
            )),
        }),
    )
    .await;
    let rendered = committed_text(&state);
    assert!(rendered.contains("e"), "展开态输出应完整: {rendered}");

    handle_key(&ctx, &mut state, Key::Ctrl('o')).await;
    assert!(!state.expanded);
    let collapsed = super::view::render_transcript(&state.transcript, &state.theme, 80, false);
    let text = collapsed
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("+1 lines"), "收起态应有折叠提示: {text}");
}

// ---- thinking 持久化(定稿后可回看) ----

fn assistant_start() -> latent_agent::AgentEvent {
    let model = latent_ai::Model::minimal("m", "mock", "mock");
    latent_agent::AgentEvent::MessageStart {
        message: Box::new(latent_agent::AgentMessage::Assistant(Box::new(
            latent_ai::AssistantMessage::pending(&model),
        ))),
        partial: None,
    }
}

#[tokio::test]
async fn thinking_commits_into_transcript_when_text_starts() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    handle_ui_event(&ctx, &mut state, session_event(assistant_start())).await;
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::MessageDelta {
            delta: latent_agent::MessageDeltaPayload::Thinking {
                delta: "思考过程第一步".into(),
            },
        }),
    )
    .await;
    assert!(state.pending_thinking.as_deref() == Some("思考过程第一步"));
    assert_eq!(state.status, Status::Thinking);

    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::MessageDelta {
            delta: latent_agent::MessageDeltaPayload::Text {
                delta: "正文开始".into(),
            },
        }),
    )
    .await;
    // 首个文本 delta:thinking 块进转录,累积清空
    assert!(committed_text(&state).contains("思考过程第一步"));
    assert!(state.pending_thinking.is_none());
}

#[tokio::test]
async fn assistant_message_finalizes_as_markdown_item() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    handle_ui_event(&ctx, &mut state, session_event(assistant_start())).await;
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::MessageDelta {
            delta: latent_agent::MessageDeltaPayload::Text {
                delta: "# 标题\n\n正文".into(),
            },
        }),
    )
    .await;
    let model = latent_ai::Model::minimal("m", "mock", "mock");
    let mut assistant = latent_ai::AssistantMessage::pending(&model);
    assistant.stop_reason = StopReason::Stop;
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::MessageEnd {
            message: Box::new(latent_agent::AgentMessage::Assistant(Box::new(assistant))),
        }),
    )
    .await;

    let rendered = committed_text(&state);
    assert!(rendered.contains("标题"), "{rendered}");
    assert!(rendered.contains("正文"));
    // 定稿正文落盘(裸渲染,无外框)
    assert!(state
        .transcript
        .iter()
        .any(|item| matches!(item, super::state::TranscriptItem::Assistant { .. })));
}

#[tokio::test]
async fn assistant_message_with_tool_call_not_boxed() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    handle_ui_event(&ctx, &mut state, session_event(assistant_start())).await;
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::MessageDelta {
            delta: latent_agent::MessageDeltaPayload::Text {
                delta: "我来查看目录".into(),
            },
        }),
    )
    .await;
    // 定稿消息含工具调用(后续要执行命令)→ 正文不加框
    let model = latent_ai::Model::minimal("m", "mock", "mock");
    let mut assistant = latent_ai::AssistantMessage::pending(&model);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![latent_ai::ContentBlock::ToolCall {
        id: "call-1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({ "command": "ls" }),
    }];
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::MessageEnd {
            message: Box::new(latent_agent::AgentMessage::Assistant(Box::new(assistant))),
        }),
    )
    .await;

    assert!(state
        .transcript
        .iter()
        .any(|item| matches!(item, super::state::TranscriptItem::Assistant { .. })));
    let rendered = committed_text(&state);
    assert!(rendered.contains("我来查看目录"), "{rendered}");
}

// ---- 用量与错误可见性 ----

fn error_assistant(message: &str) -> latent_ai::AssistantMessage {
    let model = latent_ai::Model::minimal("m", "mock", "mock");
    let mut assistant = latent_ai::AssistantMessage::pending(&model);
    assistant.stop_reason = StopReason::Error;
    assistant.error_message = Some(message.to_string());
    assistant
}

#[tokio::test]
async fn turn_end_error_renders_without_usage_line() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    let mut assistant = error_assistant("No API key for provider: anthropic");
    assistant.usage.total_tokens = 0;

    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Session(AgentSessionEvent::Agent(latent_agent::AgentEvent::TurnEnd {
            message: Box::new(assistant),
            tool_results: vec![],
        })),
    )
    .await;

    let ephemeral: String = state
        .pending
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        ephemeral.contains("Error: No API key for provider: anthropic"),
        "错误信息应上屏: {ephemeral}"
    );
    assert!(
        !committed_text(&state).contains("[tokens]"),
        "错误回合不打用量行"
    );
}

#[tokio::test]
async fn turn_end_success_records_usage_and_context() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    let model = latent_ai::Model::minimal("m", "mock", "mock");
    let mut assistant = latent_ai::AssistantMessage::pending(&model);
    assistant.stop_reason = StopReason::Stop;
    assistant.usage = latent_ai::Usage {
        input: 100,
        output: 10,
        cache_read: 5,
        cache_write: 0,
        total_tokens: 110,
        ..Default::default()
    };
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Session(AgentSessionEvent::Agent(latent_agent::AgentEvent::TurnEnd {
            message: Box::new(assistant),
            tool_results: vec![],
        })),
    )
    .await;
    // ↑ = 完整 prompt(100 + cache_read 5),括号内缓存明细与命中率(5/105 ≈ 4%)
    assert!(committed_text(&state).contains("↑ 105 (U 100 / R 5 · 4%)"));
    assert_eq!(
        state.context_tokens, 115,
        "ctx 估计 = in+out+cacheRead+cacheWrite"
    );
    assert!(state.usage.summary().contains("110 tok"));
}

#[tokio::test]
async fn auto_retry_end_failure_renders_red() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Session(AgentSessionEvent::AutoRetryEnd {
            success: false,
            reason: "provider retry".into(),
        }),
    )
    .await;
    let ephemeral: String = state
        .pending
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        ephemeral.contains("Retry failed: provider retry"),
        "重试失败应上屏"
    );
}

// ---- 工具调用:标题按终态着色,输出折叠 ----

#[tokio::test]
async fn tool_result_renders_title_and_collapsed_output() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"command": "ls"}),
        }),
    )
    .await;
    assert_eq!(state.status, Status::Tool("bash".into()));
    // 执行中:标题尚未落盘(边框状态行指示)
    assert!(!committed_text(&state).contains("⏺ bash"));

    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            output: "ok".into(),
            is_error: false,
        }),
    )
    .await;
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::MessageEnd {
            message: Box::new(latent_agent::AgentMessage::tool_result_text(
                "call-1",
                "bash",
                "a.txt\nb.txt\nc.txt\nd.txt\ne.txt",
                false,
            )),
        }),
    )
    .await;

    let rendered = committed_text(&state);
    assert!(rendered.contains("⏺ bash"), "工具标题应落盘: {rendered}");
    assert!(rendered.contains("a.txt"));
    assert!(
        rendered.contains("+1 lines"),
        "5 行输出折叠为 4 行 + 提示: {rendered}"
    );
    assert!(state.pending_tools.is_empty());
}

#[tokio::test]
async fn parallel_results_pair_with_own_args() {
    // 并行批(03 文档 I4):start 事件全部先于结果消息发出,结果消息还可能
    // 按完成序到达 —— 每个结果必须按 tool_call_id 配对自己的标题(回归:
    // 单槽"最近一次 start"配对会把最后一个调用的 args 安到第一个结果上)
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    for (id, command) in [
        ("call-1", "ps aux | head -20"),
        ("call-2", "uname -a && whoami"),
    ] {
        handle_ui_event(
            &ctx,
            &mut state,
            session_event(latent_agent::AgentEvent::ToolExecutionStart {
                tool_call_id: id.into(),
                tool_name: "bash".into(),
                args: serde_json::json!({"command": command}),
            }),
        )
        .await;
    }
    // 结果按完成序到达:后发起的 call-2 先落定
    for (id, reason) in [
        ("call-2", "Plan 模式只允许只读命令:uname -a && whoami"),
        ("call-1", "Plan 模式只允许只读命令:ps aux | head -20"),
    ] {
        handle_ui_event(
            &ctx,
            &mut state,
            session_event(latent_agent::AgentEvent::MessageEnd {
                message: Box::new(latent_agent::AgentMessage::tool_result_text(
                    id, "bash", reason, true,
                )),
            }),
        )
        .await;
    }

    let rendered = committed_text(&state);
    // 两个标题都要带各自的 args(旧单槽逻辑下第二个结果标题为空)
    assert!(
        rendered.contains(r#"{"command":"ps aux | head -20"}"#),
        "call-1 标题应带自己的 args: {rendered}"
    );
    assert!(
        rendered.contains(r#"{"command":"uname -a && whoami"}"#),
        "call-2 标题应带自己的 args: {rendered}"
    );
    assert!(state.pending_tools.is_empty());
}

#[tokio::test]
async fn agent_settled_commits_dangling_tool_title() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    handle_ui_event(
        &ctx,
        &mut state,
        session_event(latent_agent::AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({}),
        }),
    )
    .await;
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Session(AgentSessionEvent::AgentSettled),
    )
    .await;
    assert_eq!(state.status, Status::Idle);
    assert!(
        committed_text(&state).contains("⏺ bash"),
        "悬空工具标题应兜底落盘"
    );
}

// ---- 并发选择请求排队 ----

#[tokio::test]
async fn concurrent_select_requests_queue_and_promote() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    let (tx1, _rx1) = tokio::sync::oneshot::channel();
    let (tx2, _rx2) = tokio::sync::oneshot::channel();
    let (tx3, _rx3) = tokio::sync::oneshot::channel();

    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Confirm {
            message: "q1".into(),
            responder: tx1,
        },
    )
    .await;
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Confirm {
            message: "q2".into(),
            responder: tx2,
        },
    )
    .await;
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Select {
            message: "q3".into(),
            options: vec![],
            responder: tx3,
        },
    )
    .await;

    assert_eq!(state.select.as_ref().unwrap().prompt, "q1", "队首先展示");
    assert_eq!(state.select_queue.len(), 2);

    let request = state.select.take().unwrap();
    match request.kind {
        SelectKind::Confirm(responder) => {
            let _ = responder.send(true);
        }
        _ => panic!("应为 confirm"),
    }
    state.promote_next_select();
    assert_eq!(state.select.as_ref().unwrap().prompt, "q2");
}

// ---- ! bash 透传 ----

#[tokio::test]
async fn bash_passthrough_runs_and_injects_context() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    state.editor.set_text("!echo passthrough-ok");
    handle_key(&ctx, &mut state, Key::Enter).await;
    assert!(matches!(state.status, Status::Bash(_)));

    // 模拟后台任务回流
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::BashDone {
            command: "echo passthrough-ok".into(),
            output: "passthrough-ok".into(),
            exit_code: None,
            is_error: false,
            inject: true,
        },
    )
    .await;

    assert_eq!(state.status, Status::Idle);
    let rendered = committed_text(&state);
    assert!(rendered.contains("! echo passthrough-ok"), "{rendered}");
    assert!(rendered.contains("passthrough-ok"));
    // 注入上下文:转录里出现 BashExecution 消息
    let messages = built.session.agent().messages();
    assert!(messages
        .iter()
        .any(|message| matches!(message, latent_agent::AgentMessage::BashExecution { .. })));
}

#[tokio::test]
async fn bash_bang_bang_skips_context_injection() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    state.editor.set_text("!!echo secret");
    handle_key(&ctx, &mut state, Key::Enter).await;
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::BashDone {
            command: "echo secret".into(),
            output: "secret".into(),
            exit_code: None,
            is_error: false,
            inject: false,
        },
    )
    .await;

    assert!(committed_text(&state).contains("secret"));
    let messages = built.session.agent().messages();
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, latent_agent::AgentMessage::BashExecution { .. })),
        "!! 不应注入上下文"
    );
}

// ---- 历史回放 ----

#[tokio::test]
async fn replay_renders_thinking_blocks() {
    let model = latent_ai::Model::minimal("m", "mock", "mock");
    let provider = Arc::new(latent_ai::ScriptedProvider::new(&model, vec![]));
    let built = crate::assembly::build_session(crate::assembly::BuildOptions {
        provider,
        model: model.clone(),
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: vec![],
        spawn_hook: None,
        session_store: crate::assembly::SessionStore::Memory,
        context_snapshot: None,
        active_tools: None,
        search_ignore: Default::default(),
        tool_result_max_chars: None,
        block_images: false,
        compaction: Default::default(),
        session_mode: None,
        default_session_mode: Default::default(),
        sandbox: Default::default(),
        approval: Default::default(),
        subagent_async_approval: Default::default(),
        approval_ui: None,
        rpc_approval: None,
    })
    .await
    .unwrap();

    let mut assistant = latent_ai::AssistantMessage::pending(&model);
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
        .set_messages(vec![latent_agent::AgentMessage::Assistant(Box::new(
            assistant,
        ))])
        .unwrap();

    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    replay_history(&ctx, &mut state);
    assert!(committed_text(&state).contains("回放中的思考"));
    assert!(committed_text(&state).contains("回放正文"));
}

#[tokio::test]
async fn replay_renders_user_assistant_toolcall_and_error() {
    let model = latent_ai::Model::minimal("m", "mock", "mock");
    let provider = Arc::new(latent_ai::ScriptedProvider::new(&model, vec![]));
    let built = crate::assembly::build_session(crate::assembly::BuildOptions {
        provider,
        model: model.clone(),
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: vec![],
        spawn_hook: None,
        session_store: crate::assembly::SessionStore::Memory,
        context_snapshot: None,
        active_tools: None,
        search_ignore: Default::default(),
        tool_result_max_chars: None,
        block_images: false,
        compaction: Default::default(),
        session_mode: None,
        default_session_mode: Default::default(),
        sandbox: Default::default(),
        approval: Default::default(),
        subagent_async_approval: Default::default(),
        approval_ui: None,
        rpc_approval: None,
    })
    .await
    .unwrap();

    let mut assistant = latent_ai::AssistantMessage::pending(&model);
    assistant.content = vec![ContentBlock::ToolCall {
        id: "call-1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command": "ls"}),
    }];
    assistant.stop_reason = StopReason::ToolUse;
    let mut failed = latent_ai::AssistantMessage::pending(&model);
    failed.stop_reason = StopReason::Error;
    failed.error_message = Some("boom".into());
    built
        .session
        .agent()
        .set_messages(vec![
            latent_agent::AgentMessage::user("跑一下"),
            latent_agent::AgentMessage::Assistant(Box::new(assistant)),
            latent_agent::AgentMessage::tool_result_text("call-1", "bash", "a.txt\nb.txt", false),
            latent_agent::AgentMessage::Assistant(Box::new(failed)),
        ])
        .unwrap();

    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    replay_history(&ctx, &mut state);

    let rendered = committed_text(&state);
    assert!(rendered.contains("跑一下"), "user 消息应回放: {rendered}");
    assert!(rendered.contains("⏺ bash"), "工具调用应回放");
    assert!(rendered.contains("a.txt"), "工具结果应回放");
    assert!(rendered.contains("Error: boom"), "历史错误消息应回放");
}

// ---- 斜杠命令 ----

#[tokio::test]
async fn execute_help_and_session_commit_lines() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    let quit = super::handlers::execute_command(&ctx, &mut state, slash::SlashAction::Help).await;
    assert!(!quit);
    assert!(committed_text(&state).contains("/help"));

    super::handlers::execute_command(
        &ctx,
        &mut state,
        slash::SlashAction::Session {
            arg: Some("info".into()),
        },
    )
    .await;
    let rendered = committed_text(&state);
    assert!(rendered.contains("会话信息"), "{rendered}");
    assert!(rendered.contains("model:"), "{rendered}");
    assert!(rendered.contains("usage:"), "{rendered}");
}

#[tokio::test]
async fn execute_thinking_with_arg_updates_footer() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    super::handlers::execute_command(
        &ctx,
        &mut state,
        slash::SlashAction::Thinking {
            arg: Some("high".into()),
        },
    )
    .await;
    assert_eq!(state.thinking_label, "high");

    super::handlers::execute_command(
        &ctx,
        &mut state,
        slash::SlashAction::Thinking {
            arg: Some("off".into()),
        },
    )
    .await;
    assert_eq!(state.thinking_label, "off");

    // 非法级别:本地红字,状态不变
    super::handlers::execute_command(
        &ctx,
        &mut state,
        slash::SlashAction::Thinking {
            arg: Some("ultra".into()),
        },
    )
    .await;
    assert_eq!(state.thinking_label, "off");
}

#[tokio::test]
async fn execute_model_with_arg_updates_footer() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    super::handlers::execute_command(
        &ctx,
        &mut state,
        slash::SlashAction::Model {
            arg: Some("deepseek/deepseek-chat".into()),
        },
    )
    .await;
    assert_eq!(state.model_label, "deepseek/deepseek-chat");

    // 未知 provider:本地红字,状态不变
    super::handlers::execute_command(
        &ctx,
        &mut state,
        slash::SlashAction::Model {
            arg: Some("nope/model".into()),
        },
    )
    .await;
    assert_eq!(state.model_label, "deepseek/deepseek-chat");
}

#[tokio::test]
async fn execute_model_without_arg_opens_selector_at_current_model() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    super::handlers::execute_command(
        &ctx,
        &mut state,
        slash::SlashAction::Model {
            arg: Some("deepseek/deepseek-chat".into()),
        },
    )
    .await;

    super::handlers::execute_command(&ctx, &mut state, slash::SlashAction::Model { arg: None })
        .await;
    let select = state.select.as_ref().expect("应打开模型选择器");
    assert!(
        !select.list.options.is_empty(),
        "内置 provider 默认模型应入列"
    );
    assert_eq!(
        select
            .list
            .options
            .get(select.list.selected)
            .map(String::as_str),
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
                ctx.session.current().set_model(model.clone()).await;
                super::handlers::refresh_footer(&ctx, &mut state);
            }
        }
        _ => panic!("应为 model 选择器"),
    }
    assert_ne!(
        state.model_label, "deepseek/deepseek-chat",
        "选择后 footer 应刷新"
    );
}

// ---- /model 配置入口(添加模型表单 / $EDITOR 条目) ----

/// 在真实按键路径下选中 /model 选择器的配置条目并回车
/// (`from_end`:true = 末位「编辑 models.json」,false = 次末位「添加模型」)。
async fn enter_model_config_entry(
    ctx: &InteractiveCtx<'_>,
    state: &mut InteractiveState,
    from_end: bool,
) {
    super::handlers::execute_command(ctx, state, slash::SlashAction::Model { arg: None }).await;
    let mut request = state.select.take().unwrap();
    request.list.selected = if from_end {
        request.list.options.len() - 1
    } else {
        request.list.options.len() - 2
    };
    state.select = Some(request);
    handle_key(ctx, state, Key::Enter).await;
}

#[tokio::test]
async fn model_selector_lists_config_entries() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    super::handlers::execute_command(&ctx, &mut state, slash::SlashAction::Model { arg: None })
        .await;
    let select = state.select.as_ref().expect("应打开模型选择器");
    assert!(select.list.options.len() >= 2, "至少含两个配置条目");
    let last = select.list.options.len() - 1;
    assert_eq!(
        select.list.options[last],
        super::handlers::MODEL_EDITOR_ENTRY
    );
    assert_eq!(
        select.list.options[last - 1],
        super::handlers::MODEL_FORM_ENTRY
    );
}

#[tokio::test]
async fn model_selector_editor_entry_requests_suspend() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    enter_model_config_entry(&ctx, &mut state, true).await;
    assert!(
        matches!(
            state.suspend_action,
            Some(super::state::SuspendAction::EditModelsJson)
        ),
        "编辑条目应置挂起动作标记"
    );
    assert!(state.select.is_none());
}

#[tokio::test]
async fn model_form_known_provider_skips_provider_steps() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    enter_model_config_entry(&ctx, &mut state, false).await;
    assert!(state.model_form.is_some(), "回车配置条目应打开表单");
    assert!(
        matches!(
            state.select.as_ref().unwrap().kind,
            SelectKind::ModelTargetChoice
        ),
        "第一步应弹出写入位置选择"
    );
    handle_key(&ctx, &mut state, Key::Enter).await; // 选中「当前目录」
    assert_eq!(
        state.model_form.as_ref().unwrap().target,
        Some(super::state::ModelFormTarget::Project)
    );

    // 已知 provider(zai 内置)→ 跳过 api/baseUrl/apiKey,直达模型 id
    state.editor.set_text("zai");
    handle_key(&ctx, &mut state, Key::Enter).await;
    let form = state.model_form.as_ref().unwrap();
    assert!(form.known_provider);
    assert_eq!(form.step, super::state::ModelFormStep::ModelId);

    // Esc 取消整张表单
    handle_key(&ctx, &mut state, Key::Esc).await;
    assert!(state.model_form.is_none());
}

#[tokio::test]
async fn model_form_adds_new_provider_and_switches() {
    let dir = std::env::temp_dir().join(format!(
        "latent_model_form_{}_{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    state.cwd = dir.clone();
    state.home = None;

    enter_model_config_entry(&ctx, &mut state, false).await;
    handle_key(&ctx, &mut state, Key::Enter).await; // 写入位置:当前目录
    assert_eq!(
        state.model_form.as_ref().unwrap().step,
        super::state::ModelFormStep::Provider
    );

    // 新 provider:provider → api(选择列表) → baseUrl(留空跳过) → apiKey → model
    state.editor.set_text("tprov");
    handle_key(&ctx, &mut state, Key::Enter).await;
    let form = state.model_form.as_ref().unwrap();
    assert_eq!(
        form.step,
        super::state::ModelFormStep::Api,
        "未知 provider 应进入 api 选择"
    );
    assert!(state.select.is_some(), "api 协议应弹出选择列表");
    handle_key(&ctx, &mut state, Key::Enter).await; // 选中 openai-completions
    assert_eq!(
        state.model_form.as_ref().unwrap().api.as_deref(),
        Some("openai-completions")
    );
    handle_key(&ctx, &mut state, Key::Enter).await; // baseUrl 留空跳过
    assert_eq!(
        state.model_form.as_ref().unwrap().step,
        super::state::ModelFormStep::ApiKeyEnv
    );
    state.editor.set_text("TPROV_API_KEY");
    handle_key(&ctx, &mut state, Key::Enter).await;
    assert_eq!(
        state.model_form.as_ref().unwrap().step,
        super::state::ModelFormStep::ModelId
    );
    state.editor.set_text("test-model");
    handle_key(&ctx, &mut state, Key::Enter).await;

    // 落盘 + 热重载 + 自动切换
    assert!(state.model_form.is_none());
    assert_eq!(state.model_label, "tprov/test-model");
    let file = dir.join(".latent/models.json");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("tprov") && text.contains("test-model"),
        "{text}"
    );
    let model = resolver
        .read()
        .unwrap()
        .resolve("tprov/test-model")
        .unwrap();
    assert_eq!(model.api, "openai-completions");
    // env 未设置 → apiKey 字面值兜底(与配置体系语义一致)
    assert_eq!(model.api_key.as_deref(), Some("TPROV_API_KEY"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn compact_done_resets_context_estimate_and_reports() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    state.context_tokens = 9000;
    state.status = Status::Compacting;

    handle_ui_event(&ctx, &mut state, UiEvent::CompactDone(Ok(12))).await;
    assert_eq!(state.context_tokens, 0, "压缩后 ctx 估计应重置");
    assert_eq!(state.status, Status::Idle);
    assert!(committed_text(&state).contains("compacted → 12"));

    handle_ui_event(&ctx, &mut state, UiEvent::CompactDone(Err("boom".into()))).await;
    let ephemeral: String = state
        .pending
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(ephemeral.contains("compact failed: boom"), "{ephemeral}");
}

// ---- 视口布局收缩 ----

#[tokio::test]
async fn viewport_shrinks_preview_under_budget() {
    let mut state = test_state();
    state.stream_text = (1..=20)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    // 预算 8 行:预览收缩后 帧高 ≤ 固定行(空隙 1 + 状态 1 + 编辑区 3
    // + footer 3)+ 预览 2
    let frame = super::view::viewport(&state, None, 2, 1, 8);
    assert!(frame.height <= 10, "帧高应受预算约束: {}", frame.height);
}

// 全帧差分渲染后预览上限恒为常量(view.rs STREAM_PREVIEW_ROWS,行为覆盖
// 见 view.rs::tail_height_follows_content),不再有 busy/idle 双轨与预留守恒。

// ---- 斜杠补全弹窗(Codex 交互) ----

/// 弹窗状态机测试的共享前奏:内存会话 + 已输入 `/m` 的状态。
async fn popup_state_with_input(input: &str) -> (crate::assembly::BuiltSession, InteractiveState) {
    let built = built_memory_session().await;
    let mut state = test_state();
    for c in input.chars() {
        let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
        let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
        handle_key(&ctx_of(&built, &resolver, &router), &mut state, Key::Char(c)).await;
    }
    (built, state)
}

#[tokio::test]
async fn typing_slash_opens_filtered_popup() {
    let (_built, state) = popup_state_with_input("/m").await;
    assert!(state.slash_popup.visible(), "输入 /m 应弹出补全");
    // 前缀(/mode、/model)优先,模糊子序列(compact/theme 含 m)次之
    assert_eq!(state.slash_popup.match_count(), 4);
    assert_eq!(
        state.slash_popup.selected_entry().map(|e| e.name.as_str()),
        Some("mode"),
        "前缀匹配应排首位(COMMANDS 表序)"
    );

    // 继续输入到无匹配:弹窗退场
    let (built, mut state) = popup_state_with_input("/m").await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    for c in "zz".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    assert!(!state.slash_popup.visible());
}

#[tokio::test]
async fn slash_quit_via_enter_returns_quit_signal() {
    // 回归:submit_input 曾丢弃 execute_command 的退出信号,/quit 静默失效
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    for c in "/quit".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    let quit = handle_key(&ctx, &mut state, Key::Enter).await;
    assert!(quit, "/quit 提交后应向事件循环返回退出信号");
}

#[tokio::test]
async fn enter_executes_partial_slash_directly() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    for c in "/think".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    // Enter(非完全匹配):补全为完整命令并立即执行,一次回车直达
    handle_key(&ctx, &mut state, Key::Enter).await;
    assert!(state.select.is_some(), "Enter 应直接执行 /thinking 打开选择器");
    assert!(state.editor.text().is_empty(), "执行后编辑器应清空");
}

#[tokio::test]
async fn exact_slash_input_executes_directly_on_enter() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    for c in "/model".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    assert!(state.slash_popup.is_exact_match());
    handle_key(&ctx, &mut state, Key::Enter).await;
    assert!(state.select.is_some(), "完全匹配时 Enter 直接执行");
}

#[tokio::test]
async fn tab_completes_and_esc_dismisses_popup() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();
    for c in "/h".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    handle_key(&ctx, &mut state, Key::Tab).await;
    assert_eq!(state.editor.text(), "/help ", "Tab 应补全");

    // Esc 关闭弹窗;查询继续变化时重新打开
    let mut state = test_state();
    for c in "/h".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    handle_key(&ctx, &mut state, Key::Esc).await;
    assert!(!state.slash_popup.visible());
    handle_key(&ctx, &mut state, Key::Char('e')).await;
    assert!(state.slash_popup.visible(), "查询变化后重新打开");
}

#[tokio::test]
async fn arrows_navigate_popup_while_visible() {
    let (_built, mut state) = popup_state_with_input("/").await;
    assert!(state.slash_popup.visible(), "裸 / 应列出全部命令");
    let first = state.slash_popup.selected_entry().map(|e| e.name.clone());
    state.slash_popup.move_down();
    let second = state.slash_popup.selected_entry().map(|e| e.name.clone());
    assert_ne!(first, second);
    state.slash_popup.move_up();
    assert_eq!(
        state.slash_popup.selected_entry().map(|e| e.name.clone()),
        first
    );
}

// ---- `@` 文件弹窗(pi @ autocomplete) ----

/// @ 弹窗测试夹具:临时工作区(README.md / src/main.rs / node_modules/x.js)。
fn mention_fixture_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("latent_mention_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("node_modules")).unwrap();
    std::fs::write(dir.join("README.md"), "hello").unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.join("node_modules/x.js"), "noise").unwrap();
    dir
}

/// 共享前奏:内存会话 + 已注入 cwd 的状态(cwd 须在按键前注入,
/// 首次激活时按需采集候选)。mention_ignore 注入 `.latentignore` 规则
/// (生产中由装配层 load_search_ignore 以 cwd 为匹配根提供)。
async fn mention_state_at(dir: &std::path::Path) -> (crate::assembly::BuiltSession, InteractiveState) {
    let built = built_memory_session().await;
    let mut state = test_state();
    state.cwd = dir.to_path_buf();
    state.mention_ignore = std::sync::Arc::new(latent_tools::SearchIgnore::from_patterns(
        dir,
        ["node_modules/"],
        |_, error| panic!("{error}"),
    ));
    (built, state)
}

#[tokio::test]
async fn typing_at_opens_file_popup_filtered_by_latentignore() {
    let dir = mention_fixture_dir("open");
    let (built, mut state) = mention_state_at(&dir).await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    for c in "@rea".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    assert!(state.mention_popup.visible(), "输入 @rea 应弹出文件弹窗");
    assert_eq!(
        state.mention_popup.selected_entry().map(|e| e.path.as_str()),
        Some("README.md")
    );

    // 裸 @ 列出全部候选:node_modules 被 .latentignore 规则剪枝,src 目录在列
    let (built, mut state) = mention_state_at(&dir).await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    handle_key(&ctx, &mut state, Key::Char('@')).await;
    assert!(state.mention_popup.visible());
    let paths: Vec<String> = (0..state.mention_popup.match_count())
        .map(|i| state.mention_popup.matches()[i].path.clone())
        .collect();
    assert!(!paths.iter().any(|p| p.contains("node_modules")), "{paths:?}");
    assert!(paths.contains(&"src".to_string()), "{paths:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn enter_completes_file_with_trailing_space_and_popup_exits() {
    let dir = mention_fixture_dir("file");
    let (built, mut state) = mention_state_at(&dir).await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    for c in "@rea".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    handle_key(&ctx, &mut state, Key::Enter).await;
    // 选中补全 = 绝对全路径 + 尾随空格(手输文本才原样发出)
    assert_eq!(state.editor.text(), format!("@{}/README.md ", dir.display()));
    assert!(
        !state.mention_popup.visible(),
        "文件补全后 token 以空格终结,弹窗退场"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn enter_on_directory_keeps_popup_for_descent() {
    let dir = mention_fixture_dir("dir");
    let (built, mut state) = mention_state_at(&dir).await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    for c in "@sr".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    handle_key(&ctx, &mut state, Key::Enter).await;
    // 目录补全 = 绝对全路径 + / 尾缀、无空格(下钻 token 也是绝对形式)
    assert_eq!(state.editor.text(), format!("@{}/src/", dir.display()));
    assert!(state.mention_popup.visible(), "目录补全后弹窗保持下钻");
    assert_eq!(
        state.mention_popup.selected_entry().map(|e| e.path.as_str()),
        Some("src/main.rs")
    );
    // 绝对 token 剥离 base 后继续下钻到文件,补全仍是全路径
    handle_key(&ctx, &mut state, Key::Enter).await;
    assert_eq!(
        state.editor.text(),
        format!("@{}/src/main.rs ", dir.display())
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn bang_prefix_suppresses_mention_popup() {
    let dir = mention_fixture_dir("bang");
    let (built, mut state) = mention_state_at(&dir).await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    for c in "!echo @".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    assert!(
        !state.mention_popup.visible(),
        "! 透传无提及语义,弹窗让位"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn esc_dismisses_mention_popup_until_query_changes() {
    let dir = mention_fixture_dir("esc");
    let (built, mut state) = mention_state_at(&dir).await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    for c in "@rea".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    handle_key(&ctx, &mut state, Key::Esc).await;
    assert!(!state.mention_popup.visible());
    handle_key(&ctx, &mut state, Key::Char('d')).await;
    assert!(state.mention_popup.visible(), "查询变化后重新打开(@read 仍前缀命中)");
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 审批 overlay(13 文档 §10.3)
// ---------------------------------------------------------------------------

fn approval_request() -> latent_core::ApprovalRequest {
    latent_core::ApprovalRequest {
        tool_call_id: "t1".into(),
        tool_name: "bash".into(),
        args: serde_json::json!({"command": "make test"}),
        risk: latent_core::ToolRiskClass::Shell,
        reason: latent_core::ApprovalReason::ShellCommand,
        detail: "make test".into(),
    }
}

#[tokio::test]
async fn approval_overlay_digit_keys_resolve_decisions() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    // 审批请求入队 → overlay 渲染(四选项)
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Approval {
            request: approval_request(),
            responder: tx,
        },
    )
    .await;
    assert!(state.select.is_some(), "审批请求应打开 overlay");
    assert!(state
        .select
        .as_ref()
        .unwrap()
        .prompt
        .contains("make test"), "overlay 应展示命令详情");

    // 数字键 1 = 批准一次
    handle_key(&ctx, &mut state, latent_tui::Key::Char('1')).await;
    assert_eq!(rx.await.unwrap(), latent_core::ApprovalDecision::Approve);
    assert!(state.select.is_none(), "决策后 overlay 关闭");
}

#[tokio::test]
async fn approval_overlay_esc_denies_and_ctrl_c_aborts() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    let (tx, rx) = tokio::sync::oneshot::channel();
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Approval {
            request: approval_request(),
            responder: tx,
        },
    )
    .await;
    handle_key(&ctx, &mut state, latent_tui::Key::Esc).await;
    assert_eq!(rx.await.unwrap(), latent_core::ApprovalDecision::Deny);

    // Ctrl+C = 中止本次任务
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Approval {
            request: approval_request(),
            responder: tx,
        },
    )
    .await;
    handle_key(&ctx, &mut state, latent_tui::Key::Ctrl('c')).await;
    assert_eq!(rx.await.unwrap(), latent_core::ApprovalDecision::Abort);
}

#[tokio::test]
async fn slash_mode_switches_session_mode() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    for c in "/mode confirm".chars() {
        handle_key(&ctx, &mut state, latent_tui::Key::Char(c)).await;
    }
    handle_key(&ctx, &mut state, latent_tui::Key::Enter).await;
    assert_eq!(built.session.mode(), latent_core::SessionMode::Confirm);
    // 不打转录提示(footer 状态栏已显示模式标记)
    assert_eq!(state.mode_label, "confirm");

    // Shift+Tab 循环:confirm → full-access
    handle_key(&ctx, &mut state, latent_tui::Key::BackTab).await;
    assert_eq!(built.session.mode(), latent_core::SessionMode::FullAccess);
    handle_key(&ctx, &mut state, latent_tui::Key::BackTab).await;
    assert_eq!(built.session.mode(), latent_core::SessionMode::Plan);
}

#[test]
fn discover_resource_sections_lists_skills_and_subagents() {
    // 临时目录造定义文件:skill(description 必填,name 缺省目录名)与
    // agent(name/description 可选)
    let root =
        std::env::temp_dir().join(format!("latent_res_test_{}_{}", std::process::id(), line!()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".latent/skills/review")).unwrap();
    std::fs::write(
        root.join(".latent/skills/review/SKILL.md"),
        "---\ndescription: 代码审查流程\n---\n正文",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(".latent/agents")).unwrap();
    std::fs::write(
        root.join(".latent/agents/scout.md"),
        "---\nname: scout\ndescription: 侦察代理\n---\n你是侦察者",
    )
    .unwrap();

    let sections = super::discover_resource_sections(&root, None);
    // 条目只取名称,横向排列由渲染折叠态完成
    assert_eq!(
        sections,
        vec![
            ("Skills".into(), vec!["review".to_string()]),
            ("Subagents".into(), vec!["scout".to_string()]),
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn discover_resource_sections_empty_without_defs() {
    let root = std::env::temp_dir().join(format!("latent_res_none_{}_{}", std::process::id(), line!()));
    assert!(super::discover_resource_sections(&root, None).is_empty());
}

// ---- /session 历史会话切换 ----

#[tokio::test]
async fn switch_resume_session_restores_messages_and_reuses_file() {
    let dir = std::env::temp_dir().join(format!(
        "latent_switch_resume_{}_{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("s.jsonl");

    // 历史会话:两条 user 消息
    {
        let manager = latent_session::create_session_with(Some(&file), "/tmp/proj", None).unwrap();
        manager
            .append_message(latent_agent::AgentMessage::user("历史问题一"))
            .unwrap();
        manager
            .append_message(latent_agent::AgentMessage::user("历史问题二"))
            .unwrap();
    }

    let built = built_memory_session().await;
    let session = built.session.clone();
    let path = crate::assembly::switch_resume_session(
        &session,
        &built.manager_holder,
        &file,
    )
    .await
    .unwrap();
    assert_eq!(path.as_deref(), Some(file.as_path()), "应返回所切文件路径");

    // seed 消息恢复进 agent 转录(set_mode 会追加 ModeSection,只断言 user 消息)
    let messages = session.agent().messages();
    let user_texts: Vec<&str> = messages
        .iter()
        .filter_map(|message| match message {
            latent_agent::AgentMessage::User { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(user_texts, vec!["历史问题一", "历史问题二"]);

    // 切换后续聊 append 落回同一文件(append-only,不新建)
    let holder_manager = built.manager_holder.get().unwrap();
    holder_manager
        .append_message(latent_agent::AgentMessage::user("新消息"))
        .unwrap();
    let reloaded = latent_session::create_session_with(Some(&file), "/tmp/proj", None).unwrap();
    assert!(matches!(
        reloaded.entries().last().unwrap(),
        latent_session::Entry::Message {
            message: latent_agent::AgentMessage::User { content, .. },
            ..
        } if content == "新消息"
    ));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 带变体命令的弹窗交互(统一规范:裸命令无实际功能,回车后选择) ----

#[tokio::test]
async fn variant_command_enter_completes_then_second_enter_executes() {
    let dir = std::env::temp_dir().join(format!(
        "latent_variant_enter_{}_{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let built = built_session_with_store(crate::assembly::SessionStore::New { dir: dir.clone() }).await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    for c in "/sess".chars() {
        handle_key(&ctx, &mut state, latent_tui::Key::Char(c)).await;
    }
    assert!(state.slash_popup.visible(), "输入 /sess 应弹出补全");

    // 部分输入 /sess → Enter:补全命令名并展开变体选择页(不执行)
    handle_key(&ctx, &mut state, latent_tui::Key::Enter).await;
    assert_eq!(state.editor.text(), "/session", "应补全到命令名");
    assert!(state.select.is_none(), "此步不应执行命令");
    assert!(
        state.slash_popup.visible() && state.slash_popup.match_count() == 2,
        "应进入变体选择页(list/info): {}",
        state.slash_popup.match_count()
    );

    // ↓ 选中 info 变体 → Enter:直接执行(选中行带参数 → 提交执行;
    // info 只打转录行,select 仍为 None)
    state.slash_popup.move_down();
    handle_key(&ctx, &mut state, latent_tui::Key::Enter).await;
    assert!(
        committed_text(&state).contains("id:"),
        "↓+Enter 应执行选中的 /session info"
    );
    assert!(state.select.is_none(), "info 不打开选择器");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn bare_variant_commands_show_usage_only() {
    let built = built_memory_session().await;
    let resolver = std::sync::RwLock::new(latent_core::create_model_resolver());
    let router = crate::modes::interactive::handlers::SessionRouter::new(built.session.clone());
    let ctx = ctx_of(&built, &resolver, &router);
    let mut state = test_state();

    // 裸 /session、/mode:只提示用法,无任何实际动作(/subagent 无参打开
    // 选择器,属 /model 类,不受此规范约束)
    for action in [
        slash::SlashAction::Session { arg: None },
        slash::SlashAction::Mode { arg: None },
    ] {
        super::handlers::execute_command(&ctx, &mut state, action).await;
    }
    let rendered: String = state
        .pending
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("用法: /session list"), "{rendered}");
    assert!(rendered.contains("用法: /mode <"), "{rendered}");
    assert!(state.select.is_none(), "裸命令不应打开任何选择器");
}

/// 异步 subagent 卡片:运行中保持 pending,结算翻终态(成功绿/失败红)
/// 并解绑 + 触发全文重绘。
#[test]
fn flush_settled_subagent_cards_flips_status() {
    use latent_core::{ChildOutcome, RunStatus, SubagentRegistry};
    use std::sync::{Mutex, Weak};
    use tokio_util::sync::CancellationToken;

    let registry = Arc::new(SubagentRegistry::new(Arc::new(Mutex::new(Weak::new()))));
    let outcome = |status: RunStatus| ChildOutcome {
        status,
        output: Some("done".into()),
        error: None,
        duration: std::time::Duration::from_millis(10),
        tool_calls: 0,
        usage: latent_ai::Usage::zero(),
        cancelled_by_parent: false,
        model_id: "m".into(),
    };
    let mut state = test_state();
    let card = |status| super::state::TranscriptItem::ToolCall {
        name: "subagent".into(),
        args: r#"{"task":"t"}"#.into(),
        status,
    };

    let run_id = registry.register("reviewer", "task", true, CancellationToken::new());
    state.commit(card(super::state::ToolStatus::Pending));
    state.subagent_run_cards.push((run_id.clone(), 0));

    // 运行中:保持 pending,不触发重绘
    super::handlers::flush_settled_subagent_cards(Some(&registry), &mut state);
    assert!(matches!(
        state.transcript[0],
        super::state::TranscriptItem::ToolCall {
            status: super::state::ToolStatus::Pending,
            ..
        }
    ));
    assert!(!state.needs_full_redraw);

    // 结算(成功):翻绿 + 全文重绘 + 解绑
    registry.finish(&run_id, &outcome(RunStatus::Completed));
    super::handlers::flush_settled_subagent_cards(Some(&registry), &mut state);
    assert!(matches!(
        state.transcript[0],
        super::state::TranscriptItem::ToolCall {
            status: super::state::ToolStatus::Success,
            ..
        }
    ));
    assert!(state.needs_full_redraw);
    assert!(state.subagent_run_cards.is_empty());

    // 失败:翻红
    let run2 = registry.register("reviewer", "task2", true, CancellationToken::new());
    state.needs_full_redraw = false;
    state.commit(card(super::state::ToolStatus::Pending));
    state.subagent_run_cards.push((run2.clone(), 1));
    registry.finish(&run2, &outcome(RunStatus::Failed));
    super::handlers::flush_settled_subagent_cards(Some(&registry), &mut state);
    assert!(matches!(
        state.transcript[1],
        super::state::TranscriptItem::ToolCall {
            status: super::state::ToolStatus::Error,
            ..
        }
    ));
}

/// 打字门控回写:编辑器非空 = true(延迟唤醒),清空 = false。
#[test]
fn sync_wake_gate_follows_editor_buffer() {
    use std::sync::atomic::AtomicBool;
    let flag = Arc::new(AtomicBool::new(false));
    let mut state = test_state();
    super::handlers::sync_wake_gate(Some(&flag), &state);
    assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
    state.editor.set_text("还在输入");
    super::handlers::sync_wake_gate(Some(&flag), &state);
    assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
}
