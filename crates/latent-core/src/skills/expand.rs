//! convert_to_llm 出口的 skill 展开(纯函数,转录 → LLM 消息的后处理):
//!
//! - **load_skill 工具结果**:正文(已由工具包成 `<skill name="…">` 形态)原样
//!   搬进紧随其后的 developer 消息,工具结果本身换成短确认 —— 模型上下文里
//!   技能正文以 developer 角色出现且不双份占 token;转录/UI 仍是工具全文。
//! - **`/skill <名称>` 消息前缀**(输入框伪命令,与工具调用无关):在 user
//!   消息之前插入 developer 消息(调用时读 SKILL.md,即时生效),user 文本
//!   剥掉前缀;查无此技能/读取失败/正文为空 → 不注入、文本原样保留。
//!
//! 两条路径均为转录的确定性推导,不改转录、不新增持久化形态("转录即真相"
//! 保持:技能正文按请求时磁盘现读,会话中途修改 SKILL.md 即时生效)。

use latent_ai::{ContentBlock, Message, UserContent};

use super::defs::SkillDef;
use super::tool::TOOL_NAME;

/// load_skill 工具结果在模型视图中的短确认(转录里仍是 `<skill>` 全文)
const LOAD_CONFIRMATION: &str =
    "Skill loaded; its full instructions follow as a developer message.";

/// load_skill 工具结果 → [短确认 toolResult, developer(正文)]。
/// 非文本块(details 类)与错误结果原样保留;正文为空只留确认。
fn expand_load_skill_tool_result(
    tool_call_id: String,
    tool_name: String,
    content: Vec<ContentBlock>,
    details: Option<serde_json::Value>,
    is_error: bool,
    timestamp: i64,
    out: &mut Vec<Message>,
) {
    let text = content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join("");
    out.push(Message::ToolResult {
        tool_call_id,
        tool_name,
        content: vec![ContentBlock::text(LOAD_CONFIRMATION)],
        details,
        is_error,
        timestamp,
    });
    if !text.trim().is_empty() {
        out.push(Message::developer(text));
    }
}

/// 解析消息开头的 `/skill <名称>` 前缀(仅开头,连续多个逐一展开)。
/// 返回 (技能名, 剥掉前缀后的正文);非该形态返回 None。
/// `/skills` 等其他词不误伤(名称前必须有空白);裸 `/skill`(无名称)不算。
fn parse_leading_skill_token(text: &str) -> Option<(&str, &str)> {
    let after = text.strip_prefix("/skill")?;
    let mut chars = after.char_indices();
    // `/skill` 后必须紧跟空白(否则是别的词)
    let (_, ws) = chars.next()?;
    if !ws.is_whitespace() {
        return None;
    }
    let after_ws = after[ws.len_utf8()..].trim_start();
    let name_end = after_ws.find(char::is_whitespace).unwrap_or(after_ws.len());
    if name_end == 0 {
        return None;
    }
    let name = &after_ws[..name_end];
    let rest = after_ws[name_end..].trim_start();
    Some((name, rest))
}

/// 读取并包裹技能正文:`<skill name="…">\n全文\n</skill>`。
fn skill_developer_message(name: &str, def: &SkillDef) -> Option<Message> {
    // 调用时读取而非启动缓存:会话中途修改 SKILL.md 即时生效
    let body = std::fs::read_to_string(&def.path).ok()?;
    Some(Message::developer(format!(
        "<skill name=\"{name}\">\n{body}\n</skill>"
    )))
}

/// 展开一条 user 消息的 `/skill <名称>` 前缀:每个前缀在其正文(最终 user
/// 消息)之前产出一条 developer 消息。任一前缀解析失败(未知技能/读文件
/// 失败)即停止:不注入,剩余文本(含未展开的前缀)原样保留。
fn expand_user_skill_prefix(
    text: String,
    skills: &[SkillDef],
    timestamp: i64,
    out: &mut Vec<Message>,
) {
    let mut rest: &str = &text;
    while let Some((name, remainder)) = parse_leading_skill_token(rest) {
        let Some(def) = skills.iter().find(|skill| skill.name == name) else {
            break;
        };
        let Some(message) = skill_developer_message(name, def) else {
            break;
        };
        out.push(message);
        rest = remainder;
    }
    out.push(Message::User {
        content: UserContent::Text(rest.to_string()),
        timestamp,
    });
}

