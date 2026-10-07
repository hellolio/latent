//! typing 指示器生命周期(上游 `src/channels/typing.ts`、`typing-lifecycle.ts`):
//! keepalive 刷新间隔 **3000ms**;单次 typing 最长 **60000ms(TTL,超时告警
//! 并停止)**;连续失败 **2 次**即停。
//!
//! **没有"typing 完成才发送"的门** —— typing 只是指示器,入队即发(上游
//! 文档明确澄清过,不发明 gate)。平台不支持时渠道侧静默抑制(no-op)。

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::plugin::ChannelSender;
use crate::types::{ChatRef, ChatType};

/// keepalive 刷新间隔。
pub const KEEPALIVE_INTERVAL_MS: u64 = 3_000;
/// 单次 typing 最长持续时间(TTL,超时告警并停止)。
pub const TYPING_TTL_MS: u64 = 60_000;
/// 连续失败上限(达到即停止刷新)。
pub const MAX_CONSECUTIVE_FAILURES: u32 = 2;

/// typingMode(配置 `agents.defaults.typingMode`,默认 `message`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TypingMode {
    /// 从不显示
    Never,
    /// 入站接受即显示
    Instant,
    /// 思考开始即显示
    Thinking,
    /// 首个用户可见回复活动才显示;DM 与被 @ 的群聊即时发(默认)
    #[default]
    Message,
}

impl TypingMode {
    /// 配置串解析(未知值回退默认 message,与 gateway 配置校验共用)。
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("never") => TypingMode::Never,
            Some("instant") => TypingMode::Instant,
            Some("thinking") => TypingMode::Thinking,
            _ => TypingMode::Message,
        }
    }

    /// 给定触发时机是否应开始/继续显示 typing。
    pub fn should_show(
        &self,
        chat_type: ChatType,
        to_me: bool,
        trigger: TypingTrigger,
    ) -> bool {
        match self {
            TypingMode::Never => false,
            TypingMode::Instant => true,
            TypingMode::Thinking => matches!(
                trigger,
                TypingTrigger::InboundAccepted
                    | TypingTrigger::ThinkingStarted
                    | TypingTrigger::ReplyActivity
            ),
            // message:首个可见回复活动才发;DM 与被 @ 的群聊在入站接受时即发
            TypingMode::Message => match trigger {
                TypingTrigger::InboundAccepted => {
                    chat_type == ChatType::Private || to_me
                }
                TypingTrigger::ThinkingStarted | TypingTrigger::ReplyActivity => true,
            },
        }
    }
}

/// typing 生命周期的触发时机(gateway 管线在对应时点询问)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypingTrigger {
    InboundAccepted,
    ThinkingStarted,
    ReplyActivity,
}

/// typing 配置(常量可被单测钉死;interval/ttl/failures 来自上游语义)。
#[derive(Debug, Clone, Copy)]
pub struct TypingConfig {
    pub mode: TypingMode,
    pub interval: Duration,
    pub ttl: Duration,
    pub max_consecutive_failures: u32,
}

impl Default for TypingConfig {
    fn default() -> Self {
        TypingConfig {
            mode: TypingMode::Message,
            interval: Duration::from_millis(KEEPALIVE_INTERVAL_MS),
            ttl: Duration::from_millis(TYPING_TTL_MS),
            max_consecutive_failures: MAX_CONSECUTIVE_FAILURES,
        }
    }
}

/// 活动中的 typing 指示器:持有即持续刷新(keepalive),drop/stop 时发送
/// 关闭信号。连续失败达上限或超 TTL 自动停止(超时打告警)。
pub struct TypingGuard {
    chat: ChatRef,
    sender: ChannelSender,
    cancel: CancellationToken,
}

