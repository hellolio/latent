//! subagent 引擎集成测试(ScriptedProvider 模式):
//! 同步嵌套闭环、递归防护、超时、并行排队、异步 + supervisor 唤醒、
//! list/stop、后台 Deny 审批。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use latent_agent::{
    Agent, PassthroughHooks, Tool, ToolCall, ToolError, ToolOutput, ToolUpdater,
};
use latent_ai::{ContentBlock, Model, ScriptedProvider, ScriptedTurn};
use latent_core::extensions::ExtensionRegistry;
use latent_core::{
    create_agent_session, ApprovalRules, AgentSessionConfig, HeadlessApproval, NoopUi,
    PermissionEngine, SandboxConfig, SessionMode, SubagentDeps, SubagentTool,
    SystemPromptOptions, TOOL_NAME,
};
use tokio_util::sync::CancellationToken;

fn parent_model() -> Model {
    Model::minimal("mock-1", "mock", "mock")
}

fn child_model() -> Model {
    Model::minimal("child", "mock", "mock")
}

fn task_call(id: &str, args: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall {
        id: id.into(),
        name: TOOL_NAME.into(),
        arguments: args,
    }
}

struct StubTool {
    name: &'static str,
    calls: AtomicU32,
}

#[async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        self.name
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}})
    }
    async fn execute(
        &self,
        _call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("stub ran"))
    }
}

struct NullUpdater;
#[async_trait]
impl ToolUpdater for NullUpdater {
    async fn update(&self, _partial: String) {}
}

/// 测试装配:父会话(脚本化 turns)+ task 工具(子 provider 脚本化)。
struct Fixture {
    session: Arc<latent_core::AgentSession>,
    task: Arc<SubagentTool>,
}

async fn fixture(
    parent_turns: Vec<ScriptedTurn>,
    child_turns: Vec<ScriptedTurn>,
    extra_tools: Vec<Arc<dyn Tool>>,
    mode: SessionMode,
) -> Fixture {
    let parent_provider = Arc::new(ScriptedProvider::new(&parent_model(), parent_turns));
    let child_provider = Arc::new(ScriptedProvider::new(&child_model(), child_turns));
    let parent_cell: Arc<Mutex<Weak<Agent>>> = Arc::new(Mutex::new(Weak::new()));
    let engine = Arc::new(PermissionEngine::new(
        mode,
        SandboxConfig::default(),
        ApprovalRules::default(),
        std::env::temp_dir(),
        true,
    ));
    let task = Arc::new(SubagentTool::new(SubagentDeps {
        provider: child_provider,
        hooks: Arc::new(PassthroughHooks),
        engine: engine.clone(),
        subscribers: Arc::new(Mutex::new(Vec::new())),
        default_tools: vec![],
        tool_pool: extra_tools.clone(),
        resolve_model: Arc::new(|spec: &str| {
            if spec == "mock/child" {
                Ok(child_model())
            } else {
                Err(format!("unknown model: {spec}"))
            }
        }),
        parent: parent_cell.clone(),
        async_approval: HeadlessApproval::Deny,
        cwd: std::env::temp_dir(),
        latent_dir: None,
        agent_defs: Vec::new(),
        child_store_factory: None,
    }));

    let mut tools: Vec<Arc<dyn Tool>> = vec![task.clone()];
    tools.extend(extra_tools);
    let session = Arc::new(
        create_agent_session(AgentSessionConfig {
            provider: parent_provider,
            model: parent_model(),
            hooks: Arc::new(PassthroughHooks),
            ui: Arc::new(NoopUi),
            extensions: ExtensionRegistry::default(),
            tools,
            active_tool_names: None,
            system_prompt: SystemPromptOptions::default(),
            limits: Default::default(),
            stream_options: Default::default(),
            session_sink: None,
            seed_messages: vec![],
            compactor: None,
            subscribers: None,
            permission: Some(engine),
        })
        .await
        .unwrap(),
    );
    *parent_cell.lock().unwrap() = Arc::downgrade(session.agent());
    task.spawn_supervisor();
    let _ = parent_cell; // cell 已回填进 deps,句柄仅供构造
    Fixture { session, task }
}

fn last_tool_result(agent: &Agent) -> Option<String> {
    agent
        .messages()
        .iter()
        .rev()
        .find_map(|message| message.tool_result_content())
}

fn assistant_texts(agent: &Agent) -> Vec<String> {
    agent
        .messages()
        .iter()
        .filter_map(|message| message.as_assistant().map(|a| a.text_content()))
        .collect()
}

