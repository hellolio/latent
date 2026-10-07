//! 跨渠道统一消息模型(收发共用;OneBot 12 / Koichi 风格消息段)。
//!
//! 对外 JSON 字段 camelCase(仓库约定);`platform` 是渠道 id 静态串,
//! 只序列化不反序列化(消息由渠道归一化层在 Rust 侧构造)。

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatType {
    Private,
    Group,
}

/// 会话/聊天标识(跨渠道统一)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRef {
    /// "qq" | "wecom" | "telegram"(== config.rs channels 节的键名)
    pub platform: &'static str,
    pub chat_type: ChatType,
    /// 群号 / 对方 user_id(私聊)
    pub conversation_id: String,
}

impl ChatRef {
    pub fn new(chat_type: ChatType, conversation_id: impl Into<String>) -> Self {
        ChatRef {
            platform: "unknown",
            chat_type,
            conversation_id: conversation_id.into(),
        }
    }

    pub fn private(platform: &'static str, user_id: impl Into<String>) -> Self {
        ChatRef {
            platform,
            chat_type: ChatType::Private,
            conversation_id: user_id.into(),
        }
    }

    pub fn group(platform: &'static str, group_id: impl Into<String>) -> Self {
        ChatRef {
            platform,
            chat_type: ChatType::Group,
            conversation_id: group_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sender {
    pub user_id: String,
    /// 昵称或群名片
    pub display_name: String,
}

/// 消息段(收发共用,OneBot 12 / Koishi 风格)。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Segment {
    Text(String),
    At {
        user_id: String,
    },
    /// "all" = @全体(平台支持时)
    Image {
        url: Option<String>,
        file_id: Option<String>,
    },
    File {
        url: Option<String>,
        file_id: Option<String>,
        name: Option<String>,
    },
    Reply {
        message_id: String,
    },
}

impl Segment {
    pub fn text(t: impl Into<String>) -> Self {
        Segment::Text(t.into())
    }

    pub fn at(user_id: impl Into<String>) -> Self {
        Segment::At {
            user_id: user_id.into(),
        }
    }
}

/// 自消息防环(硬规则):渠道归一化层必须丢弃 sender == 平台自身账号的事件
/// (QQ:OneBot 会上报机器人自己的发言,user_id == self_id;不丢则私聊"恒
/// to_me"直接死循环)。在渠道源头丢弃,不给 InboundMessage 加字段 —— 防环
/// 是渠道责任。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboundMessage {
    pub platform: &'static str,
    pub chat: ChatRef,
    pub sender: Sender,
    /// 渠道侧去重键(OneBot message_id;TG 用 update_id 推导)
    pub message_id: String,
    pub segments: Vec<Segment>,
    /// 纯文本投影(Text 拼接、At→"@名字"、Reply→"[回复]")
    pub text: String,
    /// @机器人 / 回复机器人 / 私聊 —— 渠道归一化时算好
    pub to_me: bool,
    pub reply_to_me: bool,
    /// 原始载荷透传(诊断/扩展用)
    pub raw: serde_json::Value,
}

impl InboundMessage {
    /// 纯文本便捷构造(测试与 mock 渠道用):单 Text 段。
    pub fn plain_text(
        platform: &'static str,
        chat: ChatRef,
        sender: Sender,
        message_id: impl Into<String>,
        text: impl Into<String>,
        to_me: bool,
    ) -> Self {
        let text = text.into();
        InboundMessage {
            platform,
            chat,
            sender,
            message_id: message_id.into(),
            segments: vec![Segment::text(text.clone())],
            text,
            to_me,
            reply_to_me: false,
            raw: serde_json::Value::Null,
        }
    }

    /// 消息段里是否带 At(@机器人 / @任意人)。
    pub fn has_at_segment(&self) -> bool {
        self.segments
            .iter()
            .any(|s| matches!(s, Segment::At { .. }))
    }

    /// 文本是否为聊天命令(`/` 开头;与 auto_reply 命令拦截同形)。
    pub fn is_command(&self) -> bool {
        self.text.trim_start().starts_with('/')
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundMessage {
    pub segments: Vec<Segment>,
    /// 引用的 message_id(平台不支持则忽略)
    pub reply_to: Option<String>,
}

impl OutboundMessage {
    pub fn text(t: impl Into<String>) -> Self {
        OutboundMessage {
            segments: vec![Segment::text(t)],
            reply_to: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub enum ChannelEvent {
    Inbound(InboundMessage),
    Status(ChannelStatus),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ChannelStatus {
    Connected {
        account_id: String,
    },
    /// 可自动重连
    Disconnected {
        reason: String,
    },
    /// 需宿主介入(如凭据失效)
    Failed {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_projects_single_text_segment() {
        let msg = InboundMessage::plain_text(
            "qq",
            ChatRef::group("qq", "12345"),
            Sender {
                user_id: "u1".into(),
                display_name: "张三".into(),
            },
            "m-1",
            "帮我看看",
            true,
        );
        assert_eq!(msg.text, "帮我看看");
        assert_eq!(msg.segments, vec![Segment::text("帮我看看")]);
        assert!(msg.to_me);
        assert!(!msg.reply_to_me);
    }

    #[test]
    fn command_and_at_detection() {
        let mut msg = InboundMessage::plain_text(
            "qq",
            ChatRef::private("qq", "u1"),
            Sender {
                user_id: "u1".into(),
                display_name: "u".into(),
            },
            "m-2",
            "/status",
            true,
        );
        assert!(msg.is_command());
        assert!(!msg.has_at_segment());
        msg.segments.push(Segment::at("10000"));
        assert!(msg.has_at_segment());

        let plain = InboundMessage::plain_text(
            "qq",
            ChatRef::private("qq", "u1"),
            Sender {
                user_id: "u1".into(),
                display_name: "u".into(),
            },
            "m-3",
            "普通消息 /not-command",
            true,
        );
        assert!(!plain.is_command(), "只有行首 / 是命令");
    }

    #[test]
    fn serialization_uses_camel_case() {
        let status = ChannelStatus::Connected {
            account_id: "acct".into(),
        };
        let json = serde_json::to_value(&status).unwrap();
        // 枚举为 externally tagged:{"connected": {"accountId": …}}
        assert_eq!(json["connected"]["accountId"], "acct");
        let chat = ChatRef::private("qq", "u1");
        let json = serde_json::to_value(&chat).unwrap();
        assert_eq!(json["chatType"], "private");
        assert_eq!(json["conversationId"], "u1");
    }
}
