//! `task` 工具(14 文档 §4.1):进程内 subagent 引擎的唯一工具面。
//!
//! 依赖全部经 `SubagentDeps` 装配期注入(09 接缝纪律:工具不读全局/配置)。
//! 递归防护 = 子工具面裁剪(白名单解析层再防御性过滤 `task` 本身);
//! 同步并发上限 4(超出排队),后台活跃上限 16(超出报错)。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use latent_agent::{
    Agent, LoopHooks, PassthroughHooks, Tool, ToolCall, ToolError, ToolExecution, ToolOutput,
    ToolUpdater,
};
use latent_ai::Model;

use crate::permission::{
    ApprovalHooks, HeadlessApproval, HeadlessApprovalUi, PermissionEngine,
};
use crate::session::SessionSharedSubscriber;

use super::defs::{discover_agent_defs, AgentDef};
use super::registry::{SubagentRegistry, MAX_ACTIVE_ASYNC};
use super::store::ChildStoreFactory;
use super::runner::{
    format_child_result, run_child, ChildSpec, RunGuard, RunStatus,
};

/// `provider/model` spec 解析闭包(装配层包 ModelResolver)。
pub type ModelResolveFn = Arc<dyn Fn(&str) -> Result<Model, String> + Send + Sync>;

pub const TOOL_NAME: &str = "subagent";
/// 默认超时:30 分钟。
pub const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// 同步运行并发上限(14 文档 §4.1:超出排队)。
pub const MAX_SYNC_CONCURRENCY: usize = 4;

/// 装配期依赖(工厂注入,工具不读全局)。
pub struct SubagentDeps {
    /// 子 agent 的 provider(与父同一条,含重试装饰)
    pub provider: Arc<dyn latent_ai::Provider>,
    /// 同步子 agent 的 hooks:父审批洋葱原样传入 → 权限继承自动生效(14 §3.1)
    pub hooks: Arc<dyn LoopHooks>,
    /// 后台子 agent 审批的引擎(共享会话级缓存)
    pub engine: Arc<PermissionEngine>,
    /// 审批事件广播列表(后台 ApprovalHooks 复用)
    pub subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>>,
    /// 白名单缺省集(装配层注入的只读集)
    pub default_tools: Vec<Arc<dyn Tool>>,
    /// 白名单候选池(内置 8 工具 + 扩展工具)
    pub tool_pool: Vec<Arc<dyn Tool>>,
    /// `provider/model` spec → Model(装配层包 ModelResolver)
    pub resolve_model: ModelResolveFn,
    /// 父会话 Agent 弱引(装配后回填;继承模型 + supervisor 唤醒)
    pub parent: Arc<Mutex<Weak<Agent>>>,
    /// 后台运行审批策略(settings `subagentAsyncApproval`,默认 Deny)
    pub async_approval: HeadlessApproval,
    /// agent 定义发现目录(§4.4;每次 `agent` 查询重新发现,保持数据化)
    pub cwd: PathBuf,
    /// 用户数据目录(入口层经 `crate::paths::latent_dir` 解析后注入;
    /// agent 定义的用户级来源 `<数据目录>/agents/*.md`)
    pub latent_dir: Option<PathBuf>,
    /// 装配期发现的定义快照(prompt snippet 目录用;诊断由装配方打印)
    pub agent_defs: Vec<AgentDef>,
    /// 子会话落盘工厂(可选;tag = run id,JSONL 格式与主会话一致)
    pub child_store_factory: Option<ChildStoreFactory>,
}

pub struct SubagentTool {
    deps: SubagentDeps,
    registry: Arc<SubagentRegistry>,
    sync_permits: Arc<Semaphore>,
    /// 后台子 agent 的 hooks:独立 ApprovalHooks,UI 为策略型(默认 Deny,
    /// 不弹前台审批 —— 避免与主会话 UI 并发冲突)
    async_hooks: Arc<dyn LoopHooks>,
    /// 构造时发现的定义快照(prompt snippet 目录用;每次调用重新发现)
    known_agents: Vec<AgentDef>,
}

