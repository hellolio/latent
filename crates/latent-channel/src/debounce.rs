//! 入站防抖合并(上游 `src/auto-reply/inbound-debounce.ts` —— 语义逐条
//! 对照过):
//!
//! - 按 `(platform, conversation_id, user_id)` 缓冲合并连发消息;默认窗口
//!   **0ms**(即默认不合并),配置优先级:per-channel 覆盖
//!   (`messages.queue.debounceMsByChannel`)> 全局 `messages.queue.debounceMs`;
//! - **窗口在首条到达时固定,后续消息不得顺延**;最大等待 =
//!   `debounceMs × 5`([`MAX_DEBOUNCE_WINDOW_MULTIPLIER`],上限契约);
//! - 跟踪键上限 **2048**([`DEFAULT_MAX_TRACKED_KEYS`]),超限丢最旧;
//! - 命令消息(`/` 开头)与带 At 的消息**立即冲刷缓冲不等待**(对齐
//!   `shouldDebounceTextInbound` 只防抖纯文本);
//! - flush 产出合并文本(段间 `\n`);合并产物的 `message_id` 用**合成新键**
//!   (`debounce:{首条message_id}`),不复用首条 id —— 首条 id 已在入口
//!   claim,复用会撞去重表。

use std::collections::HashMap;

use crate::types::InboundMessage;

/// 跟踪键上限(超限丢最旧)。
pub const DEFAULT_MAX_TRACKED_KEYS: usize = 2048;
/// 最大等待倍率上限(debounceMs × 5;窗口固定的安全上界契约)。
pub const MAX_DEBOUNCE_WINDOW_MULTIPLIER: u64 = 5;

/// 防抖配置(窗口由调用方按 per-channel > 全局 > 默认解析后传入)。
#[derive(Debug, Clone)]
pub struct DebounceConfig {
    pub window_ms: u64,
    pub max_tracked_keys: usize,
}

impl Default for DebounceConfig {
    fn default() -> Self {
        DebounceConfig {
            window_ms: 0,
            max_tracked_keys: DEFAULT_MAX_TRACKED_KEYS,
        }
    }
}

/// 缓冲区:同一 key 的待合并消息(首条到达时间固定窗口)。
#[derive(Debug)]
struct PendingBuffer {
    messages: Vec<InboundMessage>,
    /// 首条到达即固定,后续消息不得顺延
    expires_at_ms: u64,
}

/// 入站防抖器(纯内存;gateway 每 daemon 一个,消息按到达序喂入)。
#[derive(Debug)]
pub struct InboundDebouncer {
    max_tracked_keys: usize,
    buffers: HashMap<String, PendingBuffer>,
}

fn buffer_key(msg: &InboundMessage) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        msg.platform, msg.chat.conversation_id, msg.sender.user_id
    )
}

impl Default for InboundDebouncer {
    fn default() -> Self {
        Self::new(DebounceConfig::default())
    }
}

impl InboundDebouncer {
    pub fn new(config: DebounceConfig) -> Self {
        InboundDebouncer {
            max_tracked_keys: config.max_tracked_keys.max(1),
            buffers: HashMap::new(),
        }
    }

    /// 喂入一条消息。返回立即可下发的消息(0..=2 条:命令/At 消息先冲刷
    /// 该 key 既有缓冲保持顺序,再透传本条;纯文本进缓冲返回空)。
    pub fn enqueue(
        &mut self,
        msg: InboundMessage,
        now_ms: u64,
        window_ms: u64,
    ) -> Vec<InboundMessage> {
        // 窗口 0 = 不合并,直接透传(仍先冲刷既有缓冲保序)
        if window_ms == 0 || msg.is_command() || msg.has_at_segment() {
            let mut out = self
                .flush_key(&buffer_key(&msg))
                .into_iter()
                .collect::<Vec<_>>();
            out.push(msg);
            return out;
        }

        let key = buffer_key(&msg);
        match self.buffers.get_mut(&key) {
            Some(buffer) => {
                // 窗口在首条到达时固定,后续消息不顺延(安全上界 =
                // window × MAX_DEBOUNCE_WINDOW_MULTIPLIER,常量导出供验收)
                buffer.messages.push(msg);
            }
            None => {
                self.buffers.insert(
                    key,
                    PendingBuffer {
                        messages: vec![msg],
                        expires_at_ms: now_ms + window_ms,
                    },
                );
                self.evict_overflow();
            }
        }
        Vec::new()
    }

    /// 冲刷全部到期缓冲(按 key 插入序产出;gateway 定时器按 next_deadline
    /// 调度)。
    pub fn flush_due(&mut self, now_ms: u64) -> Vec<InboundMessage> {
        let due: Vec<String> = self
            .buffers
            .iter()
            .filter(|(_, buffer)| buffer.expires_at_ms <= now_ms)
            .map(|(key, _)| key.clone())
            .collect();
        let mut out = Vec::new();
        for key in due {
            if let Some(flushed) = self.flush_key(&key) {
                out.push(flushed);
            }
        }
        out
    }

    /// 冲刷单个 key(产出合并消息;缓冲空/不存在返回 None)。
    pub fn flush_key(&mut self, key: &str) -> Option<InboundMessage> {
        let buffer = self.buffers.remove(key)?;
        merge_messages(buffer.messages)
    }

    /// 下一个到期时间(gateway 定时器调度用)。
    pub fn next_deadline(&self) -> Option<u64> {
        self.buffers.values().map(|b| b.expires_at_ms).min()
    }

    /// 当前跟踪键数(断言/测试用)。
    pub fn tracked(&self) -> usize {
        self.buffers.len()
    }

