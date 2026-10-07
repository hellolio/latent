//! 斜杠命令解析层(pi `BUILTIN_SLASH_COMMANDS` 的核心子集):命令表与
//! 解析在此,执行(需要 session 状态)由各入口自担 —— TUI 在
//! interactive.rs,聊天端在 latent-gateway(auto_reply/commands.rs)。
//! latent 没有动态命令源(扩展命令/prompt 模板/skill),未识别的 `/xxx`
//! 不像 pi 那样发给模型,而是本地警告。

/// 命令表条目(/help 展示与解析共用)。
pub struct SlashCommand {
    pub name: &'static str,
    pub args: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        args: "",
        description: "显示帮助(命令与快捷键)",
    },
    SlashCommand {
        name: "model",
        args: "[provider/model]",
        description: "查看/切换模型(空参数选择器末尾可添加模型或编辑 models.json)",
    },
    SlashCommand {
        name: "thinking",
        args: "[level]",
        description: "查看/设置 thinking 级别(off|minimal|low|medium|high|xhigh|max)",
    },
    SlashCommand {
        name: "theme",
        args: "[name]",
        description: "查看/切换主题(ratatui-themes,如 tokyo-night/nord)",
    },
    SlashCommand {
        name: "compact",
        args: "",
        description: "压缩上下文(摘要替换历史)",
    },
    SlashCommand {
        name: "new",
        args: "",
        description: "新建会话(当前会话已保存,原样保留在原文件)",
    },
    SlashCommand {
        name: "mode",
        args: "<plan|confirm|full-access>",
        description: "切换会话模式(Shift+Tab 循环)",
    },
    SlashCommand {
        name: "subagent",
        args: "[off]",
        description: "平行子 agent 会话;无参打开选择器,off 回主会话",
    },
    SlashCommand {
        name: "session",
        args: "<list|info>",
        description: "list = 切换历史会话;info = 显示会话信息",
    },
    SlashCommand {
        name: "setting",
        args: "",
        description: "打开设置(全屏模式 / 复制快捷键 / 鼠标选中复制;写入 settings.json)",
    },
    SlashCommand {
        name: "quit",
        args: "",
        description: "退出 latent",
    },
];

/// 解析出的命令动作;执行需要 session 状态,由 interactive.rs 分派。
#[derive(Debug, Clone, PartialEq)]
pub enum SlashAction {
    Help,
    Model { arg: Option<String> },
    Thinking { arg: Option<String> },
    Theme { arg: Option<String> },
    Compact { arg: Option<String> },
    New,
    Mode { arg: Option<String> },
    Subagent { arg: Option<String> },
    Session { arg: Option<String> },
    Setting,
    Quit,
}

/// `/mode` 的参数变体(声明序即弹窗展示序;通用 variants 机制,见 command_popup)。
pub const MODE_VARIANTS: &[(&str, &str)] = &[
    ("plan", "只读:先列计划,不修改任何文件"),
    ("confirm", "全工具:文件写入与命令执行前需确认"),
    ("full-access", "全自动:无审批、无沙箱"),
];

/// `/session` 的参数变体(声明序即弹窗展示序)。
pub const SESSION_VARIANTS: &[(&str, &str)] = &[
    ("list", "切换历史会话(列表选择)"),
    ("info", "显示当前会话信息与统计"),
];

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
        "session" => SlashInput::Command(SlashAction::Session {
            arg: arg.map(str::to_string),
        }),
        "model" => SlashInput::Command(SlashAction::Model {
            arg: arg.map(str::to_string),
        }),
        "thinking" => SlashInput::Command(SlashAction::Thinking {
            arg: arg.map(str::to_string),
        }),
        "theme" => SlashInput::Command(SlashAction::Theme {
            arg: arg.map(str::to_string),
        }),
        "compact" => SlashInput::Command(SlashAction::Compact {
            arg: arg.map(str::to_string),
        }),
        "new" => SlashInput::Command(SlashAction::New),
        "setting" => SlashInput::Command(SlashAction::Setting),
        "subagent" => SlashInput::Command(SlashAction::Subagent {
            arg: arg.map(str::to_string),
        }),
        "mode" => SlashInput::Command(SlashAction::Mode {
            arg: arg.map(str::to_string),
        }),
        _ => SlashInput::Unknown(format!("/{name}")),
    }
}

/// /help 内容(markdown 源文本;调用方经 `assistant_markdown` 渲染上屏,
/// 与模型回复同一渲染管线:标题/列表/行内代码着色)。
pub fn help_markdown() -> String {
    let mut out = String::from("## 命令\n\n");
    for command in COMMANDS {
        let signature = format!("{} {}", command.name, command.args)
            .trim()
            .to_string();
        out.push_str(&format!("- `/{signature}` — {}\n", command.description));
    }
    out.push_str("\n## 快捷键\n\n");
    out.push_str("- 输入 `/` 弹出命令补全(↑/↓ 选择 · Tab/Enter 补全 · Esc 关闭)\n");
    out.push_str("- `Shift+Tab` 循环切换会话模式(plan → confirm → full-access)\n");
    out.push_str("- `Enter` 发送 · `Esc` 中止当前 run · `Ctrl+C` 中断/双击退出 · `Ctrl+D` 退出\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands_with_and_without_args() {
        assert_eq!(parse("/help"), SlashInput::Command(SlashAction::Help));
        assert_eq!(parse("/quit"), SlashInput::Command(SlashAction::Quit));
        assert_eq!(parse("/setting"), SlashInput::Command(SlashAction::Setting));
        assert_eq!(
            parse("  /session  "),
            SlashInput::Command(SlashAction::Session { arg: None })
        );
        assert_eq!(
            parse("/session info"),
            SlashInput::Command(SlashAction::Session {
                arg: Some("info".into())
            })
        );
        assert_eq!(
            parse("/compact"),
            SlashInput::Command(SlashAction::Compact { arg: None })
        );
        assert_eq!(parse("/new"), SlashInput::Command(SlashAction::New));
        assert_eq!(
            parse("/compact 保留近期消息"),
            SlashInput::Command(SlashAction::Compact {
                arg: Some("保留近期消息".into())
            })
        );
        assert_eq!(
            parse("/model anthropic/claude-opus-4-6"),
            SlashInput::Command(SlashAction::Model {
                arg: Some("anthropic/claude-opus-4-6".into())
            })
        );
        assert_eq!(
            parse("/thinking high"),
            SlashInput::Command(SlashAction::Thinking {
                arg: Some("high".into())
            })
        );
        assert_eq!(
            parse("/theme nord"),
            SlashInput::Command(SlashAction::Theme {
                arg: Some("nord".into())
            })
        );
        assert_eq!(
            parse("/theme"),
            SlashInput::Command(SlashAction::Theme { arg: None })
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
    fn help_markdown_lists_every_command() {
        let text = help_markdown();
        for command in COMMANDS {
            assert!(
                text.contains(&format!("/{}", command.name)),
                "缺少 /{}",
                command.name
            );
        }
    }
}
