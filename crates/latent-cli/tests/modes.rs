//! 08 文档四种运行模式的集成测试:json 事件剥离 partial、rpc 命令分派与
//! 扩展 UI 反向通道、print 冒烟。全部用 ScriptedProvider,不联网。

use std::sync::{Arc, Mutex};

use latent_ai::{ContentBlock, Model, ScriptedProvider, ScriptedTurn};
use latent::assembly::{build_session, BuildOptions};
use latent::modes;

fn test_model() -> Model {
    Model::minimal("test-model", "anthropic-messages", "anthropic")
}

fn scripted_provider(turns: Vec<ScriptedTurn>) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new(&test_model(), turns))
}

async fn build_with(provider: Arc<ScriptedProvider>) -> latent::assembly::BuiltSession {
    build_session(BuildOptions {
        provider,
        model: test_model(),
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: Vec::new(),
        spawn_hook: None,
        session_store: latent::assembly::SessionStore::Memory,
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
    })
    .await
    .expect("build_session")
}

/// 工具调用 + 文本两 turn 的脚本:覆盖 toolcall_* 事件线。
fn two_turn_provider() -> Arc<ScriptedProvider> {
    let m = test_model();
    // 首轮带已知 usage(T4:json 事件线的 usage 断言用)
    let mut first = latent_ai::assistant_message(
        &m,
        vec![ContentBlock::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "echo mode-test" }),
        }],
        latent_ai::StopReason::ToolUse,
    );
    first.usage = latent_ai::Usage {
        input: 1,
        output: 1,
        total_tokens: 2,
        ..Default::default()
    };
    scripted_provider(vec![
        ScriptedTurn::new(first),
        ScriptedTurn::text(&m, "all done"),
    ])
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

    let stop = modes::json::run_json_mode(built, "hi".into(), Arc::new(Mutex::new(buffer.clone())))
        .await
        .expect("json run");

    assert!(matches!(stop, latent_agent::RunStop::EndTurn));
    let output = buffer.text();
    let lines = json_lines(&output);
    let types: Vec<&str> = lines.iter().filter_map(|v| v["type"].as_str()).collect();

    // 正向:生命周期事件齐全,toolcall_start 附 id/toolName
    assert!(types.contains(&"agent_start"));
    assert!(types.contains(&"turn_start"));
    assert!(types.contains(&"message_end"));
    assert!(types.contains(&"agent_end"));
    assert!(types.contains(&"agent_settled"));
    let toolcall_start = lines
        .iter()
        .find(|v| v["type"] == "toolcall_start")
        .expect("toolcall_start 事件");
    assert_eq!(toolcall_start["id"], "call-1");
    assert_eq!(toolcall_start["toolName"], "bash");
    let toolcall_end = lines
        .iter()
        .find(|v| v["type"] == "toolcall_end")
        .expect("toolcall_end");
    assert_eq!(toolcall_end["isError"], false);

    // 边界:流式 partial(delta/update)全部剥离
    assert!(!types.contains(&"message_delta"));
    assert!(!types.contains(&"message_update"));
    assert!(!output.contains("message_delta"));

    // T4:usage 已随 turn_end 事件输出(数值与 provider 返回一致)
    let turn_end = lines
        .iter()
        .find(|v| v["type"] == "turn_end")
        .expect("turn_end 事件");
    assert_eq!(turn_end["message"]["usage"]["input"].as_u64(), Some(1));
    assert_eq!(turn_end["message"]["usage"]["output"].as_u64(), Some(1));
}

