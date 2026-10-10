//! 后台 shell 任务登记表与 task_status 工具:bash 长命令自动转后台后登记
//! 于 registry,模型经 task_status 三个动作跟进 —— `status`(查询是否
//! 完成)、`result`(取已完成任务的结果,取到即消费完成通知)、`kill`
//! (终止任务)。完成通知的投递闸门在 `AgentFollowUpNotifier`:模型已
//! 主动获知结局(result 取到结果 / kill)的任务不再重复投递,检查点在
//! follow_up 入队之前(已入队的通知不撤回,但该路径推送只发生一次)。
//!
//! registry 由装配层创建,主会话与 subagent 工具池共享同一实例;task id
//! 由本表单调分配(`bash-{n}`),跨会话不撞号。表里只有 latent 自身 bash
//! 转后台的任务(跨会话可见,task_status 的 kill 因此只及这些任务,不及
//! 系统其他进程);条目随会话存续不驱逐(含输出缓冲与临时文件路径,
//! 量级 = 转后台次数,可忽略)。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use latent_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::bash::BackgroundNotifier;
use crate::output_accumulator::OutputAccumulator;
use crate::truncate::truncate_tail;

/// kill 后等待进程退出的上限:watcher 负责杀树收尸,这里只轮询状态落定。
const KILL_WAIT: Duration = Duration::from_secs(2);

/// 任务结果尾段预算(与完成通知的输出尾段同规格:40 行 / 2000 字符)。
const TAIL_LINES: usize = 40;
const TAIL_BYTES: usize = 2000;

/// 列表/详情里命令的展示上限(转多行/超长时截断,完整命令以模型所发为准)。
const COMMAND_DISPLAY_CHARS: usize = 120;

/// 单个后台任务的可查询状态。
#[derive(Debug, Clone)]
pub enum TaskState {
    Running,
    Finished {
        exit_code: Result<i32, String>,
        elapsed_secs: u64,
        killed: bool,
    },
}

struct TaskEntry {
    command: String,
    started: Instant,
    output_path: Option<PathBuf>,
    accumulator: Arc<Mutex<OutputAccumulator>>,
    /// kill 接缝:取消即通知 watcher 杀树收尸(watcher 独占 child)
    kill: CancellationToken,
    kill_requested: AtomicBool,
    /// 模型已主动获知结局(result 取到结果 / kill)→ 完成通知不再投递
    consumed: AtomicBool,
    state: Mutex<TaskState>,
}

/// 任务只读视图(渲染进 task_status 的 tool result)。
pub struct TaskView {
    pub task_id: String,
    pub command: String,
    pub output_path: Option<PathBuf>,
    pub elapsed_secs: u64,
    pub state: TaskState,
}

/// kill 动作的结局。
#[derive(Debug, Clone)]
pub enum KillOutcome {
    /// 已在等待时限内退出
    Killed {
        exit_code: Result<i32, String>,
        elapsed_secs: u64,
    },
    /// 调用时已结束(无需 kill;结局同样已告知模型,消费完成通知)
    AlreadyFinished {
        exit_code: Result<i32, String>,
        elapsed_secs: u64,
        killed: bool,
    },
    /// 已发终止信号但 2s 内未退出(结局稍后可经 status/result 查询)
    Terminating,
}

/// 后台任务登记表:bash watcher 写入(转后台时登记、退出后回填终态),
/// task_status 工具读取,完成通知器据 consumed 抑制重复投递。
pub struct BackgroundTaskRegistry {
    inner: Mutex<HashMap<String, Arc<TaskEntry>>>,
    counter: AtomicU64,
}