    /// 超限丢最旧(按窗口最早到期近似首到序)。
    fn evict_overflow(&mut self) {
        while self.buffers.len() > self.max_tracked_keys {
            let oldest = self
                .buffers
                .iter()
                .min_by_key(|(_, buffer)| buffer.expires_at_ms)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    self.buffers.remove(&key);
                }
                None => break,
            }
        }
    }
}

/// 合并产出:文本段间 `\n`;`message_id` 用合成新键 `debounce:{首条}`;
/// to_me/reply_to_me 取"任一为真";raw 合并为数组。
fn merge_messages(messages: Vec<InboundMessage>) -> Option<InboundMessage> {
    let mut iter = messages.into_iter();
    let mut first = iter.next()?;
    let mut segments = std::mem::take(&mut first.segments);
    let mut texts = vec![std::mem::take(&mut first.text)];
    let mut raws = vec![std::mem::take(&mut first.raw)];
    let mut to_me = first.to_me;
    let mut reply_to_me = first.reply_to_me;
    for msg in iter {
        to_me |= msg.to_me;
        reply_to_me |= msg.reply_to_me;
        segments.extend(msg.segments);
        texts.push(msg.text);
        raws.push(msg.raw);
    }
    first.text = texts.join("\n");
    first.segments = segments;
    first.to_me = to_me;
    first.reply_to_me = reply_to_me;
    first.raw = serde_json::Value::Array(raws);
    first.message_id = format!("debounce:{}", first.message_id);
    Some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatRef, Sender};

    fn msg(id: &str, text: &str) -> InboundMessage {
        InboundMessage::plain_text(
            "qq",
            ChatRef::group("qq", "12345"),
            Sender {
                user_id: "u1".into(),
                display_name: "张三".into(),
            },
            id,
            text,
            true,
        )
    }

    fn at_msg(id: &str, text: &str) -> InboundMessage {
        let mut m = msg(id, text);
        m.segments.push(crate::types::Segment::at("10000"));
        m
    }

    #[test]
    fn zero_window_passes_through() {
        let mut d = InboundDebouncer::default();
        let out = d.enqueue(msg("m1", "a"), 1000, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_id, "m1");
        assert_eq!(d.tracked(), 0);
    }

    #[test]
    fn buffers_then_flushes_merged_with_synthetic_id() {
        let mut d = InboundDebouncer::default();
        assert!(d.enqueue(msg("m1", "a"), 1000, 500).is_empty());
        assert!(d.enqueue(msg("m2", "b"), 1100, 500).is_empty());
        // 窗口固定在首条到达:1000 + 500 = 1500;m2 到达(1100)不得顺延
        assert_eq!(d.next_deadline(), Some(1500));

        let out = d.flush_due(1499);
        assert!(out.is_empty(), "未到期不产出");
        let out = d.flush_due(1500);
        assert_eq!(out.len(), 1);
        // 合成新键,不复用首条 id(首条 id 已在入口 claim)
        assert_eq!(out[0].message_id, "debounce:m1");
        assert_eq!(out[0].text, "a\nb");
        assert_eq!(d.tracked(), 0);
    }

    #[test]
    fn command_and_at_flush_buffer_immediately() {
        let mut d = InboundDebouncer::default();
        assert!(d.enqueue(msg("m1", "a"), 1000, 500).is_empty());
        // 命令消息:先冲刷缓冲(保序),命令本身透传
        let out = d.enqueue(msg("m2", "/status"), 1050, 500);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].message_id, "debounce:m1");
        assert_eq!(out[0].text, "a");
        assert_eq!(out[1].message_id, "m2");
        assert_eq!(out[1].text, "/status");

        // 带 At 的消息同样立即冲刷
        assert!(d.enqueue(msg("m3", "b"), 1100, 500).is_empty());
        let out = d.enqueue(at_msg("m4", "在吗"), 1150, 500);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].message_id, "debounce:m3");
        assert!(out[1].has_at_segment());
    }

    #[test]
    fn tracked_key_overflow_evicts_oldest() {
        let mut d = InboundDebouncer::new(DebounceConfig {
            window_ms: 100,
            max_tracked_keys: 2,
        });
        // key1(q 群 u1)→ key2(q 群 u2)→ key3(q 群 u3):key1 被逐出
        let m1 = msg("m1", "a");
        assert!(d.enqueue(m1, 1000, 100).is_empty());
        let mut m2 = msg("m2", "b");
        m2.sender.user_id = "u2".into();
        assert!(d.enqueue(m2, 1100, 100).is_empty());
        let mut m3 = msg("m3", "c");
        m3.sender.user_id = "u3".into();
        assert!(d.enqueue(m3, 1200, 100).is_empty());
        assert_eq!(d.tracked(), 2, "超限丢最旧");
        // u1 的缓冲已被逐出,到期不再产出
        let out = d.flush_due(2000);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|m| m.sender.user_id != "u1"));
    }

    #[test]
    fn flush_key_is_idempotent_per_key() {
        let mut d = InboundDebouncer::default();
        assert!(d.enqueue(msg("m1", "a"), 1000, 100).is_empty());
        let first = d
            .flush_key("qq\u{0}12345\u{0}u1")
            .expect("flush 产出合并消息");
        assert_eq!(first.message_id, "debounce:m1");
        assert!(d.flush_key("qq\u{0}12345\u{0}u1").is_none());
    }

    #[test]
    fn multiplier_constant_matches_upstream() {
        assert_eq!(MAX_DEBOUNCE_WINDOW_MULTIPLIER, 5);
        assert_eq!(DEFAULT_MAX_TRACKED_KEYS, 2048);
    }
}
