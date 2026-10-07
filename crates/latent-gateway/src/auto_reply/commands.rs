//! 聊天命令(§4.7):解析复用 `latent-runtime::slash::parse` 的风格但独立
//! 命令表(聊天端 ≠ TUI 端),正则形态对齐上游(`commands-reset.ts` 的
//! `/^\/(new|reset)(?:\s|$)/i`、`commands-approve.ts` 的 `/^\/?approve(?:\s|$)/i`)。
//!
//! **群会话命令收紧(§8 偏离 11,比上游严)**:groupScope=per-group 的群会话
//! 是**全群共享**的单会话 —— 会话级修改命令(/new /reset /compact /stop
//! /model /thinking /mode /queue /activation)在**群聊中一律仅 owner**;
//! /status /help 所有人;full-access 无论私聊群聊均仅 owner(§5.1)。

use latent_core::{ApprovalDecision, SessionMode};

/// 聊天命令(解析产物;执行在 auto_reply/mod.rs 的 Gateway)。
#[derive(Debug, Clone, PartialEq)]
pub enum ChatCommand {
    New,
    Reset,
    Compact { arg: Option<String> },
    Stop,
    Status,
    Model { arg: Option<String> },
    Thinking { arg: Option<String> },
    Mode { arg: Option<String> },
    Queue { arg: Option<String> },
    Activation { arg: Option<String> },
    Approve { id: u64, decision: ApprovalDecision },
    Help,
}

impl ChatCommand {
    /// 是否需要已存在的会话(执行层据此懒构建)。
    pub fn needs_session(&self) -> bool {
        matches!(
            self,
            ChatCommand::New
                | ChatCommand::Reset
                | ChatCommand::Compact { .. }
                | ChatCommand::Stop
                | ChatCommand::Status
                | ChatCommand::Model { .. }
                | ChatCommand::Thinking { .. }
                | ChatCommand::Mode { .. }
                | ChatCommand::Queue { .. }
        )
    }

    /// 群聊中是否 owner-only(§4.7 偏离 11)。
    pub fn owner_only_in_group(&self) -> bool {
        !matches!(self, ChatCommand::Status | ChatCommand::Help)
    }
}

/// 解析一行聊天文本为命令(未命中 → None)。
pub fn parse(text: &str) -> Option<ChatCommand> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/').unwrap_or(trimmed);
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, Some(arg.trim())),
        None => (rest, None),
    };
    let arg = arg.filter(|arg| !arg.is_empty());
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "new" | "reset" => Some(ChatCommand::New),
        "compact" => Some(ChatCommand::Compact {
            arg: arg.map(str::to_string),
        }),
        "stop" => Some(ChatCommand::Stop),
        "status" => Some(ChatCommand::Status),
        "model" => Some(ChatCommand::Model {
            arg: arg.map(str::to_string),
        }),
        "thinking" => Some(ChatCommand::Thinking {
            arg: arg.map(str::to_string),
        }),
        "mode" => Some(ChatCommand::Mode {
            arg: arg.map(str::to_string),
        }),
        "queue" => Some(ChatCommand::Queue {
            arg: arg.map(str::to_string),
        }),
        "activation" => Some(ChatCommand::Activation {
            arg: arg.map(str::to_string),
        }),
        "help" => Some(ChatCommand::Help),
        "approve" => parse_approve(arg).map(|(id, decision)| ChatCommand::Approve { id, decision }),
        _ => None,
    }
}

/// `/approve <id> <decision>`:id 与 decision 顺序可换(上游同款);decision
/// 别名:allow|once|allow-once / always|allow-always / deny|reject|block。
pub fn parse_approve(arg: Option<&str>) -> Option<(u64, ApprovalDecision)> {
    let arg = arg?;
    let mut id: Option<u64> = None;
    let mut decision: Option<ApprovalDecision> = None;
    for token in arg.split_whitespace() {
        let lowered = token.to_ascii_lowercase();
        if lowered.parse::<u64>().is_ok() && id.is_none() {
            id = lowered.parse::<u64>().ok();
            continue;
        }
        if decision.is_none() {
            decision = parse_decision(&lowered);
        }
    }
    Some((id?, decision?))
}

/// decision 别名表(上游 commands-approve.ts)。
pub fn parse_decision(value: &str) -> Option<ApprovalDecision> {
    match value {
        "allow" | "once" | "allow-once" | "allow_once" | "allowonce" => {
            Some(ApprovalDecision::Approve)
        }
        "always" | "allow-always" | "allow_always" | "allowalways" => {
            Some(ApprovalDecision::ApproveForSession)
        }
        "deny" | "reject" | "block" => Some(ApprovalDecision::Deny),
        _ => None,
    }
}

