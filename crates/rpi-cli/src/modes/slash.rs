//! interactive 模式的斜杠命令(pi `BUILTIN_SLASH_COMMANDS` 的核心子集):
//! 解析与命令表在此,执行(需要 session/TUI 状态)在 interactive.rs。
//! rpi 没有动态命令源(扩展命令/prompt 模板/skill),未识别的 `/xxx`
//! 不像 pi 那样发给模型,而是本地警告。

/// 命令表条目(/help 展示与解析共用)。
pub struct SlashCommand {
    pub name: &'static str,
    pub args: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand { name: "help", args: "", description: "显示帮助(命令与快捷键)" },
    SlashCommand { name: "model", args: "[provider/model]", description: "查看/切换模型" },
    SlashCommand {
        name: "thinking",
        args: "[level]",
        description: "查看/设置 thinking 级别(off|minimal|low|medium|high|xhigh|max)",
    },
    SlashCommand { name: "compact", args: "", description: "压缩上下文(摘要替换历史)" },
    SlashCommand { name: "session", args: "", description: "显示会话信息与统计" },
    SlashCommand { name: "quit", args: "", description: "退出 rpi" },
];

/// 解析出的命令动作;执行需要 session 状态,由 interactive.rs 分派。
#[derive(Debug, Clone, PartialEq)]
pub enum SlashAction {
    Help,
    Model { arg: Option<String> },
    Thinking { arg: Option<String> },
    Compact { arg: Option<String> },
    Session,
    Quit,
}

/// 一行输入的解析结果。
#[derive(Debug, Clone, PartialEq)]
pub enum SlashInput {
    /// 已识别的斜杠命令
    Command(SlashAction),
    /// `/xxx` 但不在命令表中(本地警告,不发给模型)
    Unknown(String),
    /// 普通消息(发给模型)
    NotACommand(String),
}

/// 解析一行输入:斜杠命令 → Command/Unknown,其余原样透传。
pub fn parse(input: &str) -> SlashInput {
    let trimmed = input.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return SlashInput::NotACommand(trimmed.to_string());
    };
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, Some(arg.trim())),
        None => (rest, None),
    };
    // `/name`(空参数)与 `/name `(尾部空白)都算命令调用
    let arg = arg.filter(|arg| !arg.is_empty());
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "help" => SlashInput::Command(SlashAction::Help),
        "quit" => SlashInput::Command(SlashAction::Quit),
        "session" => SlashInput::Command(SlashAction::Session),
        "model" => SlashInput::Command(SlashAction::Model { arg: arg.map(str::to_string) }),
        "thinking" => SlashInput::Command(SlashAction::Thinking { arg: arg.map(str::to_string) }),
        "compact" => SlashInput::Command(SlashAction::Compact { arg: arg.map(str::to_string) }),
        _ => SlashInput::Unknown(format!("/{name}")),
    }
}

/// /help 渲染行:命令表 + 快捷键(pi 无 /help,用 header 键位提示;这里
/// 以命令形式提供等价信息)。
pub fn help_lines() -> Vec<String> {
    let mut lines = vec!["命令:".to_string()];
    for command in COMMANDS {
        let signature = format!("{} {}", command.name, command.args).trim().to_string();
        lines.push(format!("  /{:<28} {}", signature, command.description));
    }
    lines.push(String::new());
    lines.push("快捷键:".to_string());
    lines.push("  Enter 发送 · Esc 中止当前 run · Ctrl+C 中断/双击退出 · Ctrl+D 退出".to_string());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands_with_and_without_args() {
        assert_eq!(parse("/help"), SlashInput::Command(SlashAction::Help));
        assert_eq!(parse("/quit"), SlashInput::Command(SlashAction::Quit));
        assert_eq!(parse("  /session  "), SlashInput::Command(SlashAction::Session));
        assert_eq!(parse("/compact"), SlashInput::Command(SlashAction::Compact { arg: None }));
        assert_eq!(
            parse("/compact 保留近期消息"),
            SlashInput::Command(SlashAction::Compact { arg: Some("保留近期消息".into()) })
        );
        assert_eq!(
            parse("/model anthropic/claude-opus-4-6"),
            SlashInput::Command(SlashAction::Model { arg: Some("anthropic/claude-opus-4-6".into()) })
        );
        assert_eq!(
            parse("/thinking high"),
            SlashInput::Command(SlashAction::Thinking { arg: Some("high".into()) })
        );
    }

    #[test]
    fn unknown_slash_is_local_warning_not_prompt() {
        assert_eq!(parse("/foo"), SlashInput::Unknown("/foo".into()));
        assert_eq!(parse("/foo bar"), SlashInput::Unknown("/foo".into()));
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(parse("你好"), SlashInput::NotACommand("你好".into()));
        // 普通文本里的斜杠不是命令
        assert_eq!(parse("a/b"), SlashInput::NotACommand("a/b".into()));
        assert_eq!(parse(""), SlashInput::NotACommand("".into()));
    }

    #[test]
    fn help_lines_list_every_command() {
        let lines = help_lines();
        let text = lines.join("\n");
        for command in COMMANDS {
            assert!(text.contains(&format!("/{}", command.name)), "缺少 /{}", command.name);
        }
    }
}