impl SubagentTool {
    pub fn new(deps: SubagentDeps) -> Self {
        let known_agents = deps.agent_defs.clone();
        let async_hooks: Arc<dyn LoopHooks> = Arc::new(ApprovalHooks::new(
            Arc::new(PassthroughHooks),
            deps.engine.clone(),
            Arc::new(HeadlessApprovalUi {
                policy: deps.async_approval,
            }),
            deps.subscribers.clone(),
        ));
        let registry = SubagentRegistry::new(deps.parent.clone());
        SubagentTool {
            deps,
            registry,
            sync_permits: Arc::new(Semaphore::new(MAX_SYNC_CONCURRENCY)),
            async_hooks,
            known_agents,
        }
    }

    /// supervisor 空闲唤醒任务(装配后调用一次;重复调用 no-op)。
    pub fn spawn_supervisor(&self) {
        self.registry.spawn_supervisor();
    }

    pub fn registry(&self) -> Arc<SubagentRegistry> {
        self.registry.clone()
    }

    fn fail(message: String) -> ToolError {
        ToolError::Failed {
            name: TOOL_NAME.into(),
            message,
        }
    }

    fn parent_model(&self) -> Option<Model> {
        let parent = self.deps.parent.lock().unwrap().upgrade()?;
        parent.state_snapshot().model
    }

    fn agent_catalog(&self) -> String {
        if self.known_agents.is_empty() {
            return "none defined yet; inline `systemPrompt` works without any".to_string();
        }
        self.known_agents
            .iter()
            .map(|def| {
                if def.description.is_empty() {
                    def.name.clone()
                } else {
                    format!("{} ({})", def.name, def.description)
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// 白名单解析:params.tools ?? agent 定义 tools;名字必须落在候选池,
    /// `task` 防御性剔除(递归防护的第二道保险)。
    fn resolve_whitelist(
        &self,
        names: &[String],
    ) -> Result<Vec<Arc<dyn Tool>>, ToolError> {
        if names.is_empty() {
            return Err(Self::fail("`tools` must not be empty".into()));
        }
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for name in names {
            if name == TOOL_NAME {
                continue; // 单层嵌套:子 agent 永远拿不到本工具
            }
            match self.deps.tool_pool.iter().find(|tool| tool.name() == name) {
                Some(tool) => {
                    if !tools.iter().any(|t| t.name() == tool.name()) {
                        tools.push(tool.clone());
                    }
                }
                None => {
                    let available: Vec<&str> =
                        self.deps.tool_pool.iter().map(|tool| tool.name()).collect();
                    return Err(Self::fail(format!(
                        "unknown tool `{name}` in whitelist; available: {available:?}"
                    )));
                }
            }
        }
        if tools.is_empty() {
            return Err(Self::fail(
                "whitelist resolved to an empty tool set".into(),
            ));
        }
        Ok(tools)
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Delegate a self-contained task to a sub-agent that runs in-process with its own \
         context window and returns its final answer as the tool result. Pick a named agent \
         definition with `agent` or pass an inline `systemPrompt`. Set `async: true` to run \
         in the background: you get a run id immediately and the result is delivered as a \
         follow-up message when it settles. Manage background runs with action \"list\" / \"stop\"."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "Self-contained task text (required unless `action` is set)"},
                "agent": {"type": "string", "description": "Name of a discovered agent definition (mutually exclusive with systemPrompt)"},
                "systemPrompt": {"type": "string", "description": "Inline system prompt (mutually exclusive with agent)"},
                "model": {"type": "string", "description": "Model override as `provider/model`; defaults to the agent definition's model, else the parent session's"},
                "tools": {"type": "array", "items": {"type": "string"}, "description": "Tool whitelist; defaults to the agent definition's tools, else the read-only set"},
                "async": {"type": "boolean", "description": "Run in the background and return a run id immediately (default false)"},
                "timeoutMs": {"type": "number", "description": "Abort the sub-agent after this many milliseconds (default 1800000)"},
                "action": {"type": "string", "enum": ["list", "stop"], "description": "Manage background runs instead of starting one"},
                "id": {"type": "string", "description": "Run id (or unique id prefix) targeted by action \"stop\""}
            },
            "additionalProperties": false
        })
    }

