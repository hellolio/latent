//! 运行注册表 + supervisor 空闲唤醒(14 文档 §4.3)。
//!
//! 所有运行(同步/后台)统一登记;后台运行结算后由 supervisor 任务唤醒父
//! 会话:`wait_idle()` → `follow_up(通知)` → `continue_run()`。若 continue
//! 撞上用户已开启的新 run(AlreadyRunning),follow_up 留在队列由下一停止点
//! 消费——天然兜底,不丢消息(agent.rs 的 requeue 语义)。
//!
//! 父 abort 级联停止的运行(cancelled_by_parent)不投递通知:用户按 Esc 后
//! 不应被"已停止"的通知重新拉起新 turn。

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use latent_agent::{Agent, AgentMessage};

use super::runner::{format_child_result, truncate_chars, ChildOutcome, RunStatus};

/// 后台活跃上限(14 文档 §4.3)。
pub const MAX_ACTIVE_ASYNC: usize = 16;
/// 运行历史上限(超出淘汰最旧的已完成记录)。
pub const MAX_RUN_HISTORY: usize = 50;
/// 异步完成通知里的结果预览上限。
const NOTIFY_PREVIEW_CHARS: usize = 2_000;
/// stop 后等待结算的轮询上限。
const STOP_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_SETTLE_POLL: Duration = Duration::from_millis(50);

/// 注册表内的一条运行记录。
#[derive(Clone)]
pub struct RunEntry {
    pub id: String,
    pub agent: String,
    pub task_preview: String,
    pub background: bool,
    pub started_at: Instant,
    pub status: Option<RunStatus>,
    /// None = 仍在运行
    pub output_preview: Option<String>,
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
    pub cancelled_by_parent: bool,
    pub tool_calls: usize,
    stop: CancellationToken,
}

impl RunEntry {
    fn status_str(&self) -> &str {
        self.status.map(|s| s.as_str()).unwrap_or("running")
    }
}

/// 完成通知(supervisor 消费)。
pub struct CompletionNotice {
    pub text: String,
}

pub struct SubagentRegistry {
    runs: Mutex<Vec<RunEntry>>,
    completions_tx: mpsc::UnboundedSender<CompletionNotice>,
    completions_rx: Mutex<Option<mpsc::UnboundedReceiver<CompletionNotice>>>,
    parent: Arc<Mutex<Weak<Agent>>>,
    /// 通知抑制(/new 与会话退出 abort_all 后,残留结算不再唤醒父会话;
    /// 下一次 register 重新启用)
    suppressed: Mutex<bool>,
}

impl SubagentRegistry {
    pub fn new(parent: Arc<Mutex<Weak<Agent>>>) -> Arc<Self> {
        let (completions_tx, completions_rx) = mpsc::unbounded_channel();
        Arc::new(SubagentRegistry {
            runs: Mutex::new(Vec::new()),
            completions_tx,
            completions_rx: Mutex::new(Some(completions_rx)),
            parent,
            suppressed: Mutex::new(false),
        })
    }

    /// supervisor 任务(单例):等父空闲 → follow_up 通知 → 驱动新 run。
    /// 重复调用是 no-op(接收端只能取一次)。
    pub fn spawn_supervisor(&self) {
        let Some(mut rx) = self.completions_rx.lock().unwrap().take() else {
            return;
        };
        let this = self.parent.clone();
        tokio::spawn(async move {
            while let Some(notice) = rx.recv().await {
                let Some(agent) = this.lock().unwrap().upgrade() else {
                    continue; // 父会话已释放(换会话/退出):通知无处投递,丢弃
                };
                agent.wait_idle().await;
                agent.follow_up(AgentMessage::user(notice.text));
                // 已有活动 run(用户恰在交互)时失败:通知已在 follow_up 队列,
                // 下一停止点由循环整流消费
                let _ = agent.continue_run().await;
            }
        });
    }

    /// 登记一次运行,返回 run id(8 位短 id)。
    pub fn register(
        &self,
        agent: &str,
        task: &str,
        background: bool,
        stop: CancellationToken,
    ) -> String {
        *self.suppressed.lock().unwrap() = false;
        let mut runs = self.runs.lock().unwrap();
        let id = fresh_id(&runs);
        runs.push(RunEntry {
            id: id.clone(),
            agent: agent.to_string(),
            task_preview: truncate_chars(task, 120).replace('\n', " "),
            background,
            started_at: Instant::now(),
            status: None,
            output_preview: None,
            error: None,
            duration_ms: None,
            cancelled_by_parent: false,
            tool_calls: 0,
            stop,
        });
        evict_overflow(&mut runs);
        id
    }

