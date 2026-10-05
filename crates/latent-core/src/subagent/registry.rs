//! 运行注册表 + supervisor 空闲唤醒(14 文档 §4.3)。
//!
//! 所有运行(同步/后台)统一登记;后台运行结算后由 supervisor 任务批量唤醒
//! 父会话:收集预算内全部已结算通知 → `wait_idle()` →(打字门控)→
//! `follow_up(合并通知)` → `continue_run()`。若 continue 撞上用户已开启的
//! 新 run(AlreadyRunning),follow_up 留在队列由下一停止点消费——天然兜底,
//! 不丢消息(agent.rs 的 requeue 语义)。
//!
//! 通知抑制是按 run 的标记(`suppress_notice`):/new 与会话退出 abort_all
//! 后,被终止运行的残留结算不再唤醒父会话;其后新登记的运行不受影响
//! (全局布尔会因新 register 复位而让旧 run 的迟到结算泄漏进新会话)。
//!
//! 父 abort 级联停止的运行(cancelled_by_parent)不投递通知:用户按 Esc 后
//! 不应被"已停止"的通知重新拉起新 turn。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use futures::FutureExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use latent_agent::{Agent, AgentError, AgentMessage};

use super::runner::{format_child_result, truncate_chars, ChildOutcome, RunStatus, MAX_OUTPUT_CHARS};

/// 后台活跃上限(14 文档 §4.3)。
pub const MAX_ACTIVE_ASYNC: usize = 16;
/// 运行历史上限(超出淘汰最旧的已完成记录)。
pub const MAX_RUN_HISTORY: usize = 50;
/// 异步完成通知里的结果预览上限。
const NOTIFY_PREVIEW_CHARS: usize = 2_000;
/// stop 后等待结算的轮询上限。
const STOP_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_SETTLE_POLL: Duration = Duration::from_millis(50);
/// 单次唤醒注入的通知文本预算(字符):超出部分留在通道,下轮继续投递
/// ——与单条工具结果的输出预算同源,避免合并通知把上下文一次性撑爆。
const WAKE_BATCH_MAX_CHARS: usize = MAX_OUTPUT_CHARS;
/// 打字门控:用户正在输入时延迟唤醒的轮询间隔与上限(超时后照常唤醒,
/// 用户输入经 steering 兜底,不丢消息)。
const WAKE_GATE_POLL: Duration = Duration::from_millis(250);
const WAKE_GATE_MAX: Duration = Duration::from_secs(60);

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
    /// 通知抑制(按 run):abort_all(/new、退出清理)对运行中的条目置位,
    /// 其残留结算不再投递通知;之后新登记的运行不受影响
    suppress_notice: bool,
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

/// run 查询结果(TUI 异步卡片翻转依据)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// 仍在运行
    Active,
    /// 已结算(success = 是否 Completed)
    Settled { success: bool },
    /// 注册表无此 id(历史超限被淘汰等)
    Unknown,
}

pub struct SubagentRegistry {
    runs: Mutex<Vec<RunEntry>>,
    completions_tx: mpsc::UnboundedSender<CompletionNotice>,
    completions_rx: Mutex<Option<mpsc::UnboundedReceiver<CompletionNotice>>>,
    parent: Arc<Mutex<Weak<Agent>>>,
    /// 已入通道、尚未被 supervisor 投递(continue_run 结束)的通知条数
    /// (非交互模式退出前的等待依据)
    pending_notices: Arc<AtomicUsize>,
    /// 打字门控(交互模式注入):返回 true = 用户正在输入,唤醒延迟。
    /// None = 不门控(非交互模式)。
    wake_gate: Mutex<Option<Arc<dyn Fn() -> bool + Send + Sync>>>,
}

impl SubagentRegistry {
    pub fn new(parent: Arc<Mutex<Weak<Agent>>>) -> Arc<Self> {
        let (completions_tx, completions_rx) = mpsc::unbounded_channel();
        Arc::new(SubagentRegistry {
            runs: Mutex::new(Vec::new()),
            completions_tx,
            completions_rx: Mutex::new(Some(completions_rx)),
            parent,
            pending_notices: Arc::new(AtomicUsize::new(0)),
            wake_gate: Mutex::new(None),
        })
    }

    /// 注入打字门控(交互模式装配后调用一次):true = 用户正在编辑器输入,
    /// supervisor 延迟唤醒(有上限),避免结算通知抢在用户提交前拉起新 turn。
    pub fn set_wake_gate(&self, gate: Arc<dyn Fn() -> bool + Send + Sync>) {
        *self.wake_gate.lock().unwrap() = Some(gate);
    }

    /// 已入通道、尚未投递完成的通知条数(print/json 退出等待用)。
    pub fn pending_notices(&self) -> usize {
        self.pending_notices.load(Ordering::SeqCst)
    }