    fn execution_mode(&self) -> Option<ToolExecution> {
        Some(ToolExecution::Parallel)
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(format!(
            "subagent(...): delegate a self-contained task to a sub-agent (own context window, \
             single-level nesting); it always works: pass an inline `systemPrompt`, or a named \
             `agent` (available: {}); background runs via async:true with \
             action:\"list\"/\"stop\"",
            self.agent_catalog()
        ))
    }


    async fn execute(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let Some(args) = call.args.as_object() else {
            return Err(Self::fail("arguments must be an object".into()));
        };

        // 管理动作(不需要 task)
        match args.get("action").and_then(|value| value.as_str()) {
            Some("list") => return Ok(ToolOutput::text(self.registry.list())),
            Some("stop") => {
                let id = args
                    .get("id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| Self::fail("action:\"stop\" requires `id`".into()))?;
                let text = self
                    .registry
                    .stop(id)
                    .await
                    .map_err(Self::fail)?;
                return Ok(ToolOutput::text(text));
            }
            Some(other) => {
                return Err(Self::fail(format!(
                    "unknown action `{other}` (expected \"list\" or \"stop\")"
                )))
            }
            None => {}
        }

        // 参数校验
        let task = args
            .get("task")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .ok_or_else(|| Self::fail("`task` is required and must be non-empty".into()))?;
        let agent_name = args.get("agent").and_then(|value| value.as_str());
        let inline_prompt = args.get("systemPrompt").and_then(|value| value.as_str());
        let (display_name, system_prompt, def_model, def_tools) = match (agent_name, inline_prompt)
        {
            (Some(_), Some(_)) => {
                return Err(Self::fail(
                    "pass either `agent` or `systemPrompt`, not both".into(),
                ))
            }
            (Some(name), None) => {
                let (defs, _) = discover_agent_defs(&self.deps.cwd, self.deps.latent_dir.as_deref());
                let def = defs.iter().find(|def| def.name == name).ok_or_else(|| {
                    let available: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
                    Self::fail(format!(
                        "unknown agent `{name}`; available agents: {available:?} (defined in \
                         .latent/agents/*.md and $LATENT_HOME/agents/*.md, default \
                         ~/.config/latent/agents)"
                    ))
                })?;
                (
                    def.name.clone(),
                    def.system_prompt.clone(),
                    def.model.clone(),
                    def.tools.clone(),
                )
            }
            (None, Some(prompt)) => {
                if prompt.trim().is_empty() {
                    return Err(Self::fail("`systemPrompt` must be non-empty".into()));
                }
                ("inline".to_string(), prompt.to_string(), None, None)
            }
            (None, None) => {
                return Err(Self::fail(
                    "one of `agent` or `systemPrompt` is required".into(),
                ))
            }
        };

        // 模型解析:显式参数 > agent 定义 > 继承父会话
        let model = if let Some(spec) = args.get("model").and_then(|value| value.as_str()) {
            (self.deps.resolve_model)(spec).map_err(Self::fail)?
        } else if let Some(spec) = def_model {
            (self.deps.resolve_model)(&spec).map_err(Self::fail)?
        } else {
            self.parent_model().ok_or_else(|| {
                Self::fail(
                    "cannot inherit the parent session's model; pass `model` explicitly".into(),
                )
            })?
        };

        // 工具白名单:参数 > agent 定义 > 缺省只读集
        let tools = if let Some(names) = parse_string_list(args.get("tools")) {
            self.resolve_whitelist(&names)?
        } else if let Some(names) = def_tools {
            self.resolve_whitelist(&names)?
        } else {
            self.deps.default_tools.clone()
        };

        let timeout_ms = match args.get("timeoutMs") {
            None => DEFAULT_TIMEOUT_MS,
            Some(value) => {
                let ms = value.as_u64().or_else(|| {
                    value.as_f64().map(|f| f as u64)
                });
                match ms {
                    Some(ms) if ms >= 1 => ms,
                    _ => {
                        return Err(Self::fail("`timeoutMs` must be a number >= 1".into()));
                    }
                }
            }
        };
        let is_async = args
            .get("async")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        let mut spec = ChildSpec {
            name: display_name.clone(),
            task: task.to_string(),
            system_prompt,
            model: model.clone(),
            thinking: None, // 继承由父 hooks prepare_request 决定;子不显式请求思考
            tools,
            persistence: None,
        };