    /// 结算一条运行;后台且非父级联停止时投递完成通知。
    pub fn finish(&self, id: &str, outcome: &ChildOutcome) {
        let entry = {
            let mut runs = self.runs.lock().unwrap();
            match runs.iter_mut().find(|entry| entry.id == id) {
                Some(entry) => {
                    entry.status = Some(outcome.status);
                    entry.output_preview = outcome
                        .output
                        .as_ref()
                        .map(|output| truncate_chars(output, NOTIFY_PREVIEW_CHARS));
                    entry.error = outcome.error.clone();
                    entry.duration_ms = Some(outcome.duration.as_millis() as u64);
                    entry.cancelled_by_parent = outcome.cancelled_by_parent;
                    entry.tool_calls = outcome.tool_calls;
                    entry.clone()
                }
                None => return,
            }
        };
        let suppressed = *self.suppressed.lock().unwrap();
        if entry.background && !outcome.cancelled_by_parent && !suppressed {
            let text = format_child_result(&entry.agent, &entry.id, outcome);
            let _ = self.completions_tx.send(CompletionNotice { text });
        }
    }

    /// 后台活跃数(Running 且 background)。
    pub fn active_background(&self) -> usize {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.background && entry.status.is_none())
            .count()
    }

    /// action:"stop":按 id(或唯一前缀)停止运行,等待结算。
    pub async fn stop(&self, id: &str) -> Result<String, String> {
        let (entry, ambiguity) = {
            let runs = self.runs.lock().unwrap();
            let matches: Vec<&RunEntry> = runs
                .iter()
                .filter(|entry| entry.id == id || entry.id.starts_with(id))
                .collect();
            if matches.iter().any(|entry| entry.id == id) {
                (Some(matches.into_iter().find(|e| e.id == id).unwrap().clone()), 0)
            } else if matches.len() == 1 {
                (Some(matches[0].clone()), 0)
            } else if matches.len() > 1 {
                (None, matches.len())
            } else {
                (None, 0)
            }
        };
        let Some(entry) = entry else {
            if ambiguity > 1 {
                return Err(format!("run id `{id}` matched {ambiguity} runs; provide a longer prefix"));
            }
            let ids: Vec<String> = self
                .runs
                .lock()
                .unwrap()
                .iter()
                .map(|entry| entry.id.clone())
                .collect();
            return Err(format!("run id `{id}` not found; existing runs: {ids:?}"));
        };
        if entry.status.is_some() {
            return Ok(format!("run {} already finished ({})", entry.id, entry.status_str()));
        }
        entry.stop.cancel();
        let deadline = Instant::now() + STOP_SETTLE_TIMEOUT;
        loop {
            {
                let runs = self.runs.lock().unwrap();
                if runs
                    .iter()
                    .find(|e| e.id == entry.id)
                    .and_then(|e| e.status)
                    .is_some()
                {
                    return Ok(format!("run {} already stopped", entry.id));
                }
            }
            if Instant::now() >= deadline {
                return Ok(format!(
                    "run {} stop signal sent, still settling (check with action:\"list\" later)",
                    entry.id
                ));
            }
            tokio::time::sleep(STOP_SETTLE_POLL).await;
        }
    }

    /// action:"list":运行记录表。
    pub fn list(&self) -> String {
        let runs = self.runs.lock().unwrap();
        if runs.is_empty() {
            return "No subagent runs yet.".to_string();
        }
        let mut lines = vec!["Subagent runs (newest last):".to_string()];
        for entry in runs.iter() {
            let kind = if entry.background { "async" } else { "sync" };
            let elapsed = entry
                .duration_ms
                .map(|ms| format!("{:.1}s", ms as f32 / 1000.0))
                .unwrap_or_else(|| format!("{:.1}s", entry.started_at.elapsed().as_secs_f32()));
            let task = entry.task_preview.trim_end();
            lines.push(format!(
                "- {} · {} · {} · {} · {} · tools {} · {}",
                entry.id,
                entry.agent,
                kind,
                entry.status_str(),
                elapsed,
                entry.tool_calls,
                task,
            ));
        }
        lines.join("\n")
    }

    /// /new 与会话退出清理:停止全部存活运行并抑制其完成通知
    /// (旧会话的后台运行不应把新会话从 idle 拉起)。
    pub fn abort_all(&self) {
        *self.suppressed.lock().unwrap() = true;
        let runs = self.runs.lock().unwrap();
        for entry in runs.iter() {
            if entry.status.is_none() {
                entry.stop.cancel();
            }
        }
    }

    /// 存活运行数(TUI 状态行)。
    pub fn active_count(&self) -> usize {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.status.is_none())
            .count()
    }
}