#[tokio::test]
async fn child_run_persists_session_file_with_run_id_tag() {
    // 子会话落盘:JSONL 条目格式与主会话一致,文件名带 run id tag,
    // 且不被 --continue 的 find_latest 选中
    let dir = std::env::temp_dir().join(format!("latent-sub-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 主会话文件(对照:find_latest 应选它而非子会话文件)
    let main_manager = latent_session::create_session_in_dir(&dir, "/tmp/proj", None, None).unwrap();

    let child_provider = Arc::new(ScriptedProvider::new(
        &child_model(),
        vec![ScriptedTurn::text(&child_model(), "child output")],
    ));
    let parent_cell: Arc<Mutex<Weak<Agent>>> = Arc::new(Mutex::new(Weak::new()));
    let store_dir = dir.clone();
    let task = Arc::new(SubagentTool::new(SubagentDeps {
        provider: child_provider,
        hooks: Arc::new(PassthroughHooks),
        engine: Arc::new(PermissionEngine::new(
            SessionMode::FullAccess,
            SandboxConfig::default(),
            ApprovalRules::default(),
            std::env::temp_dir(),
            true,
        )),
        subscribers: Arc::new(Mutex::new(Vec::new())),
        default_tools: vec![],
        tool_pool: vec![],
        resolve_model: Arc::new(|spec: &str| {
            if spec == "mock/child" {
                Ok(child_model())
            } else {
                Err(format!("unknown model: {spec}"))
            }
        }),
        parent: parent_cell.clone(),
        async_approval: HeadlessApproval::Deny,
        cwd: std::env::temp_dir(),
        latent_dir: None,
        agent_defs: Vec::new(),
        child_store_factory: Some(Arc::new(move |tag: &str| {
            let manager = latent_session::create_session_in_dir(&store_dir, "/tmp/proj", None, Some(tag))
                .map_err(|e| e.to_string())?;
            let sink: Arc<dyn latent_core::SessionSink> = Arc::new(ManagerSink(manager.into()));
            Ok(latent_core::ChildStore {
                sink,
                stream_options: Default::default(),
            })
        })),
    }));

    let outcome = task
        .execute(
            ToolCall {
                id: "t1".into(),
                name: TOOL_NAME.into(),
                args: serde_json::json!({"task": "PERSIST MARKER", "systemPrompt": "s", "model": "mock/child"}),
            },
            CancellationToken::new(),
            &NullUpdater,
        )
        .await
        .unwrap();
    let run_id = outcome.details["runId"].as_str().unwrap().to_string();

    // 子会话文件存在,内容含任务文本(与主会话条目格式一致);文件在
    // `<dir>/<项目目录>/` 下(含旧版式根目录兼容扫描)
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            for sub in std::fs::read_dir(&path).unwrap().flatten() {
                if sub.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    files.push(sub.path());
                }
            }
        } else if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    assert_eq!(files.len(), 2, "{files:?}");
    let child_file = files
        .iter()
        .find(|p| p.file_name().unwrap().to_string_lossy().contains(&format!("__{run_id}__")))
        .expect("应有带 run id tag 的子会话文件");
    let content = std::fs::read_to_string(child_file).unwrap();
    assert!(content.contains("PERSIST MARKER"), "子会话 JSONL 应含任务文本");
    assert!(content.contains("child output"), "子会话 JSONL 应含子 agent 回复");
    // --continue 选取主会话文件,不选子会话
    let latest =
        latent_session::find_latest_session_file(&dir, Some("/tmp/proj")).unwrap();
    assert_eq!(latest, main_manager.file_path().unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试用 sink 适配器:直接把消息写进 manager(与 assembly 的
/// SessionManagerSink 同语义;append 走 append_message,条目格式一致)。
struct ManagerSink(Arc<latent_session::SessionManager>);

#[async_trait]
impl latent_core::SessionSink for ManagerSink {
    async fn append(&self, message: &latent_agent::AgentMessage) -> Result<(), String> {
        self.0
            .append_message(message.clone())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[tokio::test]
async fn sync_nested_run_delivers_child_output_to_parent() {
    let fx = fixture(
        vec![
            ScriptedTurn::tool_calls(
                &parent_model(),
                vec![task_call(
                    "t1",
                    serde_json::json!({"task": "summarize", "systemPrompt": "be terse", "model": "mock/child"}),
                )],
            ),
            ScriptedTurn::text(&parent_model(), "parent done"),
        ],
        vec![ScriptedTurn::text(&child_model(), "child output")],
        vec![],
        SessionMode::FullAccess,
    )
    .await;
    fx.session.prompt("go").await.unwrap();
    let result = last_tool_result(fx.session.agent()).unwrap();
    assert!(
        result.contains("Subagent inline completed"),
        "工具结果应含状态行: {result}"
    );
    assert!(result.contains("child output"), "应含子 agent 最终输出: {result}");
    assert_eq!(assistant_texts(fx.session.agent()).last().unwrap(), "parent done");
}

#[tokio::test]
async fn child_toolset_lacks_task_tool_recursion_guard() {
    // 子 agent 尝试再调 task:工具面裁剪后该调用未找到 → 错误 tool result,
    // 子 agent 继续收尾;父侧仍拿到子的最终输出
    let fx = fixture(
        vec![
            ScriptedTurn::tool_calls(
                &parent_model(),
                vec![task_call(
                    "t1",
                    serde_json::json!({"task": "go deeper", "systemPrompt": "s", "model": "mock/child"}),
                )],
            ),
            ScriptedTurn::text(&parent_model(), "parent done"),
        ],
        vec![
            ScriptedTurn::tool_calls(
                &child_model(),
                vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: TOOL_NAME.into(),
                    arguments: serde_json::json!({"task": "recurse", "systemPrompt": "s"}),
                }],
            ),
            ScriptedTurn::text(&child_model(), "gave up"),
        ],
        vec![],
        SessionMode::FullAccess,
    )
    .await;
    fx.session.prompt("go").await.unwrap();
    let result = last_tool_result(fx.session.agent()).unwrap();
    assert!(result.contains("gave up"), "{result}");
}

#[tokio::test]
async fn timeout_aborts_hung_child() {
    let fx = fixture(
        vec![],
        vec![ScriptedTurn::text(&child_model(), "late").with_delay(10_000)],
        vec![],
        SessionMode::FullAccess,
    )
    .await;
    let error = fx
        .task
        .execute(
            ToolCall {
                id: "t1".into(),
                name: TOOL_NAME.into(),
                args: serde_json::json!({
                    "task": "hang",
                    "systemPrompt": "s",
                    "model": "mock/child",
                    "timeoutMs": 100
                }),
            },
            CancellationToken::new(),
            &NullUpdater,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(fx.task.registry().list().contains("timed out"));
}

#[tokio::test]
async fn six_parallel_sync_tasks_queue_beyond_limit_of_four() {
    // 6 个同步 task 同批:上限 4,其余排队;全部完成且输出逐一对应
    let mut parent_turns = vec![ScriptedTurn::tool_calls(
        &parent_model(),
        (1..=6)
            .map(|i| {
                task_call(
                    &format!("t{i}"),
                    serde_json::json!({"task": format!("job {i}"), "systemPrompt": "s", "model": "mock/child"}),
                )
            })
            .collect(),
    )];
    parent_turns.push(ScriptedTurn::text(&parent_model(), "all done"));
    let child_turns = (1..=6)
        .map(|i| ScriptedTurn::text(&child_model(), format!("output {i}")).with_delay(50))
        .collect();
    let fx = fixture(parent_turns, child_turns, vec![], SessionMode::FullAccess).await;
    fx.session.prompt("go").await.unwrap();
    let messages = fx.session.agent().messages();
    let results: Vec<String> = messages
        .iter()
        .filter_map(|message| message.tool_result_content())
        .collect();
    for i in 1..=6 {
        assert!(
            results.iter().any(|result| result.contains(&format!("output {i}"))),
            "output {i} 缺失: {results:?}"
        );
    }
}

#[tokio::test]
async fn async_run_wakes_parent_with_follow_up_notice() {
    let fx = fixture(
        vec![
            ScriptedTurn::tool_calls(
                &parent_model(),
                vec![task_call(
                    "t1",
                    serde_json::json!({"task": "long job", "systemPrompt": "s", "model": "mock/child", "async": true}),
                )],
            ),
            ScriptedTurn::text(&parent_model(), "run1 done"),
            ScriptedTurn::text(&parent_model(), "woken by subagent"),
        ],
        vec![ScriptedTurn::text(&child_model(), "async result").with_delay(100)],
        vec![],
        SessionMode::FullAccess,
    )
    .await;
    fx.session.prompt("go").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if assistant_texts(fx.session.agent())
            .iter()
            .any(|text| text.contains("woken by subagent"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "supervisor 未在期限内唤醒父会话: {:?}",
            assistant_texts(fx.session.agent())
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let notice = fx
        .session
        .agent()
        .messages()
        .iter()
        .find_map(|message| match message {
            latent_agent::AgentMessage::User { content, .. } if content.contains("Subagent inline completed") => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("应有 follow_up 通知消息");
    assert!(notice.contains("async result"), "{notice}");
    assert!(notice.contains("Run id:"), "{notice}");
}

#[tokio::test]
async fn list_and_stop_manage_background_runs() {
    let fx = fixture(
        vec![],
        vec![ScriptedTurn::text(&child_model(), "slow").with_delay(10_000)],
        vec![],
        SessionMode::FullAccess,
    )
    .await;
    let output = fx
        .task
        .execute(
            ToolCall {
                id: "t1".into(),
                name: TOOL_NAME.into(),
                args: serde_json::json!({
                    "task": "background",
                    "systemPrompt": "s",
                    "model": "mock/child",
                    "async": true
                }),
            },
            CancellationToken::new(),
            &NullUpdater,
        )
        .await
        .unwrap();
    let run_id = output.details["runId"].as_str().unwrap().to_string();
    assert!(output.output.contains("Background subagent"));

    let list = fx.task.execute(
        ToolCall {
            id: "t2".into(),
            name: TOOL_NAME.into(),
            args: serde_json::json!({"action": "list"}),
        },
        CancellationToken::new(),
        &NullUpdater,
    ).await.unwrap();
    assert!(list.output.contains("running"), "{}", list.output);

    let stopped = fx.task.execute(
        ToolCall {
            id: "t3".into(),
            name: TOOL_NAME.into(),
            args: serde_json::json!({"action": "stop", "id": run_id}),
        },
        CancellationToken::new(),
        &NullUpdater,
    ).await.unwrap();
    assert!(stopped.output.contains("already stopped"), "{}", stopped.output);
    assert!(fx.task.registry().list().contains("stopped"));
}

#[tokio::test]
async fn background_child_approval_fails_closed_deny() {
    // Confirm 模式 + 后台运行:子 agent 调 bash → Ask → HeadlessApprovalUi(Deny)
    // → 拦截;工具绝不应真正执行
    let bash = Arc::new(StubTool {
        name: "bash",
        calls: AtomicU32::new(0),
    });
    let fx = fixture(
        vec![
            ScriptedTurn::tool_calls(
                &parent_model(),
                vec![task_call(
                    "t1",
                    serde_json::json!({"task": "try bash", "systemPrompt": "s", "model": "mock/child", "tools": ["bash"], "async": true}),
                )],
            ),
            ScriptedTurn::text(&parent_model(), "parent done"),
            ScriptedTurn::text(&parent_model(), "noted"),
        ],
        vec![
            ScriptedTurn::tool_calls(
                &child_model(),
                vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "rm -rf /"}),
                }],
            ),
            ScriptedTurn::text(&child_model(), "bash was blocked"),
        ],
        vec![bash.clone()],
        SessionMode::Confirm,
    )
    .await;
    fx.session.prompt("go").await.unwrap();
    // 异步路径:子 agent 的结果经 supervisor 以 follow_up 通知(user 消息)送达
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let notice = loop {
        let found = fx.session.agent().messages().iter().find_map(|message| match message {
            latent_agent::AgentMessage::User { content, .. }
                if content.contains("Subagent inline") =>
            {
                Some(content.clone())
            }
            _ => None,
        });
        if let Some(notice) = found {
            break notice;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "supervisor 未在期限内投递通知: {:?}",
            fx.session.agent().messages()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(notice.contains("bash was blocked"), "{notice}");
    assert_eq!(bash.calls.load(Ordering::SeqCst), 0, "后台 Deny:bash 不应执行");
}

#[tokio::test]
async fn unknown_agent_name_lists_available() {
    let fx = fixture(vec![], vec![], vec![], SessionMode::FullAccess).await;
    let error = fx
        .task
        .execute(
            ToolCall {
                id: "t1".into(),
                name: TOOL_NAME.into(),
                args: serde_json::json!({"task": "t", "agent": "nope", "model": "mock/child"}),
            },
            CancellationToken::new(),
            &NullUpdater,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("available agents"), "{error}");
}
