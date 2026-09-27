//! interactive 模式的装配级单测:状态机 + 事件处理(无终端 I/O;渲染断言
//! 走转录模型与待提交缓冲的 Line 文本)。

use std::sync::Arc;

use tokio::sync::mpsc;

use rpi_ai::{ContentBlock, StopReason};
use rpi_core::AgentSessionEvent;
use rpi_tui::text::line_text;
use rpi_tui::Key;

use crate::modes::slash;

use super::events::UiEvent;
use super::handlers::{handle_key, handle_ui_event, InteractiveCtx};
use super::replay::replay_history;
use super::state::{InteractiveState, SelectKind, Status};

fn test_state() -> InteractiveState {
    InteractiveState::new(rpi_tui::Theme::dark_ansi(), 80)
}

async fn built_memory_session() -> crate::assembly::BuiltSession {
    let model = rpi_ai::Model::minimal("m", "mock", "mock");
    crate::assembly::build_session(crate::assembly::BuildOptions {
        provider: Arc::new(rpi_ai::ScriptedProvider::new(&model, vec![])),
        model,
        ui: Arc::new(rpi_core::NoopUi),
        extension_specs: vec![],
        spawn_hook: None,
        session_store: crate::assembly::SessionStore::Memory,
        context_snapshot: None,
        active_tools: None,
    })
    .await
    .unwrap()
}

fn ctx_of<'a>(
    built: &'a crate::assembly::BuiltSession,
    resolver: &'a rpi_core::ModelResolver,
) -> InteractiveCtx<'a> {
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