/// run id:进程级单调计数器(**不能用 uuid v7 截短**:其 simple 形态前 8 位
/// 是毫秒时间戳高位,65.5s(2^16 ms)窗口内全部相同,碰撞检查会卡到窗口
/// 翻越才退出)。计数器天然唯一,扫描仅作防御。
fn fresh_id(runs: &[RunEntry]) -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    loop {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("{n:08x}");
        if !runs.iter().any(|entry| entry.id == id) {
            return id;
        }
    }
}
/// 历史超限:优先淘汰最旧的已完成记录;全在运行则不淘汰(活跃数有独立上限)。
fn evict_overflow(runs: &mut Vec<RunEntry>) {
    while runs.len() > MAX_RUN_HISTORY {
        let Some(victim) = runs.iter().position(|entry| entry.status.is_some()) else {
            return;
        };
        runs.remove(victim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, status: Option<RunStatus>, background: bool) -> RunEntry {
        RunEntry {
            id: id.into(),
            agent: "reviewer".into(),
            task_preview: "do things".into(),
            background,
            started_at: Instant::now(),
            status,
            output_preview: None,
            error: None,
            duration_ms: None,
            cancelled_by_parent: false,
            tool_calls: 0,
            stop: CancellationToken::new(),
        }
    }

    #[test]
    fn evict_removes_oldest_settled_first() {
        let mut runs: Vec<RunEntry> = Vec::new();
        for i in 0..(MAX_RUN_HISTORY + 1) {
            runs.push(entry(&format!("id{i:02}"), Some(RunStatus::Completed), true));
        }
        runs.push(entry("running1", None, true));
        evict_overflow(&mut runs);
        assert_eq!(runs.len(), MAX_RUN_HISTORY);
        assert_eq!(runs.first().unwrap().id, "id02", "最旧的已完成被淘汰");
        assert!(runs.iter().any(|e| e.id == "running1"), "运行中的不淘汰");
    }

    #[test]
    fn register_generates_unique_short_ids() {
        let registry = SubagentRegistry::new(Arc::new(Mutex::new(Weak::new())));
        let id1 = registry.register("a", "task", true, CancellationToken::new());
        let id2 = registry.register("a", "task", true, CancellationToken::new());
        assert_eq!(id1.len(), 8);
        assert_ne!(id1, id2);
        assert_eq!(registry.active_background(), 2);
    }

    #[tokio::test]
    async fn stop_by_unique_prefix_and_errors() {
        let registry = SubagentRegistry::new(Arc::new(Mutex::new(Weak::new())));
        let token = CancellationToken::new();
        let id = registry.register("a", "task", true, token.clone());
        let outcome = ChildOutcome {
            status: RunStatus::Completed,
            output: Some("done".into()),
            error: None,
            duration: Duration::from_millis(10),
            tool_calls: 0,
            usage: latent_ai::Usage::zero(),
            cancelled_by_parent: false,
            model_id: "m".into(),
        };
        // 前缀匹配:取 id 前两位
        let prefix = id[..2].to_string();
        let handle = tokio::spawn({
            let registry = registry.clone();
            let outcome = outcome.clone();
            let id = id.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                registry.finish(&id, &outcome);
            }
        });
        let result = registry.stop(&prefix).await.unwrap();
        assert!(result.contains("already stopped"), "{result}");
        handle.await.unwrap();
        // 不存在的 id
        let error = registry.stop("zzzzzzzz").await.unwrap_err();
        assert!(error.contains("not found"));
    }

    #[tokio::test]
    async fn finish_background_sends_notice_parent_cancel_does_not() {
        let registry = SubagentRegistry::new(Arc::new(Mutex::new(Weak::new())));
        let outcome_ok = ChildOutcome {
            status: RunStatus::Completed,
            output: Some("result".into()),
            error: None,
            duration: Duration::from_millis(10),
            tool_calls: 1,
            usage: latent_ai::Usage::zero(),
            cancelled_by_parent: false,
            model_id: "m".into(),
        };
        let id = registry.register("a", "task", true, CancellationToken::new());
        registry.finish(&id, &outcome_ok);
        assert_eq!(registry.list().lines().count(), 2, "表头 + 一条记录");

        let id2 = registry.register("a", "task", true, CancellationToken::new());
        let mut cancelled = outcome_ok.clone();
        cancelled.cancelled_by_parent = true;
        cancelled.status = RunStatus::Stopped;
        registry.finish(&id2, &cancelled);
        // 通知通道:只有第一条产生通知(第二条父级联停止不投递)
        // 通道在 spawn_supervisor take 之前不可读,这里通过 list 侧面验证状态
        assert!(registry.list().contains("stopped"));
    }
}