impl Default for BackgroundTaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTaskRegistry {
    pub fn new() -> Self {
        BackgroundTaskRegistry {
            inner: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// 登记转后台任务,返回分配的 task id 与 kill 令牌(由 watcher 持有)。
    pub fn register(
        &self,
        command: &str,
        output_path: Option<PathBuf>,
        accumulator: Arc<Mutex<OutputAccumulator>>,
    ) -> (String, CancellationToken) {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let task_id = format!("bash-{n}");
        let kill = CancellationToken::new();
        self.inner.lock().unwrap().insert(
            task_id.clone(),
            Arc::new(TaskEntry {
                command: command.to_string(),
                started: Instant::now(),
                output_path,
                accumulator,
                kill: kill.clone(),
                kill_requested: AtomicBool::new(false),
                consumed: AtomicBool::new(false),
                state: Mutex::new(TaskState::Running),
            }),
        );
        (task_id, kill)
    }

    /// watcher 排空输出后回填终态;killed 取 watcher 观察与 kill 请求的或
    /// (kill 信号与自然退出竞速时,以模型请求为准)。
    pub fn mark_finished(&self, task_id: &str, exit_code: Result<i32, String>, saw_kill: bool) {
        let Some(entry) = self.inner.lock().unwrap().get(task_id).cloned() else {
            return;
        };
        let killed = saw_kill || entry.kill_requested.load(Ordering::SeqCst);
        let elapsed_secs = entry.started.elapsed().as_secs();
        *entry.state.lock().unwrap() = TaskState::Finished {
            exit_code,
            elapsed_secs,
            killed,
        };
    }

    /// 模型已主动获知该任务结局 → 完成通知不再投递(通知器在入队前检查)。
    pub(crate) fn mark_consumed(&self, task_id: &str) {
        if let Some(entry) = self.inner.lock().unwrap().get(task_id) {
            entry.consumed.store(true, Ordering::SeqCst);
        }
    }

    pub fn is_consumed(&self, task_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(task_id)
            .is_some_and(|entry| entry.consumed.load(Ordering::SeqCst))
    }

    pub fn get(&self, task_id: &str) -> Option<TaskView> {
        let entry = self.inner.lock().unwrap().get(task_id).cloned()?;
        Some(view_of(task_id, &entry))
    }

    /// 全部任务(按 task id 序)。
    pub fn list(&self) -> Vec<TaskView> {
        let inner = self.inner.lock().unwrap();
        let mut ids: Vec<&String> = inner.keys().collect();
        ids.sort_by_key(|id| id.rsplit('-').next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0));
        ids.into_iter()
            .map(|id| view_of(id, inner[id].as_ref()))
            .collect()
    }

    /// 当前输出尾段(转后台前后的输出都在 accumulator 里)。
    pub(crate) fn output_tail(&self, task_id: &str) -> String {
        let Some(entry) = self.inner.lock().unwrap().get(task_id).cloned() else {
            return String::new();
        };
        let snapshot = {
            let mut acc = entry.accumulator.lock().unwrap();
            acc.snapshot(false)
        };
        truncate_tail(&snapshot.content, TAIL_LINES, TAIL_BYTES).content
    }

    /// 终止任务:置位 kill_requested + consumed(kill = 模型主动获知结局)
    /// → 取消令牌(watcher 杀树收尸)→ 轮询等待状态落定(上限 2s)。
    /// Err = 未知任务。
    pub async fn kill(&self, task_id: &str) -> Result<KillOutcome, String> {
        let entry = self
            .inner
            .lock()
            .unwrap()
            .get(task_id)
            .cloned()
            .ok_or_else(|| format!("Unknown task: {task_id}"))?;
        // 已结束:无需 kill;结局随本动作告知模型 → 消费完成通知
        // (先判状态再取视图,锁不跨越 view_of —— 同一线程重入 state 锁会死锁)
        let already_finished = matches!(&*entry.state.lock().unwrap(), TaskState::Finished { .. });
        if already_finished {
            entry.consumed.store(true, Ordering::SeqCst);
            if let TaskState::Finished {
                exit_code,
                elapsed_secs,
                killed,
            } = view_of(task_id, &entry).state
            {
                return Ok(KillOutcome::AlreadyFinished {
                    exit_code,
                    elapsed_secs,
                    killed,
                });
            }
            unreachable!("state was just checked as Finished");
        }
        entry.kill_requested.store(true, Ordering::SeqCst);
        entry.consumed.store(true, Ordering::SeqCst);
        entry.kill.cancel();
        let deadline = Instant::now() + KILL_WAIT;
        loop {
            if let TaskState::Finished {
                exit_code,
                elapsed_secs,
                ..
            } = &*entry.state.lock().unwrap()
            {
                return Ok(KillOutcome::Killed {
                    exit_code: exit_code.clone(),
                    elapsed_secs: *elapsed_secs,
                });
            }
            if Instant::now() >= deadline {
                return Ok(KillOutcome::Terminating);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn view_of(task_id: &str, entry: &TaskEntry) -> TaskView {
    let state = entry.state.lock().unwrap().clone();
    let elapsed_secs = match &state {
        TaskState::Finished { elapsed_secs, .. } => *elapsed_secs,
        TaskState::Running => entry.started.elapsed().as_secs(),
    };
    TaskView {
        task_id: task_id.to_string(),
        command: entry.command.clone(),
        output_path: entry.output_path.clone(),
        elapsed_secs,
        state,
    }
}

// ---------------------------------------------------------------------------
// task_status 工具
// ---------------------------------------------------------------------------

/// task_status 工厂:与 bash 工具共享同一 registry(装配层注入)。
pub fn create_task_status_tool(registry: Arc<BackgroundTaskRegistry>) -> Arc<dyn Tool> {
    Arc::new(TaskStatusTool { registry })
}

struct TaskStatusTool {
    registry: Arc<BackgroundTaskRegistry>,
}

fn fail(message: String) -> ToolError {
    ToolError::Failed {
        name: "task_status".into(),
        message,
    }
}

fn output(text: String) -> ToolOutput {
    ToolOutput::text(text)
}

fn require_task_id(task_id: Option<&str>) -> Result<String, ToolError> {
    task_id
        .map(str::to_string)
        .ok_or_else(|| fail("missing required argument `task_id` for this action".into()))
}

fn unknown_task(registry: &BackgroundTaskRegistry, task_id: &str) -> ToolError {
    let known = registry.list();
    let message = if known.is_empty() {
        format!("Unknown task: {task_id}; no background tasks are registered.")
    } else {
        format!(
            "Unknown task: {task_id}; known tasks: {}",
            known
                .iter()
                .map(|view| view.task_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    fail(message)
}

/// 结局的人类可读描述(status 与 result 共用)。
fn describe_exit(state: &TaskState) -> String {
    match state {
        TaskState::Running => "running".to_string(),
        TaskState::Finished {
            killed: true,
            exit_code,
            ..
        } => match exit_code {
            Ok(code) => format!("killed (exit code {code})"),
            Err(error) => format!("killed (could not be reaped: {error})"),
        },
        TaskState::Finished {
            exit_code: Ok(0), ..
        } => "finished successfully".to_string(),
        TaskState::Finished {
            exit_code: Ok(code),
            ..
        } => format!("failed with exit code {code}"),
        TaskState::Finished {
            exit_code: Err(error),
            ..
        } => format!("could not be reaped ({error})"),
    }
}

/// 命令展示:取首行、超长截断。
fn display_command(command: &str) -> String {
    let first_line = command.lines().next().unwrap_or("");
    let mut text: String = first_line.chars().take(COMMAND_DISPLAY_CHARS).collect();
    if command.lines().count() > 1 || first_line.chars().count() > COMMAND_DISPLAY_CHARS {
        text.push_str(" …");
    }
    text
}

fn render_status(view: &TaskView) -> String {
    let mut line = format!(
        "Task {}: {} (elapsed {}s); command: {}",
        view.task_id,
        describe_exit(&view.state),
        view.elapsed_secs,
        display_command(&view.command),
    );
    if matches!(view.state, TaskState::Finished { .. }) {
        line.push_str("; use action \"result\" to fetch the output");
    }
    line
}

impl TaskStatusTool {
    fn status(&self, task_id: Option<&str>) -> Result<ToolOutput, ToolError> {
        match task_id {
            Some(id) => {
                let view = self
                    .registry
                    .get(id)
                    .ok_or_else(|| unknown_task(&self.registry, id))?;
                Ok(output(render_status(&view)))
            }
            None => {
                let views = self.registry.list();
                if views.is_empty() {
                    return Ok(output("No background tasks.".into()));
                }
                Ok(output(
                    views.iter().map(render_status).collect::<Vec<_>>().join("\n"),
                ))
            }
        }
    }

    /// 取结果:finished → 完整结果并消费完成通知;running → 告知仍在执行,
    /// 不消费(通知随后照常投递)。
    fn result(&self, task_id: &str) -> Result<ToolOutput, ToolError> {
        let view = self
            .registry
            .get(task_id)
            .ok_or_else(|| unknown_task(&self.registry, task_id))?;
        match &view.state {
            TaskState::Running => {
                let mut text = format!(
                    "Task {} is still running (elapsed {}s).",
                    view.task_id, view.elapsed_secs
                );
                let tail = self.registry.output_tail(task_id);
                if !tail.is_empty() {
                    text.push_str(&format!("\nOutput so far:\n{tail}"));
                }
                Ok(output(text))
            }
            TaskState::Finished { .. } => {
                self.registry.mark_consumed(task_id);
                let mut text = format!(
                    "Task {} {} (elapsed {}s).",
                    view.task_id,
                    describe_exit(&view.state),
                    view.elapsed_secs
                );
                let tail = self.registry.output_tail(task_id);
                if !tail.is_empty() {
                    text.push_str(&format!("\nOutput tail:\n{tail}"));
                }
                if let Some(path) = &view.output_path {
                    text.push_str(&format!("\n[Full output: {}]", path.display()));
                }
                Ok(output(text))
            }
        }
    }

    async fn kill(&self, task_id: &str) -> Result<ToolOutput, ToolError> {
        match self.registry.kill(task_id).await {
            Ok(KillOutcome::Killed {
                exit_code,
                elapsed_secs,
            }) => Ok(output(format!(
                "Task {task_id} {} (elapsed {elapsed_secs}s).",
                match exit_code {
                    Ok(code) => format!("killed (exit code {code})"),
                    Err(error) => format!("killed (could not be reaped: {error})"),
                }
            ))),
            Ok(KillOutcome::AlreadyFinished {
                exit_code: _,
                elapsed_secs,
                killed: _,
            }) => Ok(output(format!(
                "Task {task_id} already finished (elapsed {elapsed_secs}s); nothing to kill."
            ))),
            Ok(KillOutcome::Terminating) => Ok(output(format!(
                "Termination signal sent to task {task_id}; it has not exited within {}s. \
                 Check again with task_status.",
                KILL_WAIT.as_secs()
            ))),
            Err(message) => Err(fail(message)),
        }
    }
}

#[async_trait]
impl Tool for TaskStatusTool {
    fn name(&self) -> &str {
        "task_status"
    }

    fn description(&self) -> &str {
        "Check on shell commands that were moved to the background by the bash tool. \
         Actions: `status` reports whether a task is still running or finished (omit \
         task_id to list all tasks); `result` fetches a finished task's exit code and \
         output — if the task is still running it reports that instead, and fetching a \
         finished task's result here means the automatic completion notice will not be \
         repeated; `kill` terminates a running background task (only tasks started by \
         this agent's bash tool can be killed — the task list is shared across the \
         main session and its subagents)."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "result", "kill"],
                    "description": "`status` = check whether a task is finished; `result` = fetch a finished task's exit code and output; `kill` = terminate a running task"
                },
                "task_id": {
                    "type": "string",
                    "description": "Background task id (from the bash backgrounding result). Optional for `status` (omit to list all tasks); required for `result` and `kill`."
                }
            }
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "task_status(action, task_id?): check background task status, fetch a finished \
             task's result, or kill it"
                .into(),
        )
    }

    async fn execute(
        &self,
        call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let obj = call
            .args
            .as_object()
            .ok_or_else(|| fail("arguments must be an object".into()))?;
        let action = obj
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| fail("missing required argument `action`".into()))?;
        let task_id = obj.get("task_id").and_then(|v| v.as_str());
        match action {
            "status" => self.status(task_id),
            "result" => self.result(&require_task_id(task_id)?),
            "kill" => self.kill(&require_task_id(task_id)?).await,
            other => Err(fail(format!(
                "unknown action `{other}` (expected status, result or kill)"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// 完成通知器
// ---------------------------------------------------------------------------

/// 后台任务完成通知器(边界即时投递):通知立即入队 —— 模型忙时在
/// 下一个轮边界即注入(位于用户新请求之前),空闲时经 continue_run 唤醒
/// 新 run 首轮携带;不再等待整个 run 结束。入队时检查消费状态:模型已
/// 通过 task_status(result/kill)主动获知结局则不投递(已知取舍:预判式
/// result 调用若在入队后才返回,通知仍会发一次 —— 纯指引不含结果,多余
/// 但无害)。agent 弱引由装配层在会话建好后回填,会话已释放则通知静默
/// 丢弃。
pub struct AgentFollowUpNotifier {
    agent: Mutex<std::sync::Weak<latent_agent::Agent>>,
    registry: Option<Arc<BackgroundTaskRegistry>>,
}

impl AgentFollowUpNotifier {
    pub fn new(registry: Option<Arc<BackgroundTaskRegistry>>) -> Self {
        AgentFollowUpNotifier {
            agent: Mutex::new(std::sync::Weak::new()),
            registry,
        }
    }

    /// 会话建好后回填 agent 弱引(装配层调用)。
    pub fn set_agent(&self, agent: &Arc<latent_agent::Agent>) {
        *self.agent.lock().unwrap() = Arc::downgrade(agent);
    }
}

#[async_trait]
impl BackgroundNotifier for AgentFollowUpNotifier {
    async fn notify(&self, task_id: String, text: String) {
        let Some(agent) = self.agent.lock().unwrap().upgrade() else {
            return; // 会话已释放:通知无处投递,丢弃
        };
        if let Some(registry) = &self.registry {
            if registry.is_consumed(&task_id) {
                return;
            }
        }
        agent.follow_up(latent_agent::AgentMessage::user(text));
        if !agent.is_streaming() {
            // 仅空闲时才需要唤醒(忙时下一轮边界即注入);用户恰好并发起
            // 新 run 则 AlreadyRunning,通知留在队列由该 run 首轮拾取
            let _ = agent.continue_run().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash::{create_bash_tool_with, ShellSpawnOptions, ShellTimeoutPolicy};
    use latent_agent::AgentEvent;
    use latent_ai::{ContentBlock, Model, ScriptedProvider, ScriptedTurn};
    use std::path::Path;

    struct Noop;
    #[async_trait]
    impl ToolUpdater for Noop {
        async fn update(&self, _partial: String) {}
    }

    #[derive(Default)]
    struct CollectingNotifier(Mutex<Vec<(String, String)>>);

    #[async_trait]
    impl BackgroundNotifier for CollectingNotifier {
        async fn notify(&self, task_id: String, text: String) {
            self.0.lock().unwrap().push((task_id, text));
        }
    }

    async fn call(tool: &dyn Tool, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        tool.execute(
            ToolCall {
                id: "t".into(),
                name: tool.name().into(),
                args,
            },
            CancellationToken::new(),
            &Noop,
        )
        .await
    }

    fn fixture(
        notifier: Arc<dyn BackgroundNotifier>,
    ) -> (
        Arc<BackgroundTaskRegistry>,
        Arc<dyn Tool>,
        Arc<dyn Tool>,
    ) {
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let bash = create_bash_tool_with(
            Path::new("."),
            ShellSpawnOptions {
                timeouts: ShellTimeoutPolicy {
                    default_timeout_secs: 300,
                    background_after_secs: 1,
                },
                background_notifier: Some(notifier),
                task_registry: Some(registry.clone()),
                ..Default::default()
            },
        );
        let status = create_task_status_tool(registry.clone());
        (registry, bash, status)
    }

    async fn wait_finished(registry: &BackgroundTaskRegistry, task_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(view) = registry.get(task_id) {
                if matches!(view.state, TaskState::Finished { .. }) {
                    return;
                }
            }
            assert!(Instant::now() < deadline, "任务未在期限内结束");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // ---- registry 单测 ----------------------------------------------------

    #[test]
    fn registers_with_monotonic_ids() {
        let registry = BackgroundTaskRegistry::new();
        let acc = Arc::new(Mutex::new(OutputAccumulator::new(100, 10_000)));
        let (id1, _) = registry.register("echo one", None, acc.clone());
        let (id2, _) = registry.register("echo two", None, acc);
        assert_eq!(id1, "bash-1");
        assert_eq!(id2, "bash-2");
        assert!(registry.get("bash-9").is_none());
        assert_eq!(registry.list().len(), 2);
        assert!(!registry.is_consumed("bash-1"));
    }

    #[tokio::test]
    async fn mark_finished_records_state_and_or_kill_flag() {
        let registry = BackgroundTaskRegistry::new();
        // 自然结束:watcher 观察,非 kill
        let (id, _) = registry.register("echo", None, Arc::new(Mutex::new(OutputAccumulator::new(100, 10_000))));
        registry.mark_finished(&id, Ok(0), false);
        match registry.get(&id).unwrap().state {
            TaskState::Finished {
                exit_code: Ok(0),
                killed: false,
                ..
            } => {}
            other => panic!("unexpected state: {other:?}"),
        }
        // kill 请求与收尸竞速:kill 令牌先取消而 watcher 未观察到(进程恰好
        // 自然退出),仍以模型请求为准判 killed —— 走真实 kill() 置位路径
        let (id2, _) = registry.register("echo", None, Arc::new(Mutex::new(OutputAccumulator::new(100, 10_000))));
        let outcome = registry.kill(&id2).await.unwrap();
        assert!(matches!(outcome, KillOutcome::Terminating), "{outcome:?}");
        registry.mark_finished(&id2, Ok(0), false);
        assert!(
            matches!(
                registry.get(&id2).unwrap().state,
                TaskState::Finished { killed: true, .. }
            ),
            "kill_requested 应与 watcher 观察取或"
        );
    }

    #[tokio::test]
    async fn kill_unknown_task_is_error() {
        let registry = BackgroundTaskRegistry::new();
        assert!(registry.kill("bash-9").await.is_err());
    }

    // ---- 工具参数校验 ------------------------------------------------------

    #[tokio::test]
    async fn rejects_missing_or_unknown_action() {
        let (_, _, status) = fixture(Arc::new(CollectingNotifier::default()));
        let err = call(status.as_ref(), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing required argument `action`"));
        let err = call(status.as_ref(), serde_json::json!({"action": "nope"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown action `nope`"));
    }

    #[tokio::test]
    async fn result_and_kill_require_task_id() {
        let (_, _, status) = fixture(Arc::new(CollectingNotifier::default()));
        for action in ["result", "kill"] {
            let err = call(status.as_ref(), serde_json::json!({"action": action}))
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("missing required argument `task_id`"),
                "{action}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn status_on_empty_registry_lists_nothing() {
        let (_, _, status) = fixture(Arc::new(CollectingNotifier::default()));
        let out = call(status.as_ref(), serde_json::json!({"action": "status"}))
            .await
            .unwrap();
        assert_eq!(out.output, "No background tasks.");
    }

    #[tokio::test]
    async fn unknown_task_error_lists_known_ids() {
        let (_, bash, status) = fixture(Arc::new(CollectingNotifier::default()));
        call(
            bash.as_ref(),
            serde_json::json!({"command": "echo hi; sleep 2"}),
        )
        .await
        .unwrap();
        let err = call(
            status.as_ref(),
            serde_json::json!({"action": "status", "task_id": "bash-9"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Unknown task: bash-9"), "{err}");
        assert!(err.to_string().contains("bash-1"), "{err}");
    }

    // ---- status / result 全流程 -------------------------------------------

    #[tokio::test]
    async fn status_and_result_flow() {
        let notifier = Arc::new(CollectingNotifier::default());
        let (registry, bash, status) = fixture(notifier.clone());

        // 转后台:立即结算,消息只含任务开始 + task id(不带输出文件路径与
        // 已产出输出 —— 模型经 task_status 查询)
        let out = call(
            bash.as_ref(),
            serde_json::json!({"command": "echo early-marker; sleep 2; echo late-marker"}),
        )
        .await
        .unwrap();
        assert!(out.output.contains("Task started in background."), "{}", out.output);
        assert!(out.output.contains("Task ID: bash-1"), "{}", out.output);
        assert!(!out.output.contains("early-marker"), "{}", out.output);
        assert!(
            !out.output.contains("Full output is being written to"),
            "{}",
            out.output
        );

        // running:status / result 都可见,且不消费
        let view = call(
            status.as_ref(),
            serde_json::json!({"action": "status", "task_id": "bash-1"}),
        )
        .await
        .unwrap();
        assert!(view.output.contains("running"), "{}", view.output);
        assert!(view.output.contains("early-marker"), "{}", view.output);
        let view = call(
            status.as_ref(),
            serde_json::json!({"action": "result", "task_id": "bash-1"}),
        )
        .await
        .unwrap();
        assert!(view.output.contains("still running"), "{}", view.output);
        assert!(!registry.is_consumed("bash-1"));

        wait_finished(&registry, "bash-1").await;

        // finished:status 只报状态,不消费;完成通知已照常产出
        let view = call(
            status.as_ref(),
            serde_json::json!({"action": "status", "task_id": "bash-1"}),
        )
        .await
        .unwrap();
        assert!(view.output.contains("finished successfully"), "{}", view.output);
        assert!(!registry.is_consumed("bash-1"));
        assert!(
            !notifier.0.lock().unwrap().is_empty(),
            "未主动取结果时完成通知应照常产出"
        );

        // result:取到结果 = 消费,含退出码与转后台后才产出的输出
        let view = call(
            status.as_ref(),
            serde_json::json!({"action": "result", "task_id": "bash-1"}),
        )
        .await
        .unwrap();
        assert!(
            view.output.contains("finished successfully"),
            "{}",
            view.output
        );
        assert!(view.output.contains("late-marker"), "{}", view.output);
        assert!(registry.is_consumed("bash-1"));
    }

    #[tokio::test]
    async fn kill_terminates_running_task_and_consumes() {
        // kill 的抑制闸门在通知器(会话感知层):用真 AgentFollowUpNotifier
        // + 裸 agent 端到端验证 kill 即消费 → 不再投递完成通知
        let model = model();
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let notifier = Arc::new(AgentFollowUpNotifier::new(Some(registry.clone())));
        let bash = create_bash_tool_with(
            Path::new("."),
            ShellSpawnOptions {
                timeouts: ShellTimeoutPolicy {
                    default_timeout_secs: 300,
                    background_after_secs: 1,
                },
                background_notifier: Some(notifier.clone()),
                task_registry: Some(registry.clone()),
                ..Default::default()
            },
        );
        let status = create_task_status_tool(registry.clone());
        let provider = Arc::new(ScriptedProvider::new(
            &model,
            vec![ScriptedTurn::text(&model, "unused")],
        ));
        let agent = latent_agent::create_agent(provider.clone(), Arc::new(latent_agent::PassthroughHooks));
        agent.set_model(model.clone());
        notifier.set_agent(&agent);

        let out = call(
            bash.as_ref(),
            serde_json::json!({"command": "sleep 30"}),
        )
        .await
        .unwrap();
        assert!(out.output.contains("Task ID: bash-1"), "{}", out.output);
        assert_eq!(registry.get("bash-1").unwrap().task_id, "bash-1");

        let out = call(
            status.as_ref(),
            serde_json::json!({"action": "kill", "task_id": "bash-1"}),
        )
        .await
        .unwrap();
        assert!(out.output.contains("killed"), "{}", out.output);
        assert!(registry.is_consumed("bash-1"));

        wait_finished(&registry, "bash-1").await;
        // kill 已置位 consumed → 完成通知不再投递
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(provider.remaining(), 1, "kill 后不应唤醒模型");
        assert_eq!(
            agent.state_snapshot().message_count,
            0,
            "kill 后不应再收到完成通知"
        );
        let view = registry.get("bash-1").unwrap();
        match view.state {
            TaskState::Finished { exit_code, killed, .. } => {
                assert!(killed);
                #[cfg(unix)]
                assert!(matches!(exit_code, Ok(137)), "SIGKILL = 128+9: {exit_code:?}");
                #[cfg(not(unix))]
                assert!(exit_code.is_ok());
            }
            other => panic!("unexpected state: {other:?}"),
        }
    }

    #[tokio::test]
    async fn kill_on_finished_task_reports_and_consumes() {
        let notifier = Arc::new(CollectingNotifier::default());
        let (registry, bash, status) = fixture(notifier);
        call(bash.as_ref(), serde_json::json!({"command": "echo done; sleep 2"}))
            .await
            .unwrap();
        wait_finished(&registry, "bash-1").await;
        let out = call(
            status.as_ref(),
            serde_json::json!({"action": "kill", "task_id": "bash-1"}),
        )
        .await
        .unwrap();
        assert!(
            out.output.contains("already finished"),
            "{}",
            out.output
        );
        assert!(registry.is_consumed("bash-1"));
    }

    // ---- 完成通知的投递与抑制(真 Agent + ScriptedProvider,全离线)----

    /// 记录全部 MessageEnd(角色前缀 + 摘要),用于断言注入顺序与轮次数。
    #[derive(Default)]
    struct MessageLog(Mutex<Vec<String>>);

    #[async_trait]
    impl latent_agent::Subscriber for MessageLog {
        async fn on_event(&self, event: &AgentEvent) {
            let AgentEvent::MessageEnd { message } = event else {
                return;
            };
            let line = match message.as_ref() {
                latent_agent::AgentMessage::User { content, .. } => format!("user:{content}"),
                latent_agent::AgentMessage::Assistant(a) => format!(
                    "assistant:{}",
                    a.content
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join("")
                ),
                latent_agent::AgentMessage::ToolResult { tool_name, .. } => {
                    format!("toolresult:{tool_name}")
                }
                _ => "other".to_string(),
            };
            self.0.lock().unwrap().push(line);
        }
    }

    fn model() -> Model {
        Model::minimal("mock-1", "mock", "mock")
    }

    fn turn_calls(model: &Model, calls: Vec<ContentBlock>) -> ScriptedTurn {
        ScriptedTurn::tool_calls(model, calls)
    }

    fn bash_call(id: &str, command: &str, timeout: Option<u64>) -> ContentBlock {
        let mut args = serde_json::json!({"command": command});
        if let Some(timeout) = timeout {
            args["timeout"] = serde_json::json!(timeout);
        }
        ContentBlock::ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: args,
        }
    }

    async fn wait_until_idle(agent: &latent_agent::Agent, provider: &ScriptedProvider) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if provider.remaining() == 0 && !agent.is_streaming() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "会话未在期限内空闲(剩余脚本 {})",
                provider.remaining()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn completion_notification_reaches_model_as_follow_up() {
        let model = model();
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let notifier = Arc::new(AgentFollowUpNotifier::new(Some(registry.clone())));
        let bash = create_bash_tool_with(
            Path::new("."),
            ShellSpawnOptions {
                timeouts: ShellTimeoutPolicy {
                    default_timeout_secs: 120,
                    background_after_secs: 1,
                },
                background_notifier: Some(notifier.clone()),
                task_registry: Some(registry.clone()),
                ..Default::default()
            },
        );
        let status = create_task_status_tool(registry.clone());
        let provider = Arc::new(ScriptedProvider::new(
            &model,
            vec![
                // turn1:长命令 → 1s 转后台,任务 1.5s 结束
                turn_calls(&model, vec![bash_call("t1", "sleep 1.5", None)]),
                // turn2:模型先收尾
                ScriptedTurn::text(&model, "ok, running in background"),
                // turn3:唤醒 run 首轮即携带通知(无盲转)
                ScriptedTurn::text(&model, "noticed completion"),
            ],
        ));
        let agent = latent_agent::create_agent(provider.clone(), Arc::new(latent_agent::PassthroughHooks));
        agent.set_model(model.clone());
        agent.install_tools(vec![bash, status]);
        notifier.set_agent(&agent);
        let log = Arc::new(MessageLog::default());
        agent.subscribe(log.clone());

        agent.prompt("run it").await.unwrap();
        wait_until_idle(&agent, &provider).await;

        let log = log.0.lock().unwrap().clone();
        // 无盲转:恰好 3 个 assistant 轮(初始 run 两轮 + 唤醒 run 一轮)
        let assistants = log.iter().filter(|l| l.starts_with("assistant:")).count();
        assert_eq!(assistants, 3, "不应有盲转或脚本耗尽轮: {log:?}");
        // 通知文本:状态 + 指引,不含输出(拉取式)
        let notice_idx = log
            .iter()
            .position(|l| l.starts_with("user:") && l.contains("[latent] background task bash-1"))
            .expect("完成通知应注入转录");
        let notice = &log[notice_idx];
        assert!(notice.contains("finished successfully"), "{notice}");
        assert!(notice.contains("task_status"), "{notice}");
        assert!(notice.contains("action: \"result\""), "{notice}");
        assert!(!notice.contains("Output tail"), "{notice}");
        // 通知先于唤醒轮的模型回复
        let last_assistant = log.iter().rposition(|l| l.starts_with("assistant:")).unwrap();
        assert!(notice_idx < last_assistant, "{log:?}");
    }

    #[tokio::test]
    async fn notification_delivered_at_next_turn_boundary_midrun() {
        let model = model();
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let notifier = Arc::new(AgentFollowUpNotifier::new(Some(registry.clone())));
        let bash = create_bash_tool_with(
            Path::new("."),
            ShellSpawnOptions {
                timeouts: ShellTimeoutPolicy {
                    default_timeout_secs: 120,
                    background_after_secs: 1,
                },
                background_notifier: Some(notifier.clone()),
                task_registry: Some(registry.clone()),
                ..Default::default()
            },
        );
        let status = create_task_status_tool(registry.clone());
        let provider = Arc::new(ScriptedProvider::new(
            &model,
            vec![
                // turn1:长命令 → 1s 转后台,任务 1.5s 结束
                turn_calls(&model, vec![bash_call("t1", "sleep 1.5", None)]),
                // turn2:无 timeout 的阻塞命令(约 1.1s-2.6s);任务在其
                // 执行窗口内(1.5s 处,两侧余量 >400ms)完成 → 通知入队
                turn_calls(&model, vec![bash_call("t2", "sleep 1.5", None)]),
                // turn3:通知在 turn2 结束的边界注入(不等整个 run 结束)
                ScriptedTurn::text(&model, "noticed completion"),
            ],
        ));
        let agent = latent_agent::create_agent(provider.clone(), Arc::new(latent_agent::PassthroughHooks));
        agent.set_model(model.clone());
        agent.install_tools(vec![bash, status]);
        notifier.set_agent(&agent);
        let log = Arc::new(MessageLog::default());
        agent.subscribe(log.clone());

        agent.prompt("run it").await.unwrap();
        wait_until_idle(&agent, &provider).await;

        let log = log.0.lock().unwrap().clone();
        let assistants = log.iter().filter(|l| l.starts_with("assistant:")).count();
        assert_eq!(assistants, 3, "通知应在下一轮边界注入,无额外轮次: {log:?}");
        let notice_idx = log
            .iter()
            .position(|l| l.starts_with("user:") && l.contains("[latent] background task bash-1"))
            .unwrap_or_else(|| panic!("通知应注入转录: {log:?}"));
        // 通知位于第二个工具结果之后(结果在执行期已进转录)、看到通知后的
        // 模型回复之前 —— 即 turn3 请求携带了通知
        let toolresults = log
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("toolresult:"))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        assert_eq!(toolresults.len(), 2, "{log:?}");
        assert!(notice_idx > toolresults[1], "{log:?}");
        let last_assistant = log.iter().rposition(|l| l.starts_with("assistant:")).unwrap();
        assert!(notice_idx < last_assistant, "{log:?}");
    }

    #[tokio::test]
    async fn consumed_task_suppresses_notification_at_queue_time() {
        // 取舍(选项 A):消费检查在入队时 —— 模型已主动获知结局的任务不再
        // 投递;预判式 result 调用若在入队后才返回,通知仍会发一次(纯指引,
        // 多余但无害)
        let model = model();
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let notifier = Arc::new(AgentFollowUpNotifier::new(Some(registry.clone())));
        let (task_id, _) = registry.register(
            "sleep 30",
            None,
            Arc::new(Mutex::new(OutputAccumulator::new(100, 10_000))),
        );
        registry.mark_finished(&task_id, Ok(0), false);
        registry.mark_consumed(&task_id); // 模型已主动取到结果

        let provider = Arc::new(ScriptedProvider::new(
            &model,
            vec![ScriptedTurn::text(&model, "unused")],
        ));
        let agent = latent_agent::create_agent(provider.clone(), Arc::new(latent_agent::PassthroughHooks));
        agent.set_model(model.clone());
        notifier.set_agent(&agent);

        notifier
            .notify(task_id, "[latent] background task finished.".into())
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(provider.remaining(), 1, "已消费的通知不应唤醒模型");
        assert!(!agent.is_streaming());
        assert_eq!(agent.state_snapshot().message_count, 0, "不应有新消息");
    }
}
