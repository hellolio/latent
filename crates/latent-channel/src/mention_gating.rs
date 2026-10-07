//! @机器人判定(纯函数;上游 `src/channels/mention-gating.ts`、
//! `src/auto-reply/reply/mentions.ts`、docs `channels/groups.md`)。
//!
//! 判定规则:私聊恒 `to_me`;群聊需显式 At 段 == 机器人 id,**或** reply
//! 引用的消息来自机器人(`reply_to_me`),**或**文本命中 mentionPatterns
//! (大小写不敏感正则,优先级:agent 级 > `messages.groupChat.mentionPatterns`
//! > 由机器人 identity.name 派生 —— 见 [`resolve_mention_patterns`])。

use regex::{Regex, RegexBuilder};

use crate::types::{ChatType, InboundMessage, Segment};

/// @判定配置(调用方把各层配置归并后传入)。
#[derive(Debug, Clone)]
pub struct MentionConfig {
    /// 群聊是否要求显式 @(`messages.groupChat.requireMention`,默认 true)
    pub require_mention: bool,
    /// mentionPatterns 正则串(agent 级 > groupChat 级归并后的最终列表)
    pub mention_patterns: Vec<String>,
    /// 机器人自身账号 id 列表(At 段命中其一 = 被 @)
    pub self_ids: Vec<String>,
    /// 机器人 identity.name(mentionPatterns 全空时派生兜底)
    pub identity_name: Option<String>,
    /// mentionPatterns 构造期预编译(P2-9):`Regex::new` 是重操作,逐条
    /// 群消息重新编译既慢又重复诊断非法 pattern;None = 未预编译,回退
    /// 逐条编译(兼容路径,tests 用)
    pub compiled_patterns: Option<Vec<Regex>>,
}

impl MentionConfig {
    /// 构造入口:patterns 在**构造期**预编译一次(非法 pattern 编译期诊断)。
    pub fn new(
        require_mention: bool,
        mention_patterns: Vec<String>,
        self_ids: Vec<String>,
        identity_name: Option<String>,
    ) -> Self {
        let compiled_patterns = build_mention_regexes(&mention_patterns, |pattern, error| {
            eprintln!("[latent-channel:mention] 非法 mentionPattern `{pattern}`: {error}");
        });
        MentionConfig {
            require_mention,
            mention_patterns,
            self_ids,
            identity_name,
            compiled_patterns: Some(compiled_patterns),
        }
    }
}

// A3(§10):带语义默认值的 Default 禁止 derive —— derive 会把
// require_mention 抹成 false(上游默认 true,fail-closed)
#[allow(clippy::derivable_impls)]
impl Default for MentionConfig {
    fn default() -> Self {
        MentionConfig {
            require_mention: true,
            mention_patterns: Vec::new(),
            self_ids: Vec::new(),
            identity_name: None,
            compiled_patterns: None,
        }
    }
}

/// 判定结论:`ToMe` = 触发 run;`RoomContext` = 仅作房间上下文(MVP 丢弃
/// 并记日志)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionDecision {
    ToMe,
    RoomContext,
}

/// mentionPatterns 优先级归并:agent 级(非空即用)> groupChat 级 >
/// identity.name 派生(`\b{name}\b` 形式,由调用方转义后传入也行 —— 这里
/// 统一做字面转义)。
pub fn resolve_mention_patterns(
    agent_patterns: &[String],
    group_patterns: &[String],
    identity_name: Option<&str>,
) -> Vec<String> {
    if !agent_patterns.is_empty() {
        return agent_patterns.to_vec();
    }
    if !group_patterns.is_empty() {
        return group_patterns.to_vec();
    }
    match identity_name {
        Some(name) if !name.trim().is_empty() => {
            vec![regex::escape(name.trim())]
        }
        _ => Vec::new(),
    }
}

/// 编译 mentionPatterns(大小写不敏感);非法正则跳过并诊断
/// (回调收 (pattern, error),与 .latentignore 坏行同风格)。
pub fn build_mention_regexes(
    patterns: &[String],
    mut on_error: impl FnMut(&str, &str),
) -> Vec<Regex> {
    let mut regexes = Vec::new();
    for pattern in patterns {
        match RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(regex) => regexes.push(regex),
            Err(error) => on_error(pattern, &error.to_string()),
        }
    }
    regexes
}

/// @判定主入口(纯函数)。
pub fn resolve_mention(msg: &InboundMessage, cfg: &MentionConfig) -> MentionDecision {
    // 私聊恒 to_me(上游语义:DM 不需要 @)
    if msg.chat.chat_type == ChatType::Private {
        return MentionDecision::ToMe;
    }
    // requireMention=false:群消息全部视为 to_me(机器人参与所有话题)
    if !cfg.require_mention {
        return MentionDecision::ToMe;
    }
    if is_mentioned(msg, cfg) {
        MentionDecision::ToMe
    } else {
        MentionDecision::RoomContext
    }
}