fn session_event(event: rpi_agent::AgentEvent) -> UiEvent {
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
        session_event(rpi_agent::AgentEvent::MessageEnd {
            message: Box::new(rpi_agent::AgentMessage::tool_result_text(
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
async fn thinking_commits_into_transcript_when_text_starts() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    let mut state = test_state();

    handle_ui_event(&ctx, &mut state, session_event(assistant_start())).await;
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(rpi_agent::AgentEvent::MessageDelta {
            delta: rpi_agent::MessageDeltaPayload::Thinking {
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
        session_event(rpi_agent::AgentEvent::MessageDelta {
            delta: rpi_agent::MessageDeltaPayload::Text {
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    let mut state = test_state();

    handle_ui_event(&ctx, &mut state, session_event(assistant_start())).await;
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(rpi_agent::AgentEvent::MessageDelta {
            delta: rpi_agent::MessageDeltaPayload::Text {
                delta: "# 标题\n\n正文".into(),
            },
        }),
    )
    .await;
    let model = rpi_ai::Model::minimal("m", "mock", "mock");
    let mut assistant = rpi_ai::AssistantMessage::pending(&model);
    assistant.stop_reason = StopReason::Stop;
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(rpi_agent::AgentEvent::MessageEnd {
            message: Box::new(rpi_agent::AgentMessage::Assistant(Box::new(assistant))),
        }),
    )
    .await;

    let rendered = committed_text(&state);
    assert!(rendered.contains("标题"), "{rendered}");
    assert!(rendered.contains("正文"));
    // 纯文本回复:包 AI 输出框
    assert!(state.transcript.iter().any(
        |item| matches!(item, super::state::TranscriptItem::Assistant { boxed: true, .. })
    ));
}

#[tokio::test]
async fn assistant_message_with_tool_call_not_boxed() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    let mut state = test_state();

    handle_ui_event(&ctx, &mut state, session_event(assistant_start())).await;
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(rpi_agent::AgentEvent::MessageDelta {
            delta: rpi_agent::MessageDeltaPayload::Text {
                delta: "我来查看目录".into(),
            },
        }),
    )
    .await;
    // 定稿消息含工具调用(后续要执行命令)→ 正文不加框
    let model = rpi_ai::Model::minimal("m", "mock", "mock");
    let mut assistant = rpi_ai::AssistantMessage::pending(&model);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![rpi_ai::ContentBlock::ToolCall {
        id: "call-1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({ "command": "ls" }),
    }];
    handle_ui_event(
        &ctx,
        &mut state,
        session_event(rpi_agent::AgentEvent::MessageEnd {
            message: Box::new(rpi_agent::AgentMessage::Assistant(Box::new(assistant))),
        }),
    )
    .await;

    assert!(state.transcript.iter().any(
        |item| matches!(item, super::state::TranscriptItem::Assistant { boxed: false, .. })
    ));
    let rendered = committed_text(&state);
    assert!(rendered.contains("我来查看目录"), "{rendered}");
}

// ---- 用量与错误可见性 ----

fn error_assistant(message: &str) -> rpi_ai::AssistantMessage {
    let model = rpi_ai::Model::minimal("m", "mock", "mock");
    let mut assistant = rpi_ai::AssistantMessage::pending(&model);
    assistant.stop_reason = StopReason::Error;
    assistant.error_message = Some(message.to_string());
    assistant
}

#[tokio::test]
async fn turn_end_error_renders_without_usage_line() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    let mut state = test_state();
    let mut assistant = error_assistant("No API key for provider: anthropic");
    assistant.usage.total_tokens = 0;

    handle_ui_event(
        &ctx,
        &mut state,
        UiEvent::Session(AgentSessionEvent::Agent(rpi_agent::AgentEvent::TurnEnd {
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
        &ctx,
        &mut state,
        UiEvent::Session(AgentSessionEvent::Agent(rpi_agent::AgentEvent::TurnEnd {
            message: Box::new(assistant),
            tool_results: vec![],
        })),
    )
    .await;
    assert!(committed_text(&state).contains("↑ 100"));
    assert_eq!(
        state.context_tokens, 115,
        "ctx 估计 = in+out+cacheRead+cacheWrite"
    );
    assert!(state.usage.summary().contains("110 tok"));
}

#[tokio::test]
async fn auto_retry_end_failure_renders_red() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    let mut state = test_state();

    handle_ui_event(
        &ctx,
        &mut state,
        session_event(rpi_agent::AgentEvent::ToolExecutionStart {
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
        session_event(rpi_agent::AgentEvent::ToolExecutionEnd {
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
        session_event(rpi_agent::AgentEvent::MessageEnd {
            message: Box::new(rpi_agent::AgentMessage::tool_result_text(
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
    assert!(state.current_tool.is_none());
}

#[tokio::test]
async fn agent_settled_commits_dangling_tool_title() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    let mut state = test_state();

    handle_ui_event(
        &ctx,
        &mut state,
        session_event(rpi_agent::AgentEvent::ToolExecutionStart {
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
        .any(|message| matches!(message, rpi_agent::AgentMessage::BashExecution { .. })));
}

#[tokio::test]
async fn bash_bang_bang_skips_context_injection() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
            .any(|message| matches!(message, rpi_agent::AgentMessage::BashExecution { .. })),
        "!! 不应注入上下文"
    );
}

// ---- 历史回放 ----

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
        context_snapshot: None,
        active_tools: None,
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
        .set_messages(vec![rpi_agent::AgentMessage::Assistant(Box::new(
            assistant,
        ))])
        .unwrap();

    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    let mut state = test_state();
    replay_history(&ctx, &mut state);
    assert!(committed_text(&state).contains("回放中的思考"));
    assert!(committed_text(&state).contains("回放正文"));
}

#[tokio::test]
async fn replay_renders_user_assistant_toolcall_and_error() {
    let model = rpi_ai::Model::minimal("m", "mock", "mock");
    let provider = Arc::new(rpi_ai::ScriptedProvider::new(&model, vec![]));
    let built = crate::assembly::build_session(crate::assembly::BuildOptions {
        provider,
        model: model.clone(),
        ui: Arc::new(rpi_core::NoopUi),
        extension_specs: vec![],
        spawn_hook: None,
        session_store: crate::assembly::SessionStore::Memory,
        context_snapshot: None,
        active_tools: None,
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
    let ctx = ctx_of(&built, &resolver);
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    let mut state = test_state();

    let quit = super::handlers::execute_command(&ctx, &mut state, slash::SlashAction::Help).await;
    assert!(!quit);
    assert!(committed_text(&state).contains("/help"));

    super::handlers::execute_command(&ctx, &mut state, slash::SlashAction::Session).await;
    let rendered = committed_text(&state);
    assert!(rendered.contains("session"), "{rendered}");
    assert!(rendered.contains("model:"), "{rendered}");
    assert!(rendered.contains("usage:"), "{rendered}");
}

#[tokio::test]
async fn execute_thinking_with_arg_updates_footer() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
                ctx.session.set_model(model.clone());
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

#[tokio::test]
async fn compact_done_resets_context_estimate_and_reports() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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

// 流式输出期间预览上限恒定:视口高度不随增量改高(Inline 视口逐增量
// resize 会闪烁并把屏幕顶行推进 scrollback,冲刷真实历史)。busy 期间
// 一律 5:整回合高度恒定,唯一一次增高发生在提交时刻,空带被用户消息
// 落盘立即回填,不残留空白。
#[tokio::test]
async fn preview_cap_is_constant_during_streaming() {
    let mut state = test_state();
    assert_eq!(
        super::preview_cap_for(&state),
        super::view::MAX_PREVIEW_ROWS,
        "空闲时空隙保持最小"
    );
    // busy(含尚未收到任何增量的 Working 阶段)即取固定上限
    state.status = Status::Thinking;
    assert_eq!(
        super::preview_cap_for(&state),
        super::view::STREAM_PREVIEW_ROWS,
        "busy 一开始就预增高,空带被用户消息回填"
    );
    state.stream_text = "hello".into();
    assert_eq!(
        super::preview_cap_for(&state),
        super::view::STREAM_PREVIEW_ROWS,
        "流式开始即取固定上限"
    );
    state.stream_text = (1..=500)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        super::preview_cap_for(&state),
        super::view::STREAM_PREVIEW_ROWS,
        "长文本流式时上限恒定,不随内容增长"
    );
    state.stream_text.clear();
    state.pending_thinking = Some("思考中".into());
    assert_eq!(
        super::preview_cap_for(&state),
        super::view::STREAM_PREVIEW_ROWS,
        "思考中同样取固定上限"
    );
    state.pending_thinking = None;
    state.status = Status::Tool("bash".into());
    assert_eq!(
        super::preview_cap_for(&state),
        super::view::STREAM_PREVIEW_ROWS,
        "工具执行期间恒定"
    );
}

// ---- 斜杠补全弹窗(Codex 交互) ----

/// 弹窗状态机测试的共享前奏:内存会话 + 已输入 `/m` 的状态。
async fn popup_state_with_input(input: &str) -> (crate::assembly::BuiltSession, InteractiveState) {
    let built = built_memory_session().await;
    let mut state = test_state();
    for c in input.chars() {
        handle_key(&ctx_of(&built, &rpi_core::create_model_resolver()), &mut state, Key::Char(c)).await;
    }
    (built, state)
}

#[tokio::test]
async fn typing_slash_opens_filtered_popup() {
    let (_built, state) = popup_state_with_input("/m").await;
    assert!(state.slash_popup.visible(), "输入 /m 应弹出补全");
    // 前缀(/model)优先,模糊子序列(compact/theme 含 m)次之
    assert_eq!(state.slash_popup.match_count(), 3);
    assert_eq!(
        state.slash_popup.selected_entry().map(|e| e.name.as_str()),
        Some("model"),
        "前缀匹配应排首位"
    );

    // 继续输入到无匹配:弹窗退场
    let (built, mut state) = popup_state_with_input("/m").await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    for c in "zz".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    assert!(!state.slash_popup.visible());
}

#[tokio::test]
async fn enter_executes_partial_slash_directly() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
    let mut state = test_state();
    for c in "/mod".chars() {
        handle_key(&ctx, &mut state, Key::Char(c)).await;
    }
    // Enter(非完全匹配):补全为完整命令并立即执行,一次回车直达
    handle_key(&ctx, &mut state, Key::Enter).await;
    assert!(state.select.is_some(), "Enter 应直接执行 /model 打开选择器");
    assert!(state.editor.text().is_empty(), "执行后编辑器应清空");
}

#[tokio::test]
async fn exact_slash_input_executes_directly_on_enter() {
    let built = built_memory_session().await;
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
    let resolver = rpi_core::create_model_resolver();
    let ctx = ctx_of(&built, &resolver);
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