    /// supervisor 任务(单例):等父空闲 → 打字门控 → follow_up 合并通知 →
    /// 驱动新 run。重复调用是 no-op(接收端只能取一次)。
    pub fn spawn_supervisor(&self) {
        let Some(mut rx) = self.completions_rx.lock().unwrap().take() else {
            return;
        };
        let parent = self.parent.clone();
        let gate = self.wake_gate.lock().unwrap().clone();
        let pending = self.pending_notices.clone();
        let completions_tx = self.completions_tx.clone();
        tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                // 单批处理全程 panic 防护:supervisor 意外终止会让后续通知
                // 永久积压,这里吞掉 panic + 诊断后继续消费(计数可能多计,
                // 只影响退出等待的时长,不影响投递)
                let outcome = std::panic::AssertUnwindSafe(deliver_batch(
                    &parent,
                    &gate,
                    &completions_tx,
                    &pending,
                    &mut rx,
                    first,
                ))
                .catch_unwind()
                .await;
                if let Err(panic) = outcome {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    eprintln!("[latent][subagent] supervisor wake panicked: {message}");
                }
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
            suppress_notice: false,
            stop,
        });
        evict_overflow(&mut runs);
        id
    }

    /// 结算一条运行;后台且非父级联停止、未被抑制时投递完成通知。
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
        if entry.background && !outcome.cancelled_by_parent && !entry.suppress_notice {
            let text = format_child_result(&entry.agent, &entry.id, outcome);
            self.pending_notices.fetch_add(1, Ordering::SeqCst);
            let _ = self.completions_tx.send(CompletionNotice { text });
        }
    }

    /// 按 id(精确匹配)查询运行状态(TUI 异步卡片翻转依据)。
    pub fn run_state(&self, id: &str) -> RunState {
        let runs = self.runs.lock().unwrap();
        match runs.iter().find(|entry| entry.id == id) {
            Some(entry) => match entry.status {
                None => RunState::Active,
                Some(status) => RunState::Settled {
                    success: matches!(status, RunStatus::Completed),
                },
            },
            None => RunState::Unknown,
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
            match match_entry(&runs, id) {
                Ok(Some(entry)) => (Some(entry.clone()), 0),
                Ok(None) => (None, 0),
                Err(ambiguity) => (None, ambiguity),
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

    /// action:"list":运行记录表(仅状态面;结果内容只经推送送达,避免
    /// 拉取与推送重复注入同一份输出)。
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
        let mut runs = self.runs.lock().unwrap();
        for entry in runs.iter_mut() {
            if entry.status.is_none() {
                entry.suppress_notice = true;
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

/// id(或唯一前缀)匹配;Err = 前缀命中多条(歧义数)。
fn match_entry<'a>(runs: &'a [RunEntry], id: &str) -> Result<Option<&'a RunEntry>, usize> {
    let matches: Vec<&RunEntry> = runs
        .iter()
        .filter(|entry| entry.id == id || entry.id.starts_with(id))
        .collect();
    if matches.iter().any(|entry| entry.id == id) {
        Ok(matches.into_iter().find(|e| e.id == id))
    } else if matches.len() == 1 {
        Ok(Some(matches[0]))
    } else if matches.len() > 1 {
        Err(matches.len())
    } else {
        Ok(None)
    }
}

/// 预算内批量收集:从通道 try_recv 尽可能多的通知合并为一批,超预算的
/// 第一条推回通道并停止(硬上限;单批至少含 first)。
fn collect_batch(
    rx: &mut mpsc::UnboundedReceiver<CompletionNotice>,
    completions_tx: &mpsc::UnboundedSender<CompletionNotice>,
    first: CompletionNotice,
) -> Vec<String> {
    let mut texts = vec![first.text];
    let mut total = chars_of(&texts[0]);
    while let Ok(next) = rx.try_recv() {
        let len = chars_of(&next.text);
        if total + len > WAKE_BATCH_MAX_CHARS {
            // 超预算:推回通道(仅 supervisor 自己 recv,顺序至多与并发
            // 新通知交错;条目自描述,乱序无碍)
            let _ = completions_tx.send(next);
            break;
        }
        total += len;
        texts.push(next.text);
    }
    texts
}

/// 投递一批结算通知:预算内尽量合并 → 等父空闲 → 打字门控 → follow_up →
/// continue_run。pending 计数在投递尝试结束后按实际合并条数递减
/// (finish 侧逐条递增)。
async fn deliver_batch(
    parent: &Arc<Mutex<Weak<Agent>>>,
    gate: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    completions_tx: &mpsc::UnboundedSender<CompletionNotice>,
    pending: &AtomicUsize,
    rx: &mut mpsc::UnboundedReceiver<CompletionNotice>,
    first: CompletionNotice,
) {
    let texts = collect_batch(rx, completions_tx, first);
    let batch_len = texts.len();
    let Some(agent) = parent.lock().unwrap().upgrade() else {
        // 父会话已释放(换会话/退出):通知无处投递,丢弃并回落计数
        pending.fetch_sub(batch_len, Ordering::SeqCst);
        return;
    };
    agent.wait_idle().await;
    // 打字门控:用户正在输入时延迟唤醒(有上限),避免结算通知抢在用户
    // 提交前拉起新 turn;超时后照常投递(用户输入经 steering 兜底不丢)
    let deadline = Instant::now() + WAKE_GATE_MAX;
    loop {
        let composing = gate.as_ref().is_some_and(|g| g());
        if !composing || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(WAKE_GATE_POLL).await;
    }
    agent.follow_up(AgentMessage::user(texts.join("\n\n")));
    // 已有活动 run(用户恰在交互)时失败:通知已在 follow_up 队列,
    // 下一停止点由循环整流消费
    match agent.continue_run().await {
        Ok(_) | Err(AgentError::AlreadyRunning) | Err(AgentError::NothingToContinue) => {}
        Err(error) => eprintln!("[latent][subagent] wake continue failed: {error}"),
    }
    pending.fetch_sub(batch_len, Ordering::SeqCst);
}

fn chars_of(text: &str) -> usize {
    text.chars().count()
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
            suppress_notice: false,
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
        assert_eq!(registry.pending_notices(), 1, "正常结算入队一条通知");
        assert_eq!(registry.list().lines().count(), 2, "表头 + 一条记录");

        let id2 = registry.register("a", "task", true, CancellationToken::new());
        let mut cancelled = outcome_ok.clone();
        cancelled.cancelled_by_parent = true;
        cancelled.status = RunStatus::Stopped;
        registry.finish(&id2, &cancelled);
        // 父级联停止不投递:pending 计数不变
        assert_eq!(registry.pending_notices(), 1);
        assert!(registry.list().contains("stopped"));
    }

    #[tokio::test]
    async fn abort_all_suppression_is_per_run() {
        // /new 场景:abort_all 后新登记的运行不继承抑制,旧 run 的迟到
        // 结算仍被抑制(全局布尔会在新 register 时复位,导致旧通知泄漏)
        let registry = SubagentRegistry::new(Arc::new(Mutex::new(Weak::new())));
        let old = registry.register("a", "old task", true, CancellationToken::new());
        registry.abort_all();
        let new = registry.register("a", "new task", true, CancellationToken::new());
        let outcome = ChildOutcome {
            status: RunStatus::Stopped,
            output: None,
            error: None,
            duration: Duration::from_millis(5),
            tool_calls: 0,
            usage: latent_ai::Usage::zero(),
            cancelled_by_parent: false,
            model_id: "m".into(),
        };
        registry.finish(&old, &outcome);
        assert_eq!(registry.pending_notices(), 0, "旧 run 迟到结算被抑制");
        registry.finish(&new, &outcome);
        assert_eq!(registry.pending_notices(), 1, "新登记的运行正常通知");
    }

    #[test]
    fn run_state_reports_active_settled_unknown() {
        let registry = SubagentRegistry::new(Arc::new(Mutex::new(Weak::new())));
        let id = registry.register("a", "task", true, CancellationToken::new());
        assert_eq!(registry.run_state(&id), RunState::Active);
        let outcome = ChildOutcome {
            status: RunStatus::Failed,
            output: None,
            error: Some("boom".into()),
            duration: Duration::from_millis(5),
            tool_calls: 0,
            usage: latent_ai::Usage::zero(),
            cancelled_by_parent: false,
            model_id: "m".into(),
        };
        registry.finish(&id, &outcome);
        assert_eq!(
            registry.run_state(&id),
            RunState::Settled { success: false }
        );
        assert_eq!(registry.run_state("nope"), RunState::Unknown);
    }

    #[test]
    fn collect_batch_merges_within_budget_and_pushes_back_overflow() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(CompletionNotice { text: "a".repeat(100) }).unwrap();
        tx.send(CompletionNotice { text: "b".repeat(100) }).unwrap();
        // 单条超预算也照收(至少一批一条,预算为软上限起点)
        tx.send(CompletionNotice { text: "c".repeat(WAKE_BATCH_MAX_CHARS + 1) }).unwrap();
        tx.send(CompletionNotice { text: "d".into() }).unwrap();
        let first = rx.try_recv().unwrap();
        let batch = collect_batch(&mut rx, &tx, first);
        assert_eq!(batch.len(), 2, "前两条在预算内合并: {batch:?}");
        // 第三条超预算推回,第四条留在通道:通道里应有 2 条
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}
