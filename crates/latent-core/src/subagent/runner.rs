//! 子 agent 构建与运行(14 文档 §4.2/§4.3):进程内嵌套 `latent_agent::Agent`,
//! 同一 provider/审批洋葱;转录纯内存(不接 SessionSink);单层嵌套 =
//! 子工具面里没有 task 工具(白名单解析层再防御性过滤一次)。
//!
//! 同步路径在调用方(execute)帧内 await;后台路径由 task 工具 spawn,
//! 结算后经 RunRegistry 通知 supervisor。进度预览经 `ToolUpdater::update`
//! 流式回传(仅同步路径持有 updater 借用,无需 Arc 侧写)。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use latent_agent::{create_agent, AgentEvent, LoopHooks, RunStop, SharedSubscriber, Subscriber, Tool, ToolUpdater};
use latent_ai::{Model, Provider, StopReason, ThinkingLevel, Usage};

use super::store::ChildStore;

/// 单次运行终态(14 文档 §4.2 状态映射;Running 只存在于注册表)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Stopped,
    TimedOut,
    Failed,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Completed => "completed",
            RunStatus::Stopped => "stopped",
            RunStatus::TimedOut => "timed out",
            RunStatus::Failed => "failed",
        }
    }
}

/// 子 agent 运行规格(校验后的 task 工具参数)。
pub struct ChildSpec {
    /// 展示名(agent 定义名或 "inline")
    pub name: String,
    pub task: String,
    pub system_prompt: String,
    pub model: Model,
    pub thinking: Option<ThinkingLevel>,
    pub tools: Vec<Arc<dyn Tool>>,
    /// 会话落盘(可选):tag 已由调用方定为 run id;Some 时挂持久化订阅者
    /// 并启用其 stream_options(contextSnapshot)
    pub persistence: Option<ChildStore>,
}

/// 运行护栏:超时 + 两路取消(父 abort 级联 / action:"stop")。
#[derive(Clone)]
pub struct RunGuard {
    /// 父 run 的 cancel 的 child token:父会话 abort(Esc/Ctrl-C)级联终止
    pub parent_cancel: CancellationToken,
    /// action:"stop" 专用;注册表持有同一 token
    pub stop: CancellationToken,
    pub timeout: Duration,
}

/// 运行结算结果。
#[derive(Debug, Clone)]
pub struct ChildOutcome {
    pub status: RunStatus,
    /// 最终 assistant 文本(可能为 None:没有产出文本就终止了)
    pub output: Option<String>,
    pub error: Option<String>,
    pub duration: Duration,
    pub tool_calls: usize,
    pub usage: Usage,
    /// 父会话被 abort 导致的级联停止(supervisor 据此跳过唤醒)
    pub cancelled_by_parent: bool,
    pub model_id: String,
}

/// 进度预览节流阈值:累计新文本超过该字符数才回传一次(防刷屏)。
const PROGRESS_CHUNK_CHARS: usize = 200;

struct Collector {
    name: String,
    progress_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    inner: Mutex<CollectState>,
}

#[derive(Default)]
struct CollectState {
    tool_calls: usize,
    usage: Option<Usage>,
    /// 最终输出:优先最后一条 stop 且非空的 assistant 文本
    final_output: Option<String>,
    /// 进度累积缓冲(每 PROGRESS_CHUNK_CHARS 字符 flush 一次)
    progress_buffer: String,
}

impl Collector {
    fn new(name: String) -> Self {
        Collector {
            name,
            progress_tx: Mutex::new(None),
            inner: Mutex::new(CollectState::default()),
        }
    }

    fn attach(&self, tx: mpsc::UnboundedSender<String>) {
        *self.progress_tx.lock().unwrap() = Some(tx);
    }

    fn flush_progress(&self) {
        let mut state = self.inner.lock().unwrap();
        if state.progress_buffer.chars().count() < PROGRESS_CHUNK_CHARS {
            return;
        }
        let buffer = std::mem::take(&mut state.progress_buffer);
        drop(state);
        if let Some(tx) = self.progress_tx.lock().unwrap().as_ref() {
            let chars = buffer.chars().count();
            let from = chars.saturating_sub(400);
            let tail: String = buffer.chars().skip(from).collect();
            let _ = tx.send(format!(
                "[{}] {}{}",
                self.name,
                if from > 0 { "…" } else { "" },
                tail
            ));
        }
    }
}