impl TypingGuard {
    /// 开始显示 typing(立即发送 on,任务每 interval 刷新一次)。
    pub fn start(sender: &ChannelSender, chat: ChatRef, config: &TypingConfig) -> Self {
        let cancel = CancellationToken::new();
        let task_sender = sender.clone();
        let task_chat = chat.clone();
        let task_cancel = cancel.clone();
        let interval = config.interval;
        let ttl = config.ttl;
        let max_failures = config.max_consecutive_failures;
        tokio::spawn(async move {
            let mut failures: u32 = 0;
            let started = tokio::time::Instant::now();
            loop {
                if task_cancel.is_cancelled() {
                    break;
                }
                task_sender.typing(&task_chat, true).await;
                if tokio::time::timeout(interval, task_cancel.cancelled())
                    .await
                    .is_ok()
                {
                    break;
                }
                if started.elapsed() >= ttl {
                    eprintln!(
                        "[latent-channel:typing] TTL {}s 超时,停止刷新(会话仍未结束?)",
                        ttl.as_secs()
                    );
                    break;
                }
                // 失败计数:typing 是 fire-and-forget,失败探测靠命令通道
                // 关闭(渠道任务已停止)
                if task_sender.is_closed() {
                    failures += 1;
                    if failures >= max_failures {
                        eprintln!(
                            "[latent-channel:typing] 连续 {failures} 次失败,停止刷新"
                        );
                        break;
                    }
                } else {
                    failures = 0;
                }
            }
            if !task_cancel.is_cancelled() {
                // 自然停止(TTL/失败):也要关闭指示器
                task_sender.typing(&task_chat, false).await;
            }
        });
        TypingGuard {
            chat,
            sender: sender.clone(),
            cancel,
        }
    }

    /// 停止 typing(发送关闭信号并结束刷新任务)。
    pub async fn stop(self) {
        self.cancel.cancel();
        self.sender.typing(&self.chat, false).await;
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        // 未显式 stop(如 run panic)也关闭指示器
        self.cancel.cancel();
        let chat = self.chat.clone();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            sender.typing(&chat, false).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_mode_parse_falls_back_to_message() {
        assert_eq!(TypingMode::parse(Some("never")), TypingMode::Never);
        assert_eq!(TypingMode::parse(Some("instant")), TypingMode::Instant);
        assert_eq!(TypingMode::parse(Some("thinking")), TypingMode::Thinking);
        assert_eq!(TypingMode::parse(Some("message")), TypingMode::Message);
        assert_eq!(TypingMode::parse(Some("bogus")), TypingMode::Message);
        assert_eq!(TypingMode::parse(None), TypingMode::Message);
    }

    #[test]
    fn message_mode_sends_instantly_for_dm_and_mentioned_group() {
        // DM:入站接受即发
        assert!(TypingMode::Message.should_show(
            ChatType::Private,
            false,
            TypingTrigger::InboundAccepted
        ));
        // 被 @ 的群聊:入站接受即发
        assert!(TypingMode::Message.should_show(
            ChatType::Group,
            true,
            TypingTrigger::InboundAccepted
        ));
        // 未 @ 的群聊:等可见回复活动
        assert!(!TypingMode::Message.should_show(
            ChatType::Group,
            false,
            TypingTrigger::InboundAccepted
        ));
        assert!(TypingMode::Message.should_show(
            ChatType::Group,
            false,
            TypingTrigger::ReplyActivity
        ));
    }

    #[test]
    fn never_never_shows() {
        for trigger in [
            TypingTrigger::InboundAccepted,
            TypingTrigger::ThinkingStarted,
            TypingTrigger::ReplyActivity,
        ] {
            assert!(!TypingMode::Never.should_show(ChatType::Private, true, trigger));
        }
    }

    #[test]
    fn constants_match_upstream() {
        assert_eq!(KEEPALIVE_INTERVAL_MS, 3_000);
        assert_eq!(TYPING_TTL_MS, 60_000);
        assert_eq!(MAX_CONSECUTIVE_FAILURES, 2);
    }
}
