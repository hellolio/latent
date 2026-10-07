//! 入站消息包装(上游 `src/auto-reply/envelope.ts` 的
//! `formatInboundEnvelope`/`formatAgentEnvelope`):
//!
//! - DM + 自己的消息 → `(self): body`;DM → `{sender}: body`;群 →
//!   `{sender}: body`;
//! - 外层方括号头:`[{channel} {chat_type}:{conversation_id} {sender} {HH:mm}] body`;
//!   头部字段做 `sanitizeEnvelopeHeaderPart` 式清洗(替换方括号,防伪造层级)。

use chrono::Local;

use crate::approval::display_name_or_id;
use latent_channel::types::{ChatType, InboundMessage};

/// 单段头部清洗:方括号替换为圆括号,防伪造外层层级。
pub fn sanitize_envelope_header_part(part: &str) -> String {
    part.replace('[', "(").replace(']', ")")
}

/// 入站消息 → 发给模型上下文的包装文本(一次 run 的 user 消息)。
pub fn format_inbound_envelope(msg: &InboundMessage, from_self: bool) -> String {
    let now = Local::now();
    let body = if msg.text.trim().is_empty() {
        "(空消息,仅附件)".to_string()
    } else {
        msg.text.trim().to_string()
    };
    let sender_part = if from_self {
        "(self)".to_string()
    } else {
        sanitize_envelope_header_part(display_name_or_id(&msg.sender)).to_string()
    };
    let channel = sanitize_envelope_header_part(msg.platform);
    let chat_label = match msg.chat.chat_type {
        ChatType::Group => format!(
            "group:{}",
            sanitize_envelope_header_part(&msg.chat.conversation_id)
        ),
        ChatType::Private => format!(
            "dm:{}",
            sanitize_envelope_header_part(&msg.chat.conversation_id)
        ),
    };
    let time = now.format("%H:%M").to_string();
    format!("[{channel} {chat_label} {sender_part} {time}] {body}")
}

/// `chat_type` 在信封里的展示串。
pub fn chat_type_label(chat_type: ChatType) -> &'static str {
    match chat_type {
        ChatType::Private => "dm",
        ChatType::Group => "group",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use latent_channel::types::{ChatRef, Sender};

    fn msg() -> InboundMessage {
        InboundMessage::plain_text(
            "qq",
            ChatRef::group("qq", "12345"),
            Sender {
                user_id: "u1".into(),
                display_name: "张三".into(),
            },
            "m-1",
            "帮我看看 src/login.rs",
            true,
        )
    }

    #[test]
    fn group_envelope_has_bracket_header() {
        let text = format_inbound_envelope(&msg(), false);
        assert!(text.starts_with('['), "{text}");
        assert!(text.contains("qq group:12345"), "{text}");
        assert!(text.contains("张三"), "{text}");
        assert!(text.ends_with("帮我看看 src/login.rs"), "{text}");
        // HH:mm 头(时间格式)
        assert!(text.contains(']'), "{text}");
    }

    #[test]
    fn self_messages_use_self_marker() {
        let text = format_inbound_envelope(&msg(), true);
        assert!(text.contains("(self)"), "{text}");
        assert!(!text.contains("张三"), "{text}");
    }

    #[test]
    fn header_parts_are_sanitized() {
        let mut m = msg();
        m.sender.display_name = "[假]层".into();
        let text = format_inbound_envelope(&m, false);
        assert!(!text.contains("[假]层"), "伪造层级应被清洗: {text}");
        assert!(text.contains("(假)层"), "{text}");
    }

    #[test]
    fn empty_text_falls_back_to_attachment_hint() {
        let mut m = msg();
        m.text = String::new();
        let text = format_inbound_envelope(&m, false);
        assert!(text.contains("(空消息,仅附件)"), "{text}");
    }

    #[test]
    fn chat_type_labels() {
        assert_eq!(chat_type_label(ChatType::Private), "dm");
        assert_eq!(chat_type_label(ChatType::Group), "group");
    }
}