#[async_trait]
impl Subscriber for Collector {
    async fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolExecutionStart { .. } => {
                self.inner.lock().unwrap().tool_calls += 1;
            }
            // 进度增量只在 MessageDelta 逐块出现(MessageUpdate 是终态快照)
            AgentEvent::MessageDelta {
                delta: latent_agent::MessageDeltaPayload::Text { delta },
            } => {
                self.inner.lock().unwrap().progress_buffer.push_str(delta);
                self.flush_progress();
            }
            AgentEvent::MessageEnd { message } => {
                if let Some(assistant) = message.as_assistant() {
                    let mut state = self.inner.lock().unwrap();
                    let usage = state.usage.get_or_insert_with(Usage::zero);
                    usage.input += assistant.usage.input;
                    usage.output += assistant.usage.output;
                    usage.total_tokens += assistant.usage.total_tokens;
                    if assistant.stop_reason == StopReason::Stop && !assistant.text_content().is_empty() {
                        state.final_output = Some(assistant.text_content());
                    }
                }
            }
            AgentEvent::AgentEnd { messages } => {
                // 兜底:MessageEnd 已记录 stop 文本;这里补"有文本的非错误 assistant"
                let mut state = self.inner.lock().unwrap();
                if state.final_output.is_none() {
                    state.final_output = messages.iter().rev().find_map(|message| {
                        message.as_assistant().and_then(|assistant| {
                            if assistant.stop_reason == StopReason::Error {
                                None
                            } else {
                                let text = assistant.text_content();
                                (!text.is_empty()).then_some(text)
                            }
                        })
                    });
                }
            }
            _ => {}
        }
    }
}

/// 构建 + 运行一个子 agent 到终态。
///
/// abort 语义:guard 任一路触发 → `child.abort()` → 继续轮询 run 直到结算
/// (进度照常转发);父取消优先于 stop 判定。
pub async fn run_child(
    provider: Arc<dyn Provider>,
    hooks: Arc<dyn LoopHooks>,
    spec: ChildSpec,
    guard: RunGuard,
    updater: Option<&dyn ToolUpdater>,
) -> ChildOutcome {
    let child = create_agent(provider, hooks);
    child.set_system_prompt(Some(spec.system_prompt));
    child.set_model(spec.model.clone());
    child.set_thinking_level(spec.thinking);
    child.install_tools(spec.tools);
    // 会话落盘:JSONL(与主会话条目格式一致)+ contextSnapshot 快照
    if let Some(store) = &spec.persistence {
        child.subscribe(crate::session::create_session_persistence_subscriber(
            store.sink.clone(),
        ));
        child.set_stream_options(store.stream_options.clone());
    }

    let collector = Arc::new(Collector::new(spec.name.clone()));
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<String>();
    collector.attach(progress_tx);
    child.subscribe(collector.clone() as SharedSubscriber);

    let model_id = spec.model.id.clone();
    let started = Instant::now();
    let mut run = std::pin::pin!(child.prompt(spec.task));
    let mut guard_reason: Option<GuardReason> = None;
    let mut progress_closed = false;

    let stop: Option<Result<RunStop, latent_agent::AgentError>> = loop {
        tokio::select! {
            biased;
            result = &mut run => break Some(result),
            _ = guard.parent_cancel.cancelled(), if guard_reason.is_none() => {
                guard_reason = Some(GuardReason::ParentCancel);
                child.abort();
            }
            _ = guard.stop.cancelled(), if guard_reason.is_none() => {
                guard_reason = Some(GuardReason::Stop);
                child.abort();
            }
            _ = tokio::time::sleep_until(
                tokio::time::Instant::from_std(started + guard.timeout),
            ), if guard_reason.is_none() => {
                guard_reason = Some(GuardReason::Timeout);
                child.abort();
            }
            partial = progress_rx.recv(), if !progress_closed => {
                match partial {
                    Some(text) => {
                        if let Some(updater) = updater {
                            updater.update(text).await;
                        }
                    }
                    None => progress_closed = true,
                }
            }
        }
    };
    // guard 触发后 run 尚未结算:继续等待(child.abort 已发出)
    let stop = match stop {
        Some(stop) => stop,
        None => run.as_mut().await,
    };
    // run 可能在单次 poll 内完成(biased 分支优先):把未消费的进度补发完
    if let Some(updater) = updater {
        while let Ok(text) = progress_rx.try_recv() {
            updater.update(text).await;
        }
    }

    let state = std::mem::take(&mut *collector.inner.lock().unwrap());
    let cancelled_by_parent = guard.parent_cancel.is_cancelled();
    let timed_out = matches!(guard_reason, Some(GuardReason::Timeout));
    let (status, error) = match &stop {
        Ok(RunStop::EndTurn) => (RunStatus::Completed, None),
        Ok(RunStop::Aborted) => (
            if timed_out {
                RunStatus::TimedOut
            } else {
                RunStatus::Stopped
            },
            None,
        ),
        Ok(RunStop::Error(message)) => (RunStatus::Failed, Some(message.clone())),
        Ok(RunStop::BudgetExhausted(kind)) => (
            RunStatus::Failed,
            Some(format!("budget exhausted: {kind:?}")),
        ),
        Err(error) => (RunStatus::Failed, Some(error.to_string())),
    };
    ChildOutcome {
        status,
        output: state.final_output,
        error,
        duration: started.elapsed(),
        tool_calls: state.tool_calls,
        usage: state.usage.unwrap_or_else(Usage::zero),
        cancelled_by_parent,
        model_id,
    }
}

