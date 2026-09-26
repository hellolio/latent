//! 08 文档四种运行模式的集成测试:json 事件剥离 partial、rpc 命令分派与
//! 扩展 UI 反向通道、print 冒烟。全部用 ScriptedProvider,不联网。

use std::sync::{Arc, Mutex};

use rpi_ai::{ContentBlock, Model, ScriptedProvider, ScriptedTurn};
use rpi_cli::assembly::{build_session, BuildOptions};
use rpi_cli::modes;

fn test_model() -> Model {
    Model::minimal("test-model", "anthropic-messages", "anthropic")
}

fn scripted_provider(turns: Vec<ScriptedTurn>) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new(&test_model(), turns))
}

async fn build_with(provider: Arc<ScriptedProvider>) -> rpi_cli::assembly::BuiltSession {
    build_session(BuildOptions {
        provider,
        model: test_model(),
        ui: Arc::new(rpi_core::NoopUi),
        extension_specs: Vec::new(),
        spawn_hook: None,
        session_store: rpi_cli::assembly::SessionStore::Memory,
    })
    .await
    .expect("build_session")
}

/// 工具调用 + 文本两 turn 的脚本:覆盖 toolcall_* 事件线。
fn two_turn_provider() -> Arc<ScriptedProvider> {
    let m = test_model();
    // 首轮带已知 usage(T4:json 事件线的 usage 断言用)
    let mut first = rpi_ai::assistant_message(
        &m,
        vec![ContentBlock::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "echo mode-test" }),
        }],
        rpi_ai::StopReason::ToolUse,
    );
    first.usage = rpi_ai::Usage { input: 1, output: 1, total_tokens: 2, ..Default::default() };
    scripted_provider(vec![ScriptedTurn::new(first), ScriptedTurn::text(&m, "all done")])
}

// ---- 内存 writer(json 模式用:std::io::Write) ----

#[derive(Default, Clone)]
struct SharedVec(Arc<Mutex<Vec<u8>>>);

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

// ---- 内存 writer(rpc 模式用:tokio AsyncWrite) ----

#[derive(Default, Clone)]
struct SharedAsyncVec(Arc<Mutex<Vec<u8>>>);