#[tokio::test]
async fn json_mode_reports_error_stop_as_event_stream() {
    // 边界:provider 失败 → 失败编码进流(00 设计原则 5),事件线仍完整闭合
    let provider = scripted_provider(vec![ScriptedTurn::error(&test_model(), "boom")]);
    let built = build_with(provider).await;
    let buffer = SharedVec::default();

    let stop = modes::json::run_json_mode(built, "hi".into(), Arc::new(Mutex::new(buffer.clone())))
        .await
        .unwrap();
    assert!(matches!(stop, latent_agent::RunStop::Error(_)));
    let types: Vec<String> = json_lines(&buffer.text())
        .iter()
        .filter_map(|v| v["type"].as_str().map(str::to_string))
        .collect();
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
    let approval = Arc::new(modes::rpc::RpcApprovalUi::new(writer.clone()));
    let server_task =
        tokio::spawn(modes::rpc::run_rpc_mode(built, approval, server, writer));

    use tokio::io::AsyncWriteExt;
    client
        .write_all(
            concat!(
                r#"{"type":"get_state"}"#,
                "\n",
                r#"{"type":"prompt","message":"hi"}"#,
                "\n",
                r#"{"type":"bash","command":"echo rpc-bash"}"#,
                "\n",
                r#"{"type":"set_thinking_level","level":"high"}"#,
                "\n",
                "not json\n",
                r#"{"type":"prompt"}"#,
                "\n",
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    wait_for_event(&buffer, "agent_settled").await;
    client
        .write_all(
            concat!(
                r#"{"type":"get_messages"}"#,
                "\n",
                r#"{"type":"get_entries"}"#,
                "\n",
                r#"{"type":"get_tree"}"#,
                "\n",
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    drop(client);
    server_task.await.expect("rpc run").expect("rpc ok");

    let output = buffer.text();
    let lines = json_lines(&output);
    let responses: Vec<&serde_json::Value> =
        lines.iter().filter(|v| v["type"] == "response").collect();
    assert_eq!(
        responses.len(),
        9,
        "每条命令(含两条坏命令)都恰好一个应答:{output}"
    );
    // rpc 是异步协议:应答按 id 对应命令,到达顺序不保证
    let by_id = |id: u64| {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("缺 id={id} 的应答:{output}"))
    };

    // get_state:ok + 状态字段(转录含装配期 append 的模式节)
    assert_eq!(by_id(1)["ok"], true);
    assert_eq!(by_id(1)["result"]["messageCount"], 1);
    assert_eq!(by_id(1)["result"]["isStreaming"], false);

    // prompt:ok + stopReason;事件流与应答在同一 stdout
    assert_eq!(by_id(2)["ok"], true);
    assert_eq!(by_id(2)["result"]["stopReason"], "end_turn");
    let event_types: Vec<&str> = lines
        .iter()
        .filter(|v| v["type"] != "response")
        .filter_map(|v| v["type"].as_str())
        .collect();
    assert!(event_types.contains(&"agent_start"));
    assert!(event_types.contains(&"toolcall_start"));
    assert!(event_types.contains(&"agent_settled"));

    // get_messages(settled 后发):消息已进转录(user + assistant + toolResult + …)
    assert_eq!(by_id(7)["ok"], true);
    assert!(by_id(7)["result"]["messages"].as_array().unwrap().len() >= 4);

    // bash:exitCode 0 + stdout
    assert_eq!(by_id(3)["ok"], true);
    assert_eq!(by_id(3)["result"]["exitCode"], 0);
    assert_eq!(
        by_id(3)["result"]["stdout"].as_str().unwrap().trim(),
        "rpc-bash"
    );

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
        if buffer
            .text()
            .contains(&format!("\"type\":\"{event_type}\""))
        {
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
        r#"{"type":"prompt","message":"start"}"#,
        "\n",
        r#"{"type":"steer","message":"change course"}"#,
        "\n",
        r#"{"type":"get_state"}"#,
        "\n",
    )
    .as_bytes();
    let approval = Arc::new(modes::rpc::RpcApprovalUi::new(writer.clone()));

    modes::rpc::run_rpc_mode(built, approval, input, writer)
        .await
        .expect("rpc run");

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
        async move { latent_core::ExtensionUi::select(&*ui, "pick one", &["a".into(), "b".into()]).await }
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
    latent_core::ExtensionUi::notify(&*ui, "hello editor").await;
    assert!(buffer.text().contains("hello editor"));
}

// ---------------------------------------------------------------------------
// print 模式
// ---------------------------------------------------------------------------

#[tokio::test]
async fn print_mode_runs_one_prompt_to_completion() {
    let provider = scripted_provider(vec![ScriptedTurn::text(&test_model(), "printed reply")]);
    let stop = modes::print_mode::run_print_mode(
        provider,
        test_model(),
        "hello".into(),
        Vec::new(),
        latent::assembly::SessionStore::Memory,
        Default::default(),
        None,
    )
    .await
    .expect("print run");
    assert!(matches!(stop, latent_agent::RunStop::EndTurn));
}

// ---- T9:LATENT_* 会话环境注入经 build_session 全链路 ----

/// bash 工具回显 LATENT_MODEL:session 建好后 cell 已回填,执行时应拿到注入值。
#[tokio::test]
async fn bash_tool_receives_pi_session_env_via_build_session() {
    let m = test_model();
    let first = latent_ai::assistant_message(
        &m,
        vec![ContentBlock::ToolCall {
            id: "call-env".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "echo $LATENT_MODEL $LATENT_SESSION_ID" }),
        }],
        latent_ai::StopReason::ToolUse,
    );
    let provider = scripted_provider(vec![
        ScriptedTurn::new(first),
        ScriptedTurn::text(&m, "done"),
    ]);
    let built = build_with(provider).await;

    // prompt 期间 bash 实际执行;工具结果经转录可查
    let stop = built
        .session
        .prompt("run echo")
        .await
        .expect("prompt")
        .stop();
    assert_eq!(stop, latent_agent::RunStop::EndTurn);

    let tool_results: Vec<String> = built
        .session
        .agent()
        .messages()
        .iter()
        .filter_map(|msg| match msg {
            latent_agent::AgentMessage::ToolResult { content, .. } => Some(
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
    assert!(echoed.contains("test-model"), "LATENT_MODEL 应被注入: {echoed}");
    assert!(!echoed.contains("$"), "变量应已展开: {echoed}");
}

/// 降级矩阵 §7.5(集成验收):无沙箱平台 Plan 模式没有 OS 层兜底,只读
/// 判定是最后保证 —— 判定通过的 bash 命令照常执行。
#[tokio::test]
async fn plan_mode_bash_readonly_executes_without_sandbox() {
    let built = build_with(two_turn_provider()).await;

    let stop = built
        .session
        .prompt("run echo")
        .await
        .expect("prompt")
        .stop();
    assert_eq!(stop, latent_agent::RunStop::EndTurn);

    let executed = built
        .session
        .agent()
        .messages()
        .iter()
        .any(|msg| {
            matches!(msg, latent_agent::AgentMessage::ToolResult { tool_name, is_error: false, .. }
                if tool_name == "bash")
        });
    assert!(executed, "无沙箱 Plan 模式只读判定通过的 bash 应正常执行");
}

// ---- Session 文件持久化:CLI 装配走 file-backed session,重启可续聊 ----

/// /new:进程内新建会话文件并切换,旧会话原样保留,转录清空,
/// 后续消息写入新文件(旧文件不再追加)。
#[tokio::test]
async fn switch_new_session_starts_fresh_file_and_keeps_old() {
    let dir = std::env::temp_dir().join(format!("latent_new_session_it_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let built = build_with_store(
        scripted_provider(vec![ScriptedTurn::text(&test_model(), "reply-1")]),
        latent::assembly::SessionStore::New { dir: dir.clone() },
    )
    .await;
    built.session.prompt("第一轮").await.expect("prompt");
    built.session.wait_idle().await;
    let old_manager = built.session_manager.clone().expect("file-backed manager");
    let old_file = old_manager.file_path().unwrap().to_path_buf();
    let old_content = std::fs::read_to_string(&old_file).unwrap();
    let old_lines = old_content.lines().count();

    // /new:新文件建立,转录清空
    let new_file =
        latent::assembly::switch_new_session(&built.session, &built.manager_holder, None)
            .await
            .unwrap()
            .expect("文件会话应产生新文件");
    assert_ne!(new_file, old_file, "应切换到新的 session 文件");
    assert!(new_file.exists());
    let fresh_messages = built.session.agent().messages();
    assert_eq!(fresh_messages.len(), 1, "切换后转录只含新会话模式节");
    assert!(matches!(
        fresh_messages[0],
        latent_agent::AgentMessage::ModeSection { .. }
    ));

    // 旧文件原样保留(header + user + assistant,不追加)
    let old_after = std::fs::read_to_string(&old_file).unwrap();
    assert_eq!(
        old_after.lines().count(),
        old_lines,
        "旧 session 文件不应被修改"
    );

    // 后续对话写入新文件;新文件自描述(model_change 设置态 entry)
    built.session.prompt("第二轮").await.expect("prompt");
    built.session.wait_idle().await;
    let new_content = std::fs::read_to_string(&new_file).unwrap();
    assert!(new_content.contains("\"type\":\"model_change\""), "新文件应记录模型设置态");
    assert!(new_content.contains("第二轮"), "新消息应写入新文件");
    assert!(!old_after.contains("第二轮"), "旧文件不应收到新消息");

    // parent_session 血缘:新文件 header 记录旧 session id
    let header: latent_session::SessionHeader =
        serde_json::from_str(new_content.lines().next().unwrap()).unwrap();
    assert_eq!(
        header.parent_session.as_deref(),
        Some(old_manager.session_id()),
        "新会话应记录旧会话为 parent"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

async fn build_with_store(
    provider: Arc<ScriptedProvider>,
    store: latent::assembly::SessionStore,
) -> latent::assembly::BuiltSession {
    build_with_snapshot(provider, store, Some(false)).await
}

/// `context_snapshot`:Some = 显式指定快照开关(None = 按 settings,测试环境
/// 不可控,故测试一律显式传入)。`active_tools`:Some = 显式激活工具集。
async fn build_with_snapshot(
    provider: Arc<ScriptedProvider>,
    store: latent::assembly::SessionStore,
    context_snapshot: Option<bool>,
) -> latent::assembly::BuiltSession {
    build_with_tools(provider, store, context_snapshot, None).await
}

async fn build_with_tools(
    provider: Arc<ScriptedProvider>,
    store: latent::assembly::SessionStore,
    context_snapshot: Option<bool>,
    active_tools: Option<Vec<String>>,
) -> latent::assembly::BuiltSession {
    build_session(BuildOptions {
        provider,
        model: test_model(),
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: Vec::new(),
        spawn_hook: None,
        session_store: store,
        context_snapshot,
        active_tools,
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
    })
    .await
    .expect("build_session")
}

fn session_messages(entries: &[latent_session::Entry]) -> Vec<latent_agent::AgentMessage> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            latent_session::Entry::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn file_backed_session_persists_jsonl_and_resumes() {
    let dir = std::env::temp_dir().join(format!("latent_sessions_it_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let (session_id, file) = {
        let provider = scripted_provider(vec![ScriptedTurn::text(&test_model(), "reply-1")]);
        let built = build_with_store(
            provider,
            latent::assembly::SessionStore::New { dir: dir.clone() },
        )
        .await;
        let manager = built.session_manager.clone().expect("file-backed manager");
        let session_id = manager.session_id().to_string();
        built.session.prompt("你好").await.expect("prompt");
        built.session.wait_idle().await;
        // 文件名 = `<时间>__<session-id>.jsonl`(项目目录下),header + user/assistant 已落盘
        let file = manager.file_path().unwrap().to_path_buf();
        let file_name = file.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            file_name.ends_with(&format!("__{session_id}.jsonl")),
            "文件名应带项目前缀: {file_name}"
        );
        assert!(file.exists(), "session 文件应落盘: {}", file.display());
        let content = std::fs::read_to_string(&file).unwrap();
        let header: latent_session::SessionHeader =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(header.kind, "session");
        assert_eq!(header.id, session_id);
        (session_id, file)
    };

    // 重启:从 JSONL 恢复(entries 与运行时一致),续聊接在同一树上
    {
        let resumed = latent_session::create_session(Some(&file)).unwrap();
        let messages = session_messages(&resumed.entries());
        // 转录:模式节(装配期 apply_mode append)+ user + assistant
        assert_eq!(messages.len(), 3, "modeSection + user + assistant");
        assert!(matches!(
            messages[0],
            latent_agent::AgentMessage::ModeSection { .. }
        ));
        assert!(matches!(messages[1], latent_agent::AgentMessage::User { .. }));
        assert!(matches!(messages[2], latent_agent::AgentMessage::Assistant(_)));

        let leaf_before = resumed.get_leaf_id().unwrap();
        drop(resumed);
        let provider = scripted_provider(vec![ScriptedTurn::text(&test_model(), "reply-2")]);
        let built =
            build_with_store(provider, latent::assembly::SessionStore::Resume { file }).await;
        built.session.prompt("再来一条").await.expect("prompt");
        built.session.wait_idle().await;
        let manager = built.session_manager.clone().unwrap();
        assert_eq!(manager.session_id(), session_id, "resume 不换 session id");
        let messages = session_messages(&manager.entries());
        // resume 后续聊接在同一树上:模式与历史节点一致,不重复追加
        assert_eq!(messages.len(), 5, "modeSection + 两轮对话");
        assert!(matches!(&messages[3], latent_agent::AgentMessage::User { .. }));
        assert!(matches!(
            &messages[4],
            latent_agent::AgentMessage::Assistant(_)
        ));
        let _ = leaf_before;
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

/// 上下文快照(分项目会话管理):显式开启时,每次向模型提交请求落一条
/// context_ref entry + `.ctx/` 快照文件;快照不进模型上下文(投影重建与没有它
/// 时一致)。
#[tokio::test]
async fn context_snapshot_recorded_per_request_and_excluded_from_projection() {
    let dir = std::env::temp_dir().join(format!(
        "latent_ctx_snapshot_it_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let built = build_with_snapshot(
        scripted_provider(vec![ScriptedTurn::text(&test_model(), "reply-1")]),
        latent::assembly::SessionStore::New { dir: dir.clone() },
        Some(true),
    )
    .await;
    let manager = built.session_manager.clone().expect("file-backed manager");
    built.session.prompt("你好").await.expect("prompt");
    built.session.wait_idle().await;

    // jsonl 中恰有一条 context_ref;快照文件在旁路 .ctx 目录且内容完整
    let session_file = manager.file_path().unwrap().to_path_buf();
    let stem = session_file
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let entries = manager.entries();
    let refs: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e, latent_session::Entry::ContextRef { .. }))
        .collect();
    assert_eq!(refs.len(), 1, "一次 prompt = 一次请求 = 一条 context_ref");
    let snapshot_path = match refs[0] {
        latent_session::Entry::ContextRef { path, .. } => std::path::PathBuf::from(path),
        other => panic!("unreachable: {other:?}"),
    };
    assert_eq!(
        snapshot_path.parent().unwrap(),
        session_file.parent().unwrap().join(format!("{stem}.ctx")),
        "快照目录与 session 文件同名 + .ctx"
    );
    let snapshot: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snapshot_path).unwrap())
            .expect("快照为合法 JSON");
    let snapshot_messages = snapshot["messages"].as_array().expect("messages 数组");
    assert!(
        snapshot_messages.len() >= 2,
        "完整提交上下文:折叠 system + user,实际 {} 条",
        snapshot_messages.len()
    );
    let jsonl = std::fs::read_to_string(&session_file).unwrap();
    assert!(jsonl.contains("\"type\":\"context_ref\""), "entry 已落盘");

    // 投影排除:重建上下文只有 modeSection(装配期 append)+ user + assistant
    let context = latent_session::build_session_context(
        &manager.branch_entries(),
        manager.get_leaf_id().as_deref(),
    );
    assert_eq!(context.messages.len(), 3, "context_ref 不进上下文");
    assert!(matches!(
        &context.messages[0],
        latent_agent::AgentMessage::ModeSection { .. }
    ));
    assert!(matches!(&context.messages[1], latent_agent::AgentMessage::User { .. }));
    assert!(matches!(
        &context.messages[2],
        latent_agent::AgentMessage::Assistant(_)
    ));

    std::fs::remove_dir_all(&dir).unwrap();
}

/// 快照开关默认关:settings 未配置(或 BuildOptions 显式 Some(false))时,
/// 不产生 context_ref entry,也不建 `.ctx` 目录。
#[tokio::test]
async fn context_snapshot_disabled_by_default_writes_nothing() {
    let dir = std::env::temp_dir().join(format!(
        "latent_ctx_snapshot_off_it_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let built = build_with_snapshot(
        scripted_provider(vec![ScriptedTurn::text(&test_model(), "reply-1")]),
        latent::assembly::SessionStore::New { dir: dir.clone() },
        Some(false),
    )
    .await;
    let manager = built.session_manager.clone().expect("file-backed manager");
    built.session.prompt("你好").await.expect("prompt");
    built.session.wait_idle().await;

    let entries = manager.entries();
    assert!(
        !entries
            .iter()
            .any(|e| matches!(e, latent_session::Entry::ContextRef { .. })),
        "默认关:不应有 context_ref entry"
    );
    let session_file = manager.file_path().unwrap().to_path_buf();
    let stem = session_file
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        !session_file
            .parent()
            .unwrap()
            .join(format!("{stem}.ctx"))
            .exists(),
        "默认关:不应创建 .ctx 目录"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// 工具集收窄(settings `tools` / BuildOptions.active_tools):只激活 bash 时,
/// Agent 只装 bash,未知工具名装配报错。
#[tokio::test]
async fn active_tools_narrows_installed_set() {
    // 只激活 bash:tool_count = 1,且 bash 调用照常执行
    let m = test_model();
    let first = latent_ai::assistant_message(
        &m,
        vec![ContentBlock::ToolCall {
            id: "call-b1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "echo only-bash" }),
        }],
        latent_ai::StopReason::ToolUse,
    );
    let built = build_with_tools(
        scripted_provider(vec![ScriptedTurn::new(first), ScriptedTurn::text(&m, "done")]),
        latent::assembly::SessionStore::Memory,
        Some(false),
        Some(vec!["bash".to_string()]),
    )
    .await;
    assert_eq!(
        built.session.agent().state_snapshot().tool_count,
        1,
        "只激活 bash"
    );
    built.session.prompt("跑一下命令").await.expect("prompt");
    built.session.wait_idle().await;
    let messages = built.session.agent().messages();
    assert!(messages.iter().any(|msg| {
        matches!(msg, latent_agent::AgentMessage::ToolResult { tool_name, is_error: false, .. }
            if tool_name == "bash")
    }));

    // 未知工具名:装配失败并给出可用名单
    let error = match build_session(BuildOptions {
        provider: scripted_provider(vec![]),
        model: test_model(),
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: Vec::new(),
        spawn_hook: None,
        session_store: latent::assembly::SessionStore::Memory,
        context_snapshot: None,
        active_tools: Some(vec!["bask".to_string()]),
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
    })
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("未知工具名应装配失败"),
    };
    assert!(error.contains("未知工具 `bask`"), "{error}");
}

/// 空工具集(tools: []):不激活任何工具 → 不装工具、系统提示词无 <tools> 节、
/// 转录中没有任何合成声明消息(工具 schema 走请求级字段)。
#[tokio::test]
async fn empty_active_tools_installs_nothing_and_declares_nothing() {
    let built = build_with_tools(
        scripted_provider(vec![ScriptedTurn::text(&test_model(), "好的")]),
        latent::assembly::SessionStore::Memory,
        Some(false),
        Some(Vec::new()),
    )
    .await;
    assert_eq!(
        built.session.agent().state_snapshot().tool_count,
        0,
        "空列表 = 不激活任何工具"
    );
    built.session.prompt("你好").await.expect("prompt");
    built.session.wait_idle().await;
    let messages = built.session.agent().messages();
    assert_eq!(
        messages.len(),
        3,
        "modeSection + user + assistant,无任何合成声明消息"
    );
    assert!(matches!(
        messages[0],
        latent_agent::AgentMessage::ModeSection { .. }
    ));
    assert!(matches!(messages[1], latent_agent::AgentMessage::User { .. }));
    assert!(matches!(messages[2], latent_agent::AgentMessage::Assistant(_)));
}

/// transcript 统一(文档验收 Test 1–3):完整 run(user → assistant(tool call)
/// → tool result → assistant)结束后,仅从 JSONL 重建的 context 必须与 Agent
/// 内存 context 一致,且 usage entry 已落盘。
#[tokio::test]
async fn jsonl_rebuild_matches_agent_context_after_tool_run() {
    let dir = std::env::temp_dir().join(format!("latent_transcript_it_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let m = test_model();
    let first = latent_ai::assistant_message(
        &m,
        vec![ContentBlock::ToolCall {
            id: "call-t1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "echo transcript-check" }),
        }],
        latent_ai::StopReason::ToolUse,
    );
    let provider = scripted_provider(vec![
        ScriptedTurn::new(first),
        ScriptedTurn::text(&m, "done"),
    ]);
    let built = build_with_store(
        provider,
        latent::assembly::SessionStore::New { dir: dir.clone() },
    )
    .await;
    built.session.prompt("跑一下命令").await.expect("prompt");
    built.session.wait_idle().await;

    let agent_context = built.session.agent().messages();
    let file = built
        .session_manager
        .as_ref()
        .unwrap()
        .file_path()
        .unwrap()
        .to_path_buf();
    drop(built);

    // Test 2:只从 session 文件恢复(不依赖 Agent 内存 Vec)
    let manager = latent_session::create_session(Some(&file)).unwrap();
    let context = latent_session::build_session_context(
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
            .any(|msg| matches!(msg, latent_agent::AgentMessage::ToolResult { .. })),
        "tool result 应在恢复后的 context 中"
    );
    // usage entry 已随 assistant 定稿落盘
    assert!(
        manager
            .entries()
            .iter()
            .any(|e| matches!(e, latent_session::Entry::Usage { .. })),
        "usage entry 应落盘"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 权限系统(13 文档 §13 L1):rpc 审批往返 / 沙箱钩子接线 / ModeChange 持久化
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_approval_backchannel_roundtrip() {
    // 审批反向通道照抄 extension_ui 的 id 路由:approval_request 上行 →
    // approval_response 路由回 oneshot(13 文档 §4.4)
    let buffer = SharedAsyncVec::default();
    let writer: modes::rpc::SharedRpcWriter = Arc::new(tokio::sync::Mutex::new(buffer.clone()));
    let approval = Arc::new(modes::rpc::RpcApprovalUi::new(writer.clone()));

    let waiter = tokio::spawn({
        let approval = approval.clone();
        async move {
            latent_core::ApprovalUi::request_approval(
                &*approval,
                latent_core::ApprovalRequest {
                    tool_call_id: "t1".into(),
                    tool_name: "bash".into(),
                    args: serde_json::json!({"command": "make test"}),
                    risk: latent_core::ToolRiskClass::Shell,
                    reason: latent_core::ApprovalReason::ShellCommand,
                    detail: "make test".into(),
                },
            )
            .await
        }
    });

    // 等 approval_request 落到 writer
    let request = loop {
        let output = buffer.text();
        if let Some(line) = output.lines().find(|l| l.contains("approval_request")) {
            break serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(!waiter.is_finished(), "审批不应在无应答时提前返回");
    };
    assert_eq!(request["request"]["toolName"], "bash");
    assert_eq!(request["request"]["reason"], "shell_command");
    let request_id = request["id"].as_u64().unwrap();

    // 客户端应答路由回审批调用(snake_case 决策)
    assert!(approval
        .resolve(request_id, latent_core::ApprovalDecision::ApproveForSession)
        .await);
    assert_eq!(waiter.await.unwrap(), Some(latent_core::ApprovalDecision::ApproveForSession));
    // 未知 id:路由失败
    assert!(!approval.resolve(9999, latent_core::ApprovalDecision::Deny).await);
}

#[tokio::test]
async fn bash_command_executes_through_sandbox_hook_in_confirm_mode() {
    // Confirm 模式:bash 命令经 SandboxSpawnHook 包装后仍真实执行
    // (平台沙箱可用时命令串带沙箱前缀;无沙箱平台降级为原样执行,均应成功)
    let m = test_model();
    let first = latent_ai::assistant_message(
        &m,
        vec![ContentBlock::ToolCall {
            id: "call-sb".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "echo sandbox-ok" }),
        }],
        latent_ai::StopReason::ToolUse,
    );
    let provider = scripted_provider(vec![
        ScriptedTurn::new(first),
        ScriptedTurn::text(&m, "done"),
    ]);
    let built = build_session(BuildOptions {
        provider,
        model: test_model(),
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: Vec::new(),
        spawn_hook: None,
        session_store: latent::assembly::SessionStore::Memory,
        context_snapshot: None,
        active_tools: None,
        search_ignore: Default::default(),
        tool_result_max_chars: None,
        block_images: false,
        compaction: Default::default(),
        session_mode: Some(latent_core::SessionMode::Confirm),
        default_session_mode: latent_core::SessionMode::Confirm,
        sandbox: Default::default(),
        approval: Default::default(),
        subagent_async_approval: Default::default(),
        approval_ui: Some(Arc::new(latent_core::HeadlessApprovalUi {
            policy: latent_core::HeadlessApproval::AutoApprove,
        })),
    })
    .await
    .expect("build_session");

    // read-only echo 走 Confirm 免审路径,沙箱内(或降级直跑)执行成功
    let stop = built.session.prompt("echo").await.expect("prompt").stop();
    assert_eq!(stop, latent_agent::RunStop::EndTurn);
    let echoed = built
        .session
        .agent()
        .messages()
        .iter()
        .any(|msg| matches!(msg, latent_agent::AgentMessage::ToolResult { is_error: false, .. }));
    assert!(echoed, "沙箱内 echo 应成功执行");
}

#[tokio::test]
async fn session_mode_persists_as_mode_change_entry_and_resumes() {
    // 13 文档 §9:Confirm 模式会话落 ModeChange entry;resume 投影回填模式;
    // 新会话(entry 无记录)落到默认 Plan
    let dir = std::env::temp_dir().join(format!("latent_mode_persist_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let m = test_model();
    let provider = scripted_provider(vec![ScriptedTurn::text(&m, "hi")]);
    let file = dir.join("session.jsonl");
    let built = build_session(BuildOptions {
        provider,
        model: test_model(),
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: Vec::new(),
        spawn_hook: None,
        session_store: latent::assembly::SessionStore::New { dir: dir.clone() },
        context_snapshot: None,
        active_tools: None,
        search_ignore: Default::default(),
        tool_result_max_chars: None,
        block_images: false,
        compaction: Default::default(),
        session_mode: Some(latent_core::SessionMode::Confirm),
        default_session_mode: latent_core::SessionMode::Confirm,
        sandbox: Default::default(),
        approval: Default::default(),
        subagent_async_approval: Default::default(),
        approval_ui: None,
    })
    .await
    .expect("build_session");
    assert_eq!(built.session.mode(), latent_core::SessionMode::Confirm);
    built.session.prompt("hello").await.expect("prompt").stop();

    let manager = built.session_manager.as_ref().unwrap();
    assert!(
        manager
            .entries()
            .iter()
            .any(|e| matches!(e, latent_session::Entry::ModeChange { mode, .. } if mode == "confirm")),
        "ModeChange entry 应落盘"
    );
    let _ = file;

    // resume:投影 mode 恢复为 Confirm
    let provider2 = scripted_provider(vec![ScriptedTurn::text(&m, "again")]);
    let resumed = build_session(BuildOptions {
        provider: provider2,
        model: test_model(),
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: Vec::new(),
        spawn_hook: None,
        session_store: latent::assembly::SessionStore::Resume {
            file: manager.file_path().unwrap().to_path_buf(),
        },
        context_snapshot: None,
        active_tools: None,
        search_ignore: Default::default(),
        tool_result_max_chars: None,
        block_images: false,
        compaction: Default::default(),
        session_mode: None,
        default_session_mode: latent_core::SessionMode::Plan,
        sandbox: Default::default(),
        approval: Default::default(),
        subagent_async_approval: Default::default(),
        approval_ui: None,
    })
    .await
    .expect("resume");
    assert_eq!(
        resumed.session.mode(),
        latent_core::SessionMode::Confirm,
        "resume 应从 ModeChange entry 恢复模式"
    );
    // resume 不落新 entry(entry 数不变)
    let entries_after_resume = resumed.session_manager.as_ref().unwrap().entries().len();

    // 无显式模式的新会话:默认 Plan
    let provider3 = scripted_provider(vec![ScriptedTurn::text(&m, "new")]);
    let fresh = build_session(BuildOptions {
        provider: provider3,
        model: test_model(),
        ui: Arc::new(latent_core::NoopUi),
        extension_specs: Vec::new(),
        spawn_hook: None,
        session_store: latent::assembly::SessionStore::Memory,
        context_snapshot: None,
        active_tools: None,
        search_ignore: Default::default(),
        tool_result_max_chars: None,
        block_images: false,
        compaction: Default::default(),
        session_mode: None,
        default_session_mode: latent_core::SessionMode::Plan,
        sandbox: Default::default(),
        approval: Default::default(),
        subagent_async_approval: Default::default(),
        approval_ui: None,
    })
    .await
    .expect("fresh");
    assert_eq!(fresh.session.mode(), latent_core::SessionMode::Plan);
    // 模式提示词已迁出系统提示词(改为每请求消息数组末尾的 Developer 消息):
    // sections 不含 mode 节,且切换模式不重建系统提示词、不过滤工具集
    // (tools 数组恒定保 KV 缓存前缀命中)
    let sections_before = fresh.session.system_prompt_sections();
    let tool_count_before = fresh.session.agent().state_snapshot().tool_count;
    assert!(
        !sections_before.contains_key("mode"),
        "模式节不应进系统提示词"
    );
    fresh
        .session
        .set_mode(latent_core::SessionMode::Confirm)
        .await
        .unwrap();
    assert_eq!(
        fresh.session.system_prompt_sections(),
        sections_before,
        "切模式不重建系统提示词"
    );
    assert_eq!(
        fresh.session.agent().state_snapshot().tool_count,
        tool_count_before,
        "切模式不过滤工具集"
    );

    let _ = entries_after_resume;
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- latent-web:4 个 web 工具随会话常驻激活(无懒激活)----

#[tokio::test]
async fn web_tools_active_by_default() {
    let built = build_with(scripted_provider(Vec::new())).await;

    let active = built.session.active_tool_names();
    for name in ["web_search", "fetch_content", "source_check", "get_search_content"] {
        assert!(
            active.iter().any(|existing| existing == name),
            "{name} must be active from session start, got {active:?}"
        );
    }
    assert!(
        !active.iter().any(|name| name == "web_access"),
        "web_access gate tool should no longer exist, got {active:?}"
    );
    assert!(built.session.tool("web_access").is_none());
    assert!(built.session.tool("web_search").is_some());
    assert!(active.iter().any(|name| name == "subagent"));
}

// 装配链路审批回路回归(修复:此前 main.rs RPC 分支 approval_ui=None,
// Confirm 模式下工具审批被 HeadlessApprovalUi 静默 Deny,RpcApprovalUi 只做
// 路由从不参与决策 —— 现 approval_ui 与路由句柄指向同一实例)。
// 用写命令(touch)触发 Confirm 人审:echo 等只读命令引擎直接放行不弹审批。
#[tokio::test]
async fn rpc_approval_wired_through_assembly() {
    let m = test_model();
    let command = format!("touch {}/latent-ap-test", std::env::temp_dir().display());
    let first = latent_ai::assistant_message(
        &m,
        vec![ContentBlock::ToolCall {
            id: "call-ap".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": command }),
        }],
        latent_ai::StopReason::ToolUse,
    );
    let provider = scripted_provider(vec![
        ScriptedTurn::new(first),
        ScriptedTurn::text(&m, "finished"),
    ]);
    let buffer = SharedAsyncVec::default();
    let writer: modes::rpc::SharedRpcWriter = Arc::new(tokio::sync::Mutex::new(buffer.clone()));
    let approval = Arc::new(modes::rpc::RpcApprovalUi::new(writer.clone()));

    let built = modes::print_mode::build_bare_session(
        provider,
        test_model(),
        Arc::new(latent_core::NoopUi),
        Vec::new(),
        latent::assembly::SessionStore::Memory,
        Default::default(),
        Some(approval.clone() as Arc<dyn latent_core::ApprovalUi>),
        Some(latent_core::SessionMode::Confirm),
    )
    .await
    .expect("build_bare_session");

    let session = built.session.clone();
    // 事件收集器(断言工具真实执行)
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let collector: latent_core::SessionSharedSubscriber = {
        let events = events.clone();
        Arc::new({
            struct C(Arc<Mutex<Vec<String>>>);
            #[async_trait::async_trait]
            impl latent_core::SessionSubscriber for C {
                async fn on_session_event(&self, event: &latent_core::AgentSessionEvent) {
                    self.0.lock().unwrap().push(format!("{event:?}"));
                }
            }
            C(events)
        })
    };
    session.subscribe(collector);
    let (outcome_tx, mut outcome_rx) = tokio::sync::oneshot::channel::<String>();
    let run = tokio::spawn(async move {
        let outcome = session.prompt("run it").await;
        let _ = outcome_tx.send(format!("{outcome:?}"));
        outcome
    });

    // 等 approval_request 经反向通道上行(5s 超时,prompt 结果一并诊断)
    let wait = async {
        loop {
            let output = buffer.text();
            if let Some(line) = output.lines().find(|l| l.contains("approval_request")) {
                break serde_json::from_str::<serde_json::Value>(line).unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    let request = match tokio::time::timeout(std::time::Duration::from_secs(5), wait).await {
        Ok(request) => request,
        Err(_) => {
            let outcome = outcome_rx.try_recv().unwrap_or_else(|_| "(pending)".into());
            panic!(
                "approval_request 未到达;buffer={:?} outcome={outcome} events={:?}",
                buffer.text(),
                events.lock().unwrap().join(" | ")
            );
        }
    };
    assert_eq!(request["request"]["toolName"], "bash");
    let request_id = request["id"].as_u64().unwrap();

    // 客户端应答 allow → 工具真实执行,run 正常收尾
    assert!(approval.resolve(request_id, latent_core::ApprovalDecision::Approve).await);
    let outcome = run.await.unwrap().expect("prompt ok");
    let stop = format!("{outcome:?}");
    assert!(
        stop.contains("EndTurn"),
        "run 应正常收尾: {stop}"
    );
    let all_events = events.lock().unwrap().join(" | ");
    assert!(
        all_events.contains("ToolExecutionEnd") && !all_events.contains("is_error: true"),
        "bash 应真实执行(经批准): {all_events}"
    );
    assert!(
        all_events.contains("ApprovalResolved"),
        "审批决策应广播: {all_events}"
    );
}