/// owner 判定:ownerAllowFrom 里含 `"<channel>:<user_id>"`。
pub fn is_owner(platform: &str, user_id: &str, owner_allow_from: &[String]) -> bool {
    let key = format!("{platform}:{user_id}");
    owner_allow_from.iter().any(|entry| entry == &key)
}

/// /mode 参数解析:full-access 仅 owner(执行层判定);此处只做枚举面。
pub fn parse_mode(arg: &str) -> Option<SessionMode> {
    SessionMode::parse(arg)
}

/// 非 owner 发特权命令的统一回执(不暴露命令存在与否的区分)。
pub const PERMISSION_DENIED_TEXT: &str = "该命令仅 owner 可用";

/// /help 清单。
pub fn help_text() -> String {
    "命令清单:\n\
     /new、/reset — 新建会话\n\
     /compact — 压缩上下文\n\
     /stop — 中止当前任务\n\
     /status — 会话状态\n\
     /model <provider/model> — 查看/切换模型\n\
     /thinking <level> — 查看/设置思考级别\n\
     /mode <plan|confirm|full-access> — 切换会话模式\n\
     /queue <steer|followup|collect|interrupt> [cap N] — 队列模式(仅 owner)\n\
     /activation <mention|always> — 按群切换 @ 门(仅 owner)\n\
     /approve <id> <allow-once|allow-always|deny> — 审批应答(仅 owner)\n\
     /help — 本清单\n\
     注:群聊中会话级命令仅 owner 可用"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_and_without_slash_and_case_insensitive() {
        assert_eq!(parse("/new"), Some(ChatCommand::New));
        assert_eq!(parse("/reset "), Some(ChatCommand::New), "/reset 与 /new 同义");
        assert_eq!(parse("/STOP"), Some(ChatCommand::Stop));
        assert_eq!(parse("status"), Some(ChatCommand::Status));
        assert_eq!(
            parse("/compact 保留近期"),
            Some(ChatCommand::Compact {
                arg: Some("保留近期".into())
            })
        );
        assert_eq!(
            parse("/model anthropic/claude-opus-4-6"),
            Some(ChatCommand::Model {
                arg: Some("anthropic/claude-opus-4-6".into())
            })
        );
        assert_eq!(
            parse("/thinking high"),
            Some(ChatCommand::Thinking {
                arg: Some("high".into())
            })
        );
        assert_eq!(
            parse("/mode full-access"),
            Some(ChatCommand::Mode {
                arg: Some("full-access".into())
            })
        );
        assert_eq!(parse("/help"), Some(ChatCommand::Help));
    }

    #[test]
    fn unknown_slash_is_not_a_command() {
        assert_eq!(parse("/foo"), None);
        assert_eq!(parse("/foo bar"), None);
        assert_eq!(parse("普通消息 /not"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn approve_parses_both_orders_and_aliases() {
        let (id, decision) = parse_approve(Some("12 allow-once")).unwrap();
        assert_eq!(id, 12);
        assert_eq!(decision, ApprovalDecision::Approve);
        let (id, decision) = parse_approve(Some("deny 12")).unwrap();
        assert_eq!(id, 12);
        assert_eq!(decision, ApprovalDecision::Deny);
        let (_, decision) = parse_approve(Some("always 3")).unwrap();
        assert_eq!(decision, ApprovalDecision::ApproveForSession);
        assert_eq!(parse_approve(Some("reject 3")).unwrap().1, ApprovalDecision::Deny);
        assert_eq!(parse_approve(Some("block 3")).unwrap().1, ApprovalDecision::Deny);
        // 缺 id 或 decision → None
        assert!(parse_approve(Some("allow")).is_none());
        assert!(parse_approve(Some("12")).is_none());
        assert!(parse_approve(None).is_none());
        // 无斜杠前缀(approve 正则 `/^\/?approve/`)
        assert!(matches!(parse("approve 1 deny"), Some(ChatCommand::Approve { .. })));
    }

    #[test]
    fn owner_matching_is_channel_scoped() {
        let owners = vec!["qq:10000".to_string(), "telegram:123".to_string()];
        assert!(is_owner("qq", "10000", &owners));
        assert!(!is_owner("qq", "10001", &owners));
        assert!(!is_owner("telegram", "10000", &owners), "渠道不匹配不算 owner");
    }

    #[test]
    fn group_owner_only_rules() {
        assert!(ChatCommand::New.owner_only_in_group());
        assert!(ChatCommand::Mode { arg: None }.owner_only_in_group());
        assert!(!ChatCommand::Status.owner_only_in_group());
        assert!(!ChatCommand::Help.owner_only_in_group());
    }

    #[test]
    fn help_lists_every_command() {
        let text = help_text();
        for name in [
            "/new", "/reset", "/compact", "/stop", "/status", "/model", "/thinking", "/mode",
            "/queue", "/activation", "/approve", "/help",
        ] {
            assert!(text.contains(name), "缺少 {name}");
        }
    }
}
