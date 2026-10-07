//! session key 计算(上游 `src/routing/session-key.ts`):分隔符统一 `:`。
//! 私聊 dmScope 四档 + 群聊 groupScope 两档;channel/peerId 为空时用
//! `"unknown"` 兜底(上游同款);`agentId` 默认 `main`(上游
//! `LEGACY_IMPLICIT_AGENT_ID`)。

use latent_channel::types::{ChatRef, ChatType};

/// 上游 `LEGACY_IMPLICIT_AGENT_ID`:单 agent MVP 的固定 id。
pub const LEGACY_IMPLICIT_AGENT_ID: &str = "main";
/// channel/peer 缺失时的兜底段(上游同款)。
pub const UNKNOWN_PEER: &str = "unknown";

/// 私聊会话粒度(`session.dmScope`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DmScope {
    /// 全部私聊共用 main 会话
    Main,
    /// 跨渠道按对方身份一人一会话
    PerPeer,
    /// 每渠道 × 对方身份一会话(本项目默认)
    #[default]
    PerChannelPeer,
    /// 渠道账户 × 对方身份(多账户渠道预留)
    PerAccountChannelPeer,
}

impl DmScope {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("main") => DmScope::Main,
            Some("per-peer") => DmScope::PerPeer,
            Some("per-account-channel-peer") => DmScope::PerAccountChannelPeer,
            _ => DmScope::PerChannelPeer,
        }
    }
}

/// 群聊会话粒度(`session.groupScope`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupScope {
    /// 每群一会话(默认)
    #[default]
    PerGroup,
    /// 全部群共用 main 会话
    Main,
}

impl GroupScope {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("main") => GroupScope::Main,
            _ => GroupScope::PerGroup,
        }
    }
}

/// session key 计算主入口。`account_id` 仅 per-account-channel-peer 使用。
pub fn build_session_key(
    agent_id: &str,
    chat: &ChatRef,
    account_id: &str,
    dm_scope: DmScope,
    group_scope: GroupScope,
) -> String {
    let agent = if agent_id.trim().is_empty() {
        LEGACY_IMPLICIT_AGENT_ID
    } else {
        agent_id
    };
    let channel = non_empty_or(chat.platform, UNKNOWN_PEER);
    let peer = non_empty_or(&chat.conversation_id, UNKNOWN_PEER);
    match chat.chat_type {
        ChatType::Group => match group_scope {
            GroupScope::Main => format!("agent:{agent}:main"),
            GroupScope::PerGroup => format!("agent:{agent}:{channel}:group:{peer}"),
        },
        ChatType::Private => match dm_scope {
            DmScope::Main => format!("agent:{agent}:main"),
            DmScope::PerPeer => format!("agent:{agent}:direct:{peer}"),
            DmScope::PerChannelPeer => format!("agent:{agent}:{channel}:direct:{peer}"),
            DmScope::PerAccountChannelPeer => {
                let account = non_empty_or(account_id, UNKNOWN_PEER);
                format!("agent:{agent}:{channel}:{account}:direct:{peer}")
            }
        },
    }
    // 线程/话题(二期):追加 `:thread:{threadId}`,结构预留
}

fn non_empty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_key_per_group() {
        let chat = ChatRef::group("qq", "12345");
        assert_eq!(
            build_session_key("main", &chat, "", DmScope::default(), GroupScope::PerGroup),
            "agent:main:qq:group:12345"
        );
    }

    #[test]
    fn group_scope_main_collapses() {
        let chat = ChatRef::group("qq", "12345");
        assert_eq!(
            build_session_key("main", &chat, "", DmScope::default(), GroupScope::Main),
            "agent:main:main"
        );
    }

    #[test]
    fn dm_scopes() {
        let chat = ChatRef::private("telegram", "123456789");
        assert_eq!(
            build_session_key("main", &chat, "acct1", DmScope::Main, GroupScope::default()),
            "agent:main:main"
        );
        assert_eq!(
            build_session_key("main", &chat, "acct1", DmScope::PerPeer, GroupScope::default()),
            "agent:main:direct:123456789"
        );
        assert_eq!(
            build_session_key("main", &chat, "acct1", DmScope::PerChannelPeer, GroupScope::default()),
            "agent:main:telegram:direct:123456789"
        );
        assert_eq!(
            build_session_key(
                "main",
                &chat,
                "acct1",
                DmScope::PerAccountChannelPeer,
                GroupScope::default()
            ),
            "agent:main:telegram:acct1:direct:123456789"
        );
    }

    #[test]
    fn unknown_fallbacks_and_default_agent() {
        // 空 channel/peer → unknown;空 agent → main
        let chat = ChatRef::new(ChatType::Private, "");
        assert_eq!(
            build_session_key("", &chat, "", DmScope::PerChannelPeer, GroupScope::default()),
            "agent:main:unknown:direct:unknown"
        );
    }
}