/// convert_to_llm 出口后处理:load_skill 工具结果与 `/skill` 消息前缀的
/// developer 注入(模块文档)。其余消息原样透传。
pub fn expand_skill_references(skills: &[SkillDef], msgs: Vec<Message>) -> Vec<Message> {
    let mut out = Vec::with_capacity(msgs.len());
    for message in msgs {
        match message {
            Message::ToolResult {
                tool_call_id,
                tool_name,
                content,
                details,
                is_error,
                timestamp,
            } if tool_name == TOOL_NAME && !is_error => {
                expand_load_skill_tool_result(
                    tool_call_id,
                    tool_name,
                    content,
                    details,
                    is_error,
                    timestamp,
                    &mut out,
                );
            }
            Message::User {
                content: UserContent::Text(text),
                timestamp,
            } => {
                expand_user_skill_prefix(text, skills, timestamp, &mut out);
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, body: &str) -> SkillDef {
        // 唯一目录:并行测试间互不干扰(同名 skill 各自隔离)
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "latent-skill-expand-{}-{seq}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(&path, body).unwrap();
        SkillDef {
            name: name.into(),
            description: "d".into(),
            path,
        }
    }

    fn cleanup(def: &SkillDef) {
        let _ = std::fs::remove_dir_all(def.path.parent().unwrap());
    }

    fn user(text: &str) -> Message {
        Message::User {
            content: UserContent::Text(text.into()),
            timestamp: 1,
        }
    }

    fn roles(msgs: &[Message]) -> Vec<&'static str> {
        msgs.iter()
            .map(|m| match m {
                Message::Developer { .. } => "developer",
                Message::User { .. } => "user",
                Message::ToolResult { .. } => "tool_result",
                _ => "other",
            })
            .collect()
    }

    fn developer_text(message: &Message) -> String {
        match message {
            Message::Developer { content, .. } => content.clone(),
            _ => panic!("不是 developer 消息: {message:?}"),
        }
    }

    fn user_text(message: &Message) -> String {
        match message {
            Message::User {
                content: UserContent::Text(text),
                ..
            } => text.clone(),
            _ => panic!("不是文本 user 消息: {message:?}"),
        }
    }

    fn tool_result(text: &str, is_error: bool) -> Message {
        Message::ToolResult {
            tool_call_id: "t1".into(),
            tool_name: TOOL_NAME.into(),
            content: vec![ContentBlock::text(text)],
            details: None,
            is_error,
            timestamp: 1,
        }
    }

    #[test]
    fn load_skill_tool_result_becomes_confirmation_plus_developer() {
        let def = skill("review", "# Review\nSteps here.");
        let out = expand_skill_references(
            std::slice::from_ref(&def),
            vec![tool_result("<skill name=\"review\">\n# Review\nSteps here.\n</skill>", false)],
        );
        assert_eq!(roles(&out), vec!["tool_result", "developer"]);
        match &out[0] {
            Message::ToolResult { content, .. } => {
                assert_eq!(content[0].as_text().unwrap(), LOAD_CONFIRMATION);
            }
            _ => panic!(),
        }
        assert!(developer_text(&out[1]).contains("# Review\nSteps here."));
        cleanup(&def);
    }

    #[test]
    fn error_tool_result_and_other_tools_pass_through() {
        let error = Message::ToolResult {
            tool_call_id: "t1".into(),
            tool_name: TOOL_NAME.into(),
            content: vec![ContentBlock::text("boom")],
            details: None,
            is_error: true,
            timestamp: 1,
        };
        let other = Message::ToolResult {
            tool_call_id: "t2".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::text("file body")],
            details: None,
            is_error: false,
            timestamp: 1,
        };
        let out = expand_skill_references(&[], vec![error, other, user("hi")]);
        assert_eq!(roles(&out), vec!["tool_result", "tool_result", "user"]);
        assert!(out.len() == 3, "错误结果与其他工具不注入 developer 消息");
    }

    #[test]
    fn user_prefix_injects_developer_before_stripped_user() {
        let def = skill("review", "# Review\nFollow these steps.");
        let out = expand_skill_references(
            std::slice::from_ref(&def),
            vec![user("/skill review 请帮我review这个代码")],
        );
        assert_eq!(roles(&out), vec!["developer", "user"]);
        let developer = developer_text(&out[0]);
        assert!(developer.starts_with("<skill name=\"review\">"));
        assert!(developer.contains("# Review\nFollow these steps."));
        assert!(developer.ends_with("</skill>"));
        assert_eq!(user_text(&out[1]), "请帮我review这个代码");
        cleanup(&def);
    }

    #[test]
    fn multiple_prefixes_inject_in_order() {
        let a = skill("alpha", "A body");
        let b = skill("beta", "B body");
        let out = expand_skill_references(
            &[a.clone(), b.clone()],
            vec![user("/skill alpha /skill beta 正文")],
        );
        assert_eq!(roles(&out), vec!["developer", "developer", "user"]);
        assert!(developer_text(&out[0]).contains("A body"));
        assert!(developer_text(&out[1]).contains("B body"));
        assert_eq!(user_text(&out[2]), "正文");
        cleanup(&a);
        cleanup(&b);
    }

    #[test]
    fn unknown_skill_leaves_text_unchanged() {
        let out = expand_skill_references(&[], vec![user("/skill nope 你好")]);
        assert_eq!(roles(&out), vec!["user"]);
        assert_eq!(user_text(&out[0]), "/skill nope 你好");
    }

    #[test]
    fn lookalike_words_and_bare_token_do_not_expand() {
        let out = expand_skill_references(&[], vec![user("/skills list")]);
        assert_eq!(roles(&out), vec!["user"]);
        assert_eq!(user_text(&out[0]), "/skills list");
        let bare = expand_skill_references(&[], vec![user("/skill")]);
        assert_eq!(roles(&bare), vec!["user"]);
        assert_eq!(user_text(&bare[0]), "/skill");
    }

    #[test]
    fn empty_content_after_prefix_keeps_text_without_injection() {
        let def = skill("review", "body");
        let out = expand_skill_references(std::slice::from_ref(&def), vec![user("/skill review")]);
        // 前缀合法但正文为空:仍注入 developer(用户明确点名了技能),正文为空串
        assert_eq!(roles(&out), vec!["developer", "user"]);
        assert_eq!(user_text(&out[1]), "");
        cleanup(&def);
    }

    #[test]
    fn non_text_user_content_passes_through() {
        let blocks = Message::User {
            content: UserContent::Blocks(vec![ContentBlock::text("/skill review")]),
            timestamp: 1,
        };
        let out = expand_skill_references(&[], vec![blocks]);
        assert_eq!(roles(&out), vec!["user"]);
    }

    #[test]
    fn prefix_parser_boundaries() {
        assert_eq!(parse_leading_skill_token("/skill review x"), Some(("review", "x")));
        assert_eq!(parse_leading_skill_token("/skill  spaced  y"), Some(("spaced", "y")));
        assert_eq!(parse_leading_skill_token("/skill a\n\n正文"), Some(("a", "正文")));
        assert_eq!(parse_leading_skill_token("/skills list"), None);
        assert_eq!(parse_leading_skill_token("/skill"), None);
        assert_eq!(parse_leading_skill_token("/skill "), None);
        assert_eq!(parse_leading_skill_token("hello /skill a"), None);
        assert_eq!(parse_leading_skill_token("/skill\ttab x"), Some(("tab", "x")));
        assert_eq!(parse_leading_skill_token(""), None);
    }
}