        if is_async {
            if self.registry.active_background() >= MAX_ACTIVE_ASYNC {
                return Err(Self::fail(format!(
                    "too many active background runs (max {MAX_ACTIVE_ASYNC}); \
                     stop one with action:\"stop\" first"
                )));
            }
            let stop = CancellationToken::new();
            let run_id = self.registry.register(&display_name, task, true, stop.clone());
            if let Some(factory) = &self.deps.child_store_factory {
                spec.persistence = Some(factory(&run_id).map_err(Self::fail)?);
            }
            let guard = RunGuard {
                parent_cancel: cancel.child_token(),
                stop,
                timeout: Duration::from_millis(timeout_ms),
            };
            let registry = self.registry.clone();
            let provider = self.deps.provider.clone();
            let hooks = self.async_hooks.clone();
            let id_for_task = run_id.clone();
            tokio::spawn(async move {
                let outcome = run_child(provider, hooks, spec, guard, None).await;
                registry.finish(&id_for_task, &outcome);
            });
            return Ok(ToolOutput {
                output: format!(
                    "Background subagent `{display_name}` started.\nRun id: {run_id}\n\
                     Manage with action:\"list\" / action:\"stop\"; the result will be \
                     delivered as a follow-up message when the run settles."
                ),
                details: serde_json::json!({
                    "runId": run_id,
                    "agent": display_name,
                    "status": "running",
                    "model": model.id,
                }),
                terminate: false,
            });
        }

        // 同步路径:并发上限 4,超出在槽位上排队
        let permit = self
            .sync_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Self::fail("sync semaphore closed".into()))?;
        let stop = CancellationToken::new();
        let run_id = self.registry.register(&display_name, task, false, stop.clone());
        if let Some(factory) = &self.deps.child_store_factory {
            spec.persistence = Some(factory(&run_id).map_err(Self::fail)?);
        }
        let guard = RunGuard {
            parent_cancel: cancel.child_token(),
            stop,
            timeout: Duration::from_millis(timeout_ms),
        };
        let outcome = run_child(
            self.deps.provider.clone(),
            self.deps.hooks.clone(),
            spec,
            guard,
            Some(updater),
        )
        .await;
        drop(permit);
        self.registry.finish(&run_id, &outcome);

        if cancel.is_cancelled() {
            // 父 run 被 abort(Esc/Ctrl-C):按工具中止契约返回
            return Err(ToolError::Aborted {
                name: TOOL_NAME.into(),
            });
        }
        let details = serde_json::json!({
            "runId": run_id,
            "agent": display_name,
            "status": outcome.status.as_str(),
            "durationMs": outcome.duration.as_millis() as u64,
            "model": outcome.model_id,
        });
        match outcome.status {
            RunStatus::Completed => Ok(ToolOutput {
                output: format_child_result(&display_name, &run_id, &outcome),
                details,
                terminate: false,
            }),
            RunStatus::Stopped | RunStatus::TimedOut | RunStatus::Failed => {
                Err(Self::fail(format_child_result(&display_name, &run_id, &outcome)))
            }
        }
    }
}