impl tokio::io::AsyncWrite for SharedAsyncVec {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl SharedAsyncVec {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn json_lines(output: &str) -> Vec<serde_json::Value> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("每行都是合法 JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// json 模式
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_mode_emits_jsonl_without_streaming_partials() {
    let built = build_with(two_turn_provider()).await;
    let buffer = SharedVec::default();

    let stop = modes::json::run_json_mode(
        built,
        "hi".into(),
        Arc::new(Mutex::new(buffer.clone())),
    )
    .await
    .expect("json run");

    assert!(matches!(stop, rpi_agent::RunStop::EndTurn));
    let output = buffer.text();
    let lines = json_lines(&output);
    let types: Vec<&str> = lines.iter().filter_map(|v| v["type"].as_str()).collect();

    // 正向:生命周期事件齐全,toolcall_start 附 id/toolName
    assert!(types.contains(&"agent_start"));
    assert!(types.contains(&"turn_start"));
    assert!(types.contains(&"message_end"));
    assert!(types.contains(&"agent_end"));
    assert!(types.contains(&"agent_settled"));
    let toolcall_start =
        lines.iter().find(|v| v["type"] == "toolcall_start").expect("toolcall_start 事件");
    assert_eq!(toolcall_start["id"], "call-1");
    assert_eq!(toolcall_start["toolName"], "bash");
    let toolcall_end = lines.iter().find(|v| v["type"] == "toolcall_end").expect("toolcall_end");
    assert_eq!(toolcall_end["isError"], false);

    // 边界:流式 partial(delta/update)全部剥离
    assert!(!types.contains(&"message_delta"));
    assert!(!types.contains(&"message_update"));
    assert!(!output.contains("message_delta"));

    // T4:usage 已随 turn_end 事件输出(数值与 provider 返回一致)
    let turn_end = lines.iter().find(|v| v["type"] == "turn_end").expect("turn_end 事件");
    assert_eq!(turn_end["message"]["usage"]["input"].as_u64(), Some(1));
    assert_eq!(turn_end["message"]["usage"]["output"].as_u64(), Some(1));
}

#[tokio::test]
async fn json_mode_reports_error_stop_as_event_stream() {
    // 边界:provider 失败 → 失败编码进流(00 设计原则 5),事件线仍完整闭合
    let provider = scripted_provider(vec![ScriptedTurn::error(&test_model(), "boom")]);
    let built = build_with(provider).await;
    let buffer = SharedVec::default();

    let stop = modes::json::run_json_mode(
        built,
        "hi".into(),
        Arc::new(Mutex::new(buffer.clone())),
    )
    .await
    .unwrap();
    assert!(matches!(stop, rpi_agent::RunStop::Error(_)));
    let types: Vec<String> =
        json_lines(&buffer.text()).iter().filter_map(|v| v["type"].as_str().map(str::to_string)).collect();
    assert!(types.contains(&"agent_end".to_string()));
    assert!(types.contains(&"agent_settled".to_string()));
}

// ---------------------------------------------------------------------------
// rpc 模式
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_mode_dispatches_commands_and_streams_events() {
    let built = build_with(two_turn_provider()).await;
    let buffer = SharedAsyncVec::default();
    let writer: modes::rpc::SharedRpcWriter = Arc::new(tokio::sync::Mutex::new(buffer.clone()));

    // duplex 流模拟真实编辑器客户端:prompt 后等 agent_settled 事件,
    // 再发依赖 run 结果的查询命令(与 pi rpc-client 的使用方式一致)
    let (mut client, server) = tokio::io::duplex(4096);
    let server_task = tokio::spawn(modes::rpc::run_rpc_mode(built, server, writer));

    use tokio::io::AsyncWriteExt;
    client
        .write_all(concat!(
            r#"{"type":"get_state"}"#, "\n",
            r#"{"type":"prompt","message":"hi"}"#, "\n",
            r#"{"type":"bash","command":"echo rpc-bash"}"#, "\n",
            r#"{"type":"set_thinking_level","level":"high"}"#, "\n",
            "not json\n",
            r#"{"type":"prompt"}"#, "\n",
        )
        .as_bytes())
        .await
        .unwrap();
    wait_for_event(&buffer, "agent_settled").await;
    client
        .write_all(concat!(
            r#"{"type":"get_messages"}"#, "\n",
            r#"{"type":"get_entries"}"#, "\n",
            r#"{"type":"get_tree"}"#, "\n",
        )
        .as_bytes())
        .await
        .unwrap();
    drop(client);
    server_task.await.expect("rpc run").expect("rpc ok");

    let output = buffer.text();
    let lines = json_lines(&output);
    let responses: Vec<&serde_json::Value> =
        lines.iter().filter(|v| v["type"] == "response").collect();
    assert_eq!(responses.len(), 9, "每条命令(含两条坏命令)都恰好一个应答:{output}");
    // rpc 是异步协议:应答按 id 对应命令,到达顺序不保证
    let by_id = |id: u64| {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("缺 id={id} 的应答:{output}"))
    };

    // get_state:ok + 状态字段
    assert_eq!(by_id(1)["ok"], true);
    assert_eq!(by_id(1)["result"]["messageCount"], 0);
    assert_eq!(by_id(1)["result"]["isStreaming"], false);

    // prompt:ok + stopReason;事件流与应答在同一 stdout
    assert_eq!(by_id(2)["ok"], true);
    assert_eq!(by_id(2)["result"]["stopReason"], "end_turn");
    let event_types: Vec<&str> =
        lines.iter().filter(|v| v["type"] != "response").filter_map(|v| v["type"].as_str()).collect();
    assert!(event_types.contains(&"agent_start"));
    assert!(event_types.contains(&"toolcall_start"));
    assert!(event_types.contains(&"agent_settled"));

    // get_messages(settled 后发):消息已进转录(user + assistant + toolResult + …)
    assert_eq!(by_id(7)["ok"], true);
    assert!(by_id(7)["result"]["messages"].as_array().unwrap().len() >= 4);

    // bash:exitCode 0 + stdout
    assert_eq!(by_id(3)["ok"], true);
    assert_eq!(by_id(3)["result"]["exitCode"], 0);
    assert_eq!(by_id(3)["result"]["stdout"].as_str().unwrap().trim(), "rpc-bash");

    // set_thinking_level:ok
    assert_eq!(by_id(4)["ok"], true);

    // 坏命令:ok=false + 错误文案,不击穿循环
    assert_eq!(by_id(5)["ok"], false);
    assert!(by_id(5)["error"].as_str().unwrap().contains("命令解析失败"));
    assert_eq!(by_id(6)["ok"], false, "缺 message 字段也是解析失败");

    // get_entries/get_tree(settled 后发):会话持久化已装配,rpc 可查树
    assert_eq!(by_id(8)["ok"], true);
    assert!(by_id(8)["result"]["entries"].as_array().unwrap().len() >= 4);
    assert_eq!(by_id(9)["ok"], true);
    assert!(by_id(9)["result"]["tree"].is_array());
}

/// 轮询缓冲直到指定事件上线(测试辅助)。
async fn wait_for_event(buffer: &SharedAsyncVec, event_type: &str) {
    for _ in 0..200 {
        if buffer.text().contains(&format!("\"type\":\"{event_type}\"")) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("等待事件 {event_type} 超时:{}", buffer.text());
}

#[tokio::test]
async fn rpc_mode_supports_steer_during_run() {
    // run 进行中:steer 注入转向消息,get_state 仍可响应(stdin 不被 run 阻塞)
    let provider = scripted_provider(vec![
        ScriptedTurn::text(&test_model(), "slow").with_delay(800),
        ScriptedTurn::text(&test_model(), "after steer"),
    ]);
    let built = build_with(provider).await;
    let buffer = SharedAsyncVec::default();
    let writer: modes::rpc::SharedRpcWriter = Arc::new(tokio::sync::Mutex::new(buffer.clone()));

    let input: &[u8] = concat!(
        r#"{"type":"prompt","message":"start"}"#, "\n",
        r#"{"type":"steer","message":"change course"}"#, "\n",
        r#"{"type":"get_state"}"#, "\n",
    )
    .as_bytes();

    modes::rpc::run_rpc_mode(built, input, writer).await.expect("rpc run");

    let lines = json_lines(&buffer.text());
    let responses: Vec<&serde_json::Value> =
        lines.iter().filter(|v| v["type"] == "response").collect();
    // prompt 与 steer 的应答都到达;steer 在 run 期间被受理(不阻塞)
    assert_eq!(responses.len(), 3);
    let by_id = |id: u64| responses.iter().find(|r| r["id"] == id).unwrap();
    assert_eq!(by_id(2)["result"]["steered"], true);
    assert_eq!(by_id(3)["ok"], true);
}

#[tokio::test]
async fn rpc_extension_ui_backchannel_roundtrip() {
    // 扩展 UI 反向通道:select 请求上线 → extension_ui_response 路由回 oneshot
    let buffer = SharedAsyncVec::default();
    let writer: modes::rpc::SharedRpcWriter = Arc::new(tokio::sync::Mutex::new(buffer.clone()));
    let ui = Arc::new(modes::rpc::RpcUi::new(writer.clone()));

    let waiter = tokio::spawn({
        let ui = ui.clone();
        async move { rpi_core::ExtensionUi::select(&*ui, "pick one", &["a".into(), "b".into()]).await }
    });

    // 等 extension_ui_request 落到 writer
    let request = loop {
        let output = buffer.text();
        if let Some(line) = output.lines().find(|l| l.contains("extension_ui_request")) {
            break serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(!waiter.is_finished(), "select 不应在无应答时提前返回");
    };
    assert_eq!(request["request"]["kind"], "select");
    assert_eq!(request["request"]["options"][0], "a");
    let request_id = request["id"].as_u64().unwrap();

    // 客户端应答路由回 select 调用
    assert!(ui.resolve(request_id, serde_json::json!(1)).await);
    assert_eq!(waiter.await.unwrap(), Some(1));
    // 重复路由同一 id:失败(oneshot 已消费)
    assert!(!ui.resolve(request_id, serde_json::json!(0)).await);

    // notify:fire-and-forget,不需要应答
    rpi_core::ExtensionUi::notify(&*ui, "hello editor").await;
    assert!(buffer.text().contains("hello editor"));
}

// ---------------------------------------------------------------------------
// print 模式
// ---------------------------------------------------------------------------

#[tokio::test]
async fn print_mode_runs_one_prompt_to_completion() {
    let provider = scripted_provider(vec![ScriptedTurn::text(&test_model(), "printed reply")]);
    let stop = modes::print_mode::run_print_mode(provider, test_model(), "hello".into(), Vec::new(), rpi_cli::assembly::SessionStore::Memory)
        .await
        .expect("print run");
    assert!(matches!(stop, rpi_agent::RunStop::EndTurn));
}

// ---- T9:PI_* 会话环境注入经 build_session 全链路 ----

/// bash 工具回显 PI_MODEL:session 建好后 cell 已回填,执行时应拿到注入值。
#[tokio::test]
async fn bash_tool_receives_pi_session_env_via_build_session() {
    let m = test_model();
    let first = rpi_ai::assistant_message(
        &m,
        vec![ContentBlock::ToolCall {
            id: "call-env".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "echo $PI_MODEL $PI_SESSION_ID" }),
        }],
        rpi_ai::StopReason::ToolUse,
    );
    let provider = scripted_provider(vec![ScriptedTurn::new(first), ScriptedTurn::text(&m, "done")]);
    let built = build_with(provider).await;

    // prompt 期间 bash 实际执行;工具结果经转录可查
    let stop = built
        .session
        .prompt("run echo")
        .await
        .expect("prompt")
        .stop();
    assert_eq!(stop, rpi_agent::RunStop::EndTurn);

    let tool_results: Vec<String> = built
        .session
        .agent()
        .messages()
        .iter()
        .filter_map(|msg| match msg {
            rpi_agent::AgentMessage::ToolResult { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|block| block.as_text().map(str::to_string))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect();
    assert!(!tool_results.is_empty(), "应存在 bash 工具结果");
    let echoed = &tool_results[0];
    assert!(echoed.contains("test-model"), "PI_MODEL 应被注入: {echoed}");
    assert!(!echoed.contains("$"), "变量应已展开: {echoed}");
}

// ---- Session 文件持久化:CLI 装配走 file-backed session,重启可续聊 ----

async fn build_with_store(
    provider: Arc<ScriptedProvider>,
    store: rpi_cli::assembly::SessionStore,
) -> rpi_cli::assembly::BuiltSession {
    build_session(BuildOptions {
        provider,
        model: test_model(),
        ui: Arc::new(rpi_core::NoopUi),
        extension_specs: Vec::new(),
        spawn_hook: None,
        session_store: store,
    })
    .await
    .expect("build_session")
}

fn session_messages(entries: &[rpi_session::Entry]) -> Vec<rpi_agent::AgentMessage> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            rpi_session::Entry::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn file_backed_session_persists_jsonl_and_resumes() {
    let dir = std::env::temp_dir().join(format!("rpi_sessions_it_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let session_id = {
        let provider = scripted_provider(vec![ScriptedTurn::text(&test_model(), "reply-1")]);
        let built = build_with_store(
            provider,
            rpi_cli::assembly::SessionStore::New { dir: dir.clone() },
        )
        .await;
        let manager = built.session_manager.clone().expect("file-backed manager");
        let session_id = manager.session_id().to_string();
        built.session.prompt("你好").await.expect("prompt");
        built.session.wait_idle().await;
        // 文件名即 session id,header + user/assistant 已落盘
        let file = dir.join(format!("{session_id}.jsonl"));
        assert!(file.exists(), "session 文件应落盘: {}", file.display());
        let content = std::fs::read_to_string(&file).unwrap();
        let header: rpi_session::SessionHeader =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(header.kind, "session");
        assert_eq!(header.id, session_id);
        session_id
    };

    // 重启:从 JSONL 恢复(entries 与运行时一致),续聊接在同一树上
    {
        let file = dir.join(format!("{session_id}.jsonl"));
        let resumed = rpi_session::create_session(Some(&file)).unwrap();
        let messages = session_messages(&resumed.entries());
        assert!(matches!(&messages[0], rpi_agent::AgentMessage::System { .. }), "首条 system baseline");
        let turns: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m, rpi_agent::AgentMessage::User { .. } | rpi_agent::AgentMessage::Assistant(_)))
            .collect();
        assert_eq!(turns.len(), 2, "user + assistant");
        assert!(matches!(turns[0], rpi_agent::AgentMessage::User { .. }));
        assert!(matches!(turns[1], rpi_agent::AgentMessage::Assistant(_)));

        let leaf_before = resumed.get_leaf_id().unwrap();
        drop(resumed);
        let provider = scripted_provider(vec![ScriptedTurn::text(&test_model(), "reply-2")]);
        let built = build_with_store(
            provider,
            rpi_cli::assembly::SessionStore::Resume { file },
        )
        .await;
        built.session.prompt("再来一条").await.expect("prompt");
        built.session.wait_idle().await;
        let manager = built.session_manager.clone().unwrap();
        assert_eq!(manager.session_id(), session_id, "resume 不换 session id");
        let messages = session_messages(&manager.entries());
        // resume 会追加新的 system baseline(append-only:重建系统提示词也是 entry)
        assert_eq!(messages.len(), 6, "两段 system baseline + 两轮对话");
        assert!(matches!(&messages[3], rpi_agent::AgentMessage::System { .. }));
        assert!(matches!(&messages[4], rpi_agent::AgentMessage::User { .. }));
        assert!(matches!(&messages[5], rpi_agent::AgentMessage::Assistant(_)));
        let _ = leaf_before;
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

/// transcript 统一(文档验收 Test 1–3):完整 run(user → assistant(tool call)
/// → tool result → assistant)结束后,仅从 JSONL 重建的 context 必须与 Agent
/// 内存 context 一致,且 usage entry 已落盘。
#[tokio::test]
async fn jsonl_rebuild_matches_agent_context_after_tool_run() {
    let dir = std::env::temp_dir().join(format!("rpi_transcript_it_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let m = test_model();
    let first = rpi_ai::assistant_message(
        &m,
        vec![ContentBlock::ToolCall {
            id: "call-t1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "echo transcript-check" }),
        }],
        rpi_ai::StopReason::ToolUse,
    );
    let provider = scripted_provider(vec![ScriptedTurn::new(first), ScriptedTurn::text(&m, "done")]);
    let built = build_with_store(
        provider,
        rpi_cli::assembly::SessionStore::New { dir: dir.clone() },
    )
    .await;
    built.session.prompt("跑一下命令").await.expect("prompt");
    built.session.wait_idle().await;

    let agent_context = built.session.agent().messages();
    let file = dir.join(format!(
        "{}.jsonl",
        built.session_manager.as_ref().unwrap().session_id()
    ));
    drop(built);

    // Test 2:只从 session 文件恢复(不依赖 Agent 内存 Vec)
    let manager = rpi_session::create_session(Some(&file)).unwrap();
    let context = rpi_session::build_session_context(
        &manager.branch_entries(),
        manager.get_leaf_id().as_deref(),
    );
    assert_eq!(
        context.messages, agent_context,
        "session projection 必须与运行结束时的 Agent context 一致"
    );

    // Test 3:tool interaction 保留完整
    assert!(
        agent_context
            .iter()
            .any(|msg| matches!(msg, rpi_agent::AgentMessage::ToolResult { .. })),
        "tool result 应在恢复后的 context 中"
    );
    // usage entry 已随 assistant 定稿落盘
    assert!(
        manager.entries().iter().any(|e| matches!(e, rpi_session::Entry::Usage { .. })),
        "usage entry 应落盘"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