enum GuardReason {
    ParentCancel,
    Stop,
    Timeout,
}

/// 输出截断上限(同步结果;与工具自我输出上限同源派生 = agent 转录裁剪
/// 上限 - 2k 余量,避免结果注入父转录后被 agent 层头尾裁剪挖洞)。
pub const MAX_OUTPUT_CHARS: usize =
    latent_agent::tool_self_output_limit(latent_agent::DEFAULT_TOOL_RESULT_MAX_CHARS);

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}\n…[truncated {max} chars]")
}

/// 拼装工具结果/错误文本(subagent-lite 的输出格式)。
pub fn format_child_result(spec_name: &str, run_id: &str, outcome: &ChildOutcome) -> String {
    let mut text = format!(
        "Subagent {} {} in {:.1}s.\nRun id: {} · Model: {} · tokens in {} / out {} · tool calls {}\n",
        spec_name,
        outcome.status.as_str(),
        outcome.duration.as_secs_f32(),
        run_id,
        outcome.model_id,
        outcome.usage.input,
        outcome.usage.output,
        outcome.tool_calls,
    );
    if let Some(error) = &outcome.error {
        text.push_str(&format!("\nError: {error}\n"));
    }
    if let Some(output) = &outcome.output {
        text.push('\n');
        text.push_str(&truncate_chars(output, MAX_OUTPUT_CHARS));
    } else if outcome.error.is_none() {
        text.push_str("\n(no assistant text output)\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use latent_agent::PassthroughHooks;
    use latent_ai::{ScriptedProvider, ScriptedTurn};

    fn text_provider(model: &Model, text: &str) -> Arc<dyn Provider> {
        Arc::new(ScriptedProvider::new(
            model,
            vec![ScriptedTurn::text(model, text)],
        ))
    }

    fn model() -> Model {
        Model::minimal("m1", "mock", "mock")
    }

    fn spec(model: &Model, task: &str) -> ChildSpec {
        ChildSpec {
            name: "tester".into(),
            task: task.into(),
            system_prompt: "be terse".into(),
            model: model.clone(),
            thinking: None,
            tools: Vec::new(),
            persistence: None,
        }
    }

    #[tokio::test]
    async fn completes_and_extracts_output() {
        let model = model();
        let outcome = run_child(
            text_provider(&model, "the answer"),
            Arc::new(PassthroughHooks),
            spec(&model, "do it"),
            RunGuard {
                parent_cancel: CancellationToken::new(),
                stop: CancellationToken::new(),
                timeout: Duration::from_secs(10),
            },
            None,
        )
        .await;
        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(outcome.output.as_deref(), Some("the answer"));
        assert_eq!(outcome.tool_calls, 0);
        assert!(!outcome.cancelled_by_parent);
    }

    #[tokio::test]
    async fn timeout_marks_timed_out() {
        let model = model();
        // 子 agent 请求一个带延迟的 turn:超时先到
        let provider = Arc::new(ScriptedProvider::new(
            &model,
            vec![ScriptedTurn::text(&model, "late").with_delay(5_000)],
        ));
        let outcome = run_child(
            provider,
            Arc::new(PassthroughHooks),
            spec(&model, "hang"),
            RunGuard {
                parent_cancel: CancellationToken::new(),
                stop: CancellationToken::new(),
                timeout: Duration::from_millis(50),
            },
            None,
        )
        .await;
        assert_eq!(outcome.status, RunStatus::TimedOut);
        assert!(outcome.output.is_none() || outcome.output.as_deref() != Some("late"));
    }

    #[tokio::test]
    async fn parent_cancel_cascades_and_flags() {
        let model = model();
        let provider = Arc::new(ScriptedProvider::new(
            &model,
            vec![ScriptedTurn::text(&model, "late").with_delay(5_000)],
        ));
        let parent_cancel = CancellationToken::new();
        let stop = CancellationToken::new();
        let guard = RunGuard {
            parent_cancel: parent_cancel.clone(),
            stop: stop.clone(),
            timeout: Duration::from_secs(30),
        };
        let task = run_child(
            provider,
            Arc::new(PassthroughHooks),
            spec(&model, "hang"),
            guard,
            None,
        );
        let handle = tokio::spawn(task);
        tokio::time::sleep(Duration::from_millis(20)).await;
        parent_cancel.cancel();
        let outcome = handle.await.unwrap();
        assert_eq!(outcome.status, RunStatus::Stopped);
        assert!(outcome.cancelled_by_parent);
    }

    #[tokio::test]
    async fn stop_token_stops_without_parent_flag() {
        let model = model();
        let provider = Arc::new(ScriptedProvider::new(
            &model,
            vec![ScriptedTurn::text(&model, "late").with_delay(5_000)],
        ));
        let stop = CancellationToken::new();
        let guard = RunGuard {
            parent_cancel: CancellationToken::new(),
            stop: stop.clone(),
            timeout: Duration::from_secs(30),
        };
        let handle = tokio::spawn(run_child(
            provider,
            Arc::new(PassthroughHooks),
            spec(&model, "hang"),
            guard,
            None,
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        stop.cancel();
        let outcome = handle.await.unwrap();
        assert_eq!(outcome.status, RunStatus::Stopped);
        assert!(!outcome.cancelled_by_parent);
    }

    #[tokio::test]
    async fn provider_error_maps_to_failed() {
        let model = model();
        let provider = Arc::new(ScriptedProvider::new(
            &model,
            vec![ScriptedTurn::error(&model, "boom")],
        ));
        let outcome = run_child(
            provider,
            Arc::new(PassthroughHooks),
            spec(&model, "go"),
            RunGuard {
                parent_cancel: CancellationToken::new(),
                stop: CancellationToken::new(),
                timeout: Duration::from_secs(10),
            },
            None,
        )
        .await;
        assert_eq!(outcome.status, RunStatus::Failed);
        assert!(outcome.error.as_deref().unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn forwards_progress_with_prefix() {
        struct Collecting(Mutex<Vec<String>>);
        #[async_trait]
        impl ToolUpdater for Collecting {
            async fn update(&self, partial: String) {
                self.0.lock().unwrap().push(partial);
            }
        }
        let model = model();
        let reply = "x".repeat(1000);
        let provider = text_provider(&model, &reply);
        let collecting = Arc::new(Collecting(Mutex::new(Vec::new())));
        let outcome = run_child(
            provider,
            Arc::new(PassthroughHooks),
            spec(&model, "go"),
            RunGuard {
                parent_cancel: CancellationToken::new(),
                stop: CancellationToken::new(),
                timeout: Duration::from_secs(10),
            },
            Some(&*collecting as &dyn ToolUpdater),
        )
        .await;
        assert_eq!(outcome.status, RunStatus::Completed);
        let updates = collecting.0.lock().unwrap();
        assert!(!updates.is_empty(), "应有进度回传");
        assert!(updates[0].starts_with("[tester] "));
    }

    #[test]
    fn format_child_result_includes_status_and_output() {
        let outcome = ChildOutcome {
            status: RunStatus::Completed,
            output: Some("done".into()),
            error: None,
            duration: Duration::from_millis(1500),
            tool_calls: 2,
            usage: Usage::zero(),
            cancelled_by_parent: false,
            model_id: "m1".into(),
        };
        let text = format_child_result("reviewer", "abc12345", &outcome);
        assert!(text.contains("Subagent reviewer completed in 1.5s."));
        assert!(text.contains("Run id: abc12345"));
        assert!(text.contains("done"));
    }

    #[test]
    fn truncate_chars_caps_long_output() {
        let long = "a".repeat(60_000);
        let truncated = truncate_chars(&long, MAX_OUTPUT_CHARS);
        assert!(truncated.contains("truncated"));
        assert!(truncated.chars().count() < 60_000);
    }
}