/// 字符串数组参数(dash 数组形态;CSV 由 agent 定义层负责)。
fn parse_string_list(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{ApprovalRules, SandboxConfig, SessionMode};
    use latent_agent::PassthroughHooks;
    use std::path::Path;

    fn deps(cwd: &Path) -> SubagentDeps {
        SubagentDeps {
            provider: Arc::new(latent_ai::MockProvider::new("ok")),
            hooks: Arc::new(PassthroughHooks),
            engine: Arc::new(PermissionEngine::new(
                SessionMode::FullAccess,
                SandboxConfig::default(),
                ApprovalRules::default(),
                cwd.to_path_buf(),
                true,
            )),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            default_tools: vec![],
            tool_pool: vec![],
            resolve_model: Arc::new(|spec: &str| {
                if spec == "mock/m1" {
                    Ok(Model::minimal("m1", "mock", "mock"))
                } else {
                    Err(format!("unknown model: {spec}"))
                }
            }),
            parent: Arc::new(Mutex::new(Weak::new())),
            async_approval: HeadlessApproval::Deny,
            cwd: cwd.to_path_buf(),
            latent_dir: None,
            agent_defs: Vec::new(),
            child_store_factory: None,
        }
    }

    fn tool_with_pool(cwd: &Path) -> SubagentTool {
        let mut deps = deps(cwd);
        let pool: Vec<Arc<dyn Tool>> = vec![
            Arc::new(StubTool("read")),
            Arc::new(StubTool("bash")),
            Arc::new(StubTool(TOOL_NAME)),
        ];
        deps.tool_pool = pool;
        deps.default_tools = vec![Arc::new(StubTool("read"))];
        SubagentTool::new(deps)
    }

    struct StubTool(&'static str);
    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.0
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _call: ToolCall,
            _cancel: CancellationToken,
            _updater: &dyn ToolUpdater,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("stub"))
        }
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: TOOL_NAME.into(),
            args,
        }
    }

    struct NullUpdater;
    #[async_trait]
    impl ToolUpdater for NullUpdater {
        async fn update(&self, _partial: String) {}
    }

    #[tokio::test]
    async fn list_action_works_without_task() {
        let tool = tool_with_pool(Path::new("."));
        let output = tool
            .execute(call(serde_json::json!({"action": "list"})), CancellationToken::new(), &NullUpdater)
            .await
            .unwrap();
        assert!(output.output.contains("No subagent runs yet."));
    }

    #[tokio::test]
    async fn validation_errors() {
        let tool = tool_with_pool(Path::new("."));
        let run = |args| {
            let tool = &tool;
            async move {
                tool.execute(call(args), CancellationToken::new(), &NullUpdater).await
            }
        };
        // 缺 task
        assert!(run(serde_json::json!({"systemPrompt": "x"})).await.is_err());
        // agent + systemPrompt 同时给
        assert!(run(serde_json::json!({"task": "t", "agent": "a", "systemPrompt": "s"})).await.is_err());
        // 未知 agent
        let error = run(serde_json::json!({"task": "t", "agent": "nope"})).await.unwrap_err();
        assert!(error.to_string().contains("unknown agent"));
        // 未知模型
        let error = run(serde_json::json!({"task": "t", "systemPrompt": "s", "model": "bad/x"})).await.unwrap_err();
        assert!(error.to_string().contains("unknown model"));
        // 未知白名单工具
        let error = run(serde_json::json!({"task": "t", "systemPrompt": "s", "model": "mock/m1", "tools": ["nope"]})).await.unwrap_err();
        assert!(error.to_string().contains("unknown tool"));
        // 未知 action
        let error = run(serde_json::json!({"action": "pause"})).await.unwrap_err();
        assert!(error.to_string().contains("unknown action"));
        // stop 缺 id
        let error = run(serde_json::json!({"action": "stop"})).await.unwrap_err();
        assert!(error.to_string().contains("requires `id`"));
        // timeoutMs 非法
        let error = run(serde_json::json!({"task": "t", "systemPrompt": "s", "model": "mock/m1", "timeoutMs": 0})).await.unwrap_err();
        assert!(error.to_string().contains("timeoutMs"));
    }

    #[tokio::test]
    async fn whitelist_filters_task_tool() {
        let tool = tool_with_pool(Path::new("."));
        // 只给 subagent 自己(防递归过滤后):解析为空 → 报错而不是把本工具装进子 agent
        let error = tool
            .execute(
                call(serde_json::json!({"task": "t", "systemPrompt": "s", "model": "mock/m1", "tools": [TOOL_NAME]})),
                CancellationToken::new(),
                &NullUpdater,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("empty tool set"));
    }

    #[tokio::test]
    async fn sync_run_returns_subagent_output() {
        let tool = tool_with_pool(Path::new("."));
        // 无父会话且 model 显式给出;provider 是 MockProvider,canned 回复
        let output = tool
            .execute(
                call(serde_json::json!({"task": "do", "systemPrompt": "s", "model": "mock/m1"})),
                CancellationToken::new(),
                &NullUpdater,
            )
            .await
            .unwrap();
        assert!(output.output.contains("Subagent inline completed"), "{}", output.output);
        assert_eq!(output.details["status"], "completed");
        assert!(output.details["runId"].as_str().is_some());
    }
}