/// 群聊是否被显式 @ / 回复 / 模式命中。
pub fn is_mentioned(msg: &InboundMessage, cfg: &MentionConfig) -> bool {
    // 显式 At 段 == 机器人 id(或 @全体,平台把 all 视为唤醒)
    if msg.segments.iter().any(|segment| match segment {
        Segment::At { user_id } => {
            user_id == "all" || cfg.self_ids.iter().any(|id| id == user_id)
        }
        _ => false,
    }) {
        return true;
    }
    // reply 引用的消息来自机器人
    if msg.reply_to_me {
        return true;
    }
    // 文本命中 mentionPatterns(大小写不敏感;优先用构造期预编译,P2-9)
    match &cfg.compiled_patterns {
        Some(regexes) => regexes.iter().any(|regex| regex.is_match(&msg.text)),
        None => {
            let regexes = build_mention_regexes(&cfg.mention_patterns, |pattern, error| {
                eprintln!("[latent-channel:mention] 非法 mentionPattern `{pattern}`: {error}");
            });
            regexes.iter().any(|regex| regex.is_match(&msg.text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatRef, Sender};

    fn group_msg(text: &str, at_bot: bool, reply_to_me: bool) -> InboundMessage {
        let mut segments = vec![Segment::text(text.to_string())];
        if at_bot {
            segments.push(Segment::at("10000"));
        }
        InboundMessage {
            platform: "qq",
            chat: ChatRef::group("qq", "12345"),
            sender: Sender {
                user_id: "u1".into(),
                display_name: "张三".into(),
            },
            message_id: "m-1".into(),
            segments,
            text: text.into(),
            to_me: at_bot || reply_to_me,
            reply_to_me,
            raw: serde_json::Value::Null,
        }
    }

    fn cfg() -> MentionConfig {
        MentionConfig {
            mention_patterns: Vec::new(),
            self_ids: vec!["10000".into()],
            identity_name: Some("小龙虾".into()),
            ..Default::default()
        }
    }

    #[test]
    fn private_chat_is_always_to_me() {
        let msg = InboundMessage::plain_text(
            "qq",
            ChatRef::private("qq", "u1"),
            Sender {
                user_id: "u1".into(),
                display_name: "u".into(),
            },
            "m-2",
            "随便说点什么",
            true,
        );
        assert_eq!(resolve_mention(&msg, &cfg()), MentionDecision::ToMe);
    }

    #[test]
    fn group_requires_explicit_mention_when_required() {
        assert_eq!(resolve_mention(&group_msg("普通消息", false, false), &cfg()), MentionDecision::RoomContext);
        // At 机器人
        assert_eq!(resolve_mention(&group_msg("帮我看看", true, false), &cfg()), MentionDecision::ToMe);
        // 回复机器人的消息
        assert_eq!(resolve_mention(&group_msg("收到", false, true), &cfg()), MentionDecision::ToMe);
    }

    #[test]
    fn require_mention_false_makes_everything_to_me() {
        let mut config = cfg();
        config.require_mention = false;
        assert_eq!(
            resolve_mention(&group_msg("普通消息", false, false), &config),
            MentionDecision::ToMe
        );
    }

    #[test]
    fn mention_patterns_match_case_insensitively() {
        let mut config = cfg();
        config.mention_patterns = vec![r"\bcoder\b".into()];
        assert_eq!(
            resolve_mention(&group_msg("hey Coder 帮忙", false, false), &config),
            MentionDecision::ToMe
        );
        assert_eq!(
            resolve_mention(&group_msg("hey 小龙虾 帮忙", false, false), &config),
            MentionDecision::RoomContext,
            "显式 patterns 配置后 identity 派生不再兜底"
        );
    }

    #[test]
    fn pattern_priority_agent_over_group_over_identity() {
        let agent = vec!["alpha".into()];
        let group = vec!["beta".into()];
        assert_eq!(
            resolve_mention_patterns(&agent, &group, Some("gamma")),
            vec!["alpha"]
        );
        assert_eq!(
            resolve_mention_patterns(&[], &group, Some("gamma")),
            vec!["beta"]
        );
        assert_eq!(
            resolve_mention_patterns(&[], &[], Some("小龙虾")),
            vec![regex::escape("小龙虾")]
        );
        assert_eq!(resolve_mention_patterns(&[], &[], None), Vec::<String>::new());
    }

    #[test]
    fn invalid_pattern_is_skipped_with_diagnostic() {
        let mut errors = Vec::new();
        let regexes = build_mention_regexes(
            &[r"(?P<".into(), r"ok".into()],
            |pattern, error| errors.push((pattern.to_string(), error.to_string())),
        );
        assert_eq!(regexes.len(), 1);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "(?P<");
    }
}
