//! 四模式队列(上游 `src/auto-reply/reply/queue.ts` + `queue/types.ts` +
//! docs `concepts/queue.md` —— 默认值逐项核实):
//!
//! - `QueueSettings { mode, debounce_ms, cap, drop_policy }`;优先级:会话内
//!   `/queue` 覆盖 > `messages.queue.debounceMsByChannel` > 全局 > 默认
//!   (steer / 500ms / cap 20 / summarize);
//! - 会话忙(run_lock 被占)时入站消息的处置:steer → `session.steer`;
//!   followup → `session.follow_up`;collect → 防抖合并为单条后 follow_up;
//!   interrupt → `session.abort()` 后重新 prompt;
//! - cap 溢出(默认 20):`drop=summarize` 在 latent 的近似实现 = 丢弃最旧 +
//!   发一条合成提示 follow_up(「已丢弃 N 条更早消息」,§8 偏离 3);`old`
//!   直接丢最旧;`new` 拒绝新消息并回执。

use std::collections::VecDeque;

use crate::config::{DropPolicy, QueueMode};

#[derive(Debug, Clone, Copy)]
pub struct QueueSettings {
    pub mode: QueueMode,
    pub debounce_ms: u64,
    pub cap: usize,
    pub drop_policy: DropPolicy,
}

impl Default for QueueSettings {
    fn default() -> Self {
        QueueSettings {
            mode: QueueMode::Steer,
            debounce_ms: 500,
            cap: 20,
            drop_policy: DropPolicy::Summarize,
        }
    }
}

impl QueueSettings {
    /// 会话级 /queue 覆盖(仅覆盖出现的字段)。
    pub fn with_overrides(mut self, mode: Option<QueueMode>, cap: Option<usize>) -> Self {
        if let Some(mode) = mode {
            self.mode = mode;
        }
        if let Some(cap) = cap {
            self.cap = cap;
        }
        self
    }
}

/// 会话忙时网关自管的待处理项(collect 模式的合并缓冲 / interrupt 的重投)。
#[derive(Debug)]
pub struct PendingQueue {
    settings: QueueSettings,
    items: VecDeque<PendingItem>,
    /// 累计丢弃数(summarize 合成提示用)
    dropped_total: usize,
}

#[derive(Debug, Clone)]
pub struct PendingItem {
    pub text: String,
    pub at_ms: u64,
}

/// cap 溢出处置结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverflowAction {
    /// 正常入队
    Enqueued,
    /// 拒绝新消息(drop=new;回执文本)
    Rejected(&'static str),
}

impl PendingQueue {
    pub fn new(settings: QueueSettings) -> Self {
        PendingQueue {
            settings,
            items: VecDeque::new(),
            dropped_total: 0,
        }
    }

    pub fn settings(&self) -> QueueSettings {
        self.settings
    }

    pub fn set_settings(&mut self, settings: QueueSettings) {
        self.settings = settings;
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn dropped_total(&self) -> usize {
        self.dropped_total
    }

    /// 入队(带 cap 溢出处置)。返回回执文本(有的话)。
    pub fn push(&mut self, text: String, at_ms: u64) -> OverflowAction {
        if self.items.len() >= self.settings.cap {
            match self.settings.drop_policy {
                // new:拒绝新消息并回执
                DropPolicy::New => {
                    return OverflowAction::Rejected(
                        "队列已满(cap),本条被丢弃(drop=new)",
                    )
                }
                // old / summarize:丢最旧(仅 summarize 累计提示计数)
                DropPolicy::Old | DropPolicy::Summarize => {
                    self.items.pop_front();
                    if self.settings.drop_policy == DropPolicy::Summarize {
                        self.dropped_total += 1;
                    }
                }
            }
        }
        self.items.push_back(PendingItem { text, at_ms });
        OverflowAction::Enqueued
    }

    /// 取出全部(collect 模式 flush:合并文本,段间 `\n`)。
    pub fn drain_merged(&mut self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let texts: Vec<String> = self
            .items
            .drain(..)
            .map(|item| item.text)
            .collect();
        Some(texts.join("\n"))
    }

    /// summarize 语义的合成提示(丢弃发生后由调用方 follow_up 注入;
    /// **取过即清** —— 同一批丢弃只提示一次)。
    pub fn summarize_notice(&mut self) -> Option<String> {
        if self.dropped_total == 0 {
            return None;
        }
        let notice = format!(
            "[系统提示] 已丢弃 {} 条更早消息(队列容量 {})",
            self.dropped_total, self.settings.cap
        );
        self.dropped_total = 0;
        Some(notice)
    }

    pub fn reset_drop_counter(&mut self) {
        self.dropped_total = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(cap: usize, drop: DropPolicy) -> QueueSettings {
        QueueSettings {
            mode: QueueMode::Collect,
            debounce_ms: 0,
            cap,
            drop_policy: drop,
        }
    }

    #[test]
    fn cap_overflow_drop_new_rejects_with_receipt() {
        let mut queue = PendingQueue::new(settings(2, DropPolicy::New));
        assert_eq!(queue.push("a".into(), 1), OverflowAction::Enqueued);
        assert_eq!(queue.push("b".into(), 2), OverflowAction::Enqueued);
        assert!(matches!(
            queue.push("c".into(), 3),
            OverflowAction::Rejected(_)
        ));
        assert_eq!(queue.drain_merged().unwrap(), "a\nb");
    }

    #[test]
    fn cap_overflow_drop_old_evicts_oldest() {
        let mut queue = PendingQueue::new(settings(2, DropPolicy::Old));
        queue.push("a".into(), 1);
        queue.push("b".into(), 2);
        queue.push("c".into(), 3);
        assert_eq!(queue.drain_merged().unwrap(), "b\nc");
        assert_eq!(queue.dropped_total(), 0, "old 策略不累计提示计数");
        assert!(queue.summarize_notice().is_none(), "old 策略不发合成提示");
    }

    #[test]
    fn cap_overflow_summarize_adds_notice() {
        let mut queue = PendingQueue::new(settings(2, DropPolicy::Summarize));
        queue.push("a".into(), 1);
        queue.push("b".into(), 2);
        queue.push("c".into(), 3);
        assert_eq!(queue.drain_merged().unwrap(), "b\nc");
        let notice = queue.summarize_notice().unwrap();
        assert!(notice.contains("已丢弃 1 条"), "{notice}");
        assert!(queue.summarize_notice().is_none(), "取过即清");
    }

    #[test]
    fn defaults_match_upstream() {
        let settings = QueueSettings::default();
        assert_eq!(settings.mode, QueueMode::Steer);
        assert_eq!(settings.debounce_ms, 500);
        assert_eq!(settings.cap, 20);
        assert_eq!(settings.drop_policy, DropPolicy::Summarize);
    }
}
