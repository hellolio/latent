//! 系统提示词 sections 机制(04 文档 §3):提示词是**可增量更新的命名 sections
//! 状态**,只在内存与请求级字段中维护 —— session 不持久化能力规则,恢复时
//! 从当前配置重新组装。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::CoreError;

/// section 名约束(01 文档):`[a-z][a-z0-9_-]*` 且不得叫 preamble。
fn validate_section_name(name: &str) -> Result<(), CoreError> {
    let valid = name != "preamble"
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(CoreError::SystemPrompt(format!(
            "invalid section name `{name}`: must match [a-z][a-z0-9_-]* and not be `preamble`"
        )))
    }
}

/// 构建输入(04 文档 BuildSystemPromptOptions 的 M4 子集)。
#[derive(Debug, Clone, Default)]
pub struct SystemPromptOptions {
    /// 整体替换默认前缀(pi 的 customPrompt)
    pub custom_prompt: Option<String>,
    /// before_agent_start 处理器的整 prompt 覆盖
    pub force_system_prompt: Option<String>,
    /// 每个工具贡献的一行片段(进 tools 节)
    pub tool_snippets: Vec<String>,
    /// 追加到提示词末尾(addendum 节)
    pub append_system_prompt: Option<String>,
    /// 扩展自定义 XML 节
    pub sections: BTreeMap<String, String>,
    /// AGENTS.md 等上下文文件,渲染成 <project_instructions path="...">
    pub context_files: Vec<(String, String)>,
    /// <rules> 节的追加规则(system-prompt.md 的 <rules> 标记块);拼在
    /// 内置规则之后
    pub custom_rules: Option<String>,
    /// <env> 节的当前时间行;None = 取当前本地时间(测试可注入固定值)
    pub current_time: Option<String>,
    pub cwd: Option<String>,
}

/// 有序命名节(04 文档 §3.2):除 preamble 外每节包 <name>...</name> 标签。
pub type SystemPromptSections = BTreeMap<String, String>;

const BASE_RULES: &[&str] = &[
    "Be extremely terse: deliver the answer or the change, with no filler, preamble, or restatement.",
    "Be meticulous: watch edge cases, exact identifiers, and existing conventions.",
    "Make minimal changes and preserve existing behavior.",
    "Never decide on the user's behalf: at any ambiguity or fork, present the options with a one-line recommendation and wait for confirmation.",
    "Inspect relevant files before modifying them.",
    "Verify changes when practical.",
    "Avoid interactive commands.",
];

/// 当前本地时间(装配期调用;`%:z` 渲染为 `+08:00` 形式的时区偏移)。
fn current_local_time() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z").to_string()
}

/// 拆分外置 system-prompt.md 为 (身份句, <rules> 追加规则):文件里可用
/// `<rules>...</rules>` 标记块声明自定义规则(可多处,按出现顺序拼接),
/// 块外内容是身份句替换(custom_prompt);没有标记块 = 全文是身份句
/// (向后兼容)。未闭合的 `<rules>` 把余下全文当作规则内容。
pub fn split_prompt_and_rules(text: &str) -> (Option<String>, Option<String>) {
    const OPEN: &str = "<rules>";
    const CLOSE: &str = "</rules>";
    let mut preamble = String::new();
    let mut rules: Vec<&str> = Vec::new();
    let mut rest = text;
    loop {
        match rest.find(OPEN) {
            Some(start) => {
                preamble.push_str(&rest[..start]);
                let after_open = &rest[start + OPEN.len()..];
                match after_open.find(CLOSE) {
                    Some(end) => {
                        rules.push(&after_open[..end]);
                        rest = &after_open[end + CLOSE.len()..];
                    }
                    None => {
                        rules.push(after_open);
                        rest = "";
                    }
                }
            }
            None => {
                preamble.push_str(rest);
                break;
            }
        }
    }
    let preamble = preamble.trim();
    let rules_text = rules.join("\n").trim().to_string();
    (
        (!preamble.is_empty()).then(|| preamble.to_string()),
        (!rules_text.is_empty()).then_some(rules_text),
    )
}

/// 构建 sections 状态(preamble 无标签,tools/rules/addendum/project_context/env
/// + 扩展自定义节)。
pub fn build_system_prompt_sections(
    options: &SystemPromptOptions,
) -> Result<SystemPromptSections, CoreError> {
    for name in options.sections.keys() {
        validate_section_name(name)?;
    }

    let mut sections: SystemPromptSections = BTreeMap::new();

    // preamble:身份句,无标签
    sections.insert(
        "preamble".into(),
        options.custom_prompt.clone().unwrap_or_else(|| {
            "You are Hart, an interactive agent that helps users with software engineering \
             tasks."
                .into()
        }),
    );

    // tools 节:每个激活工具一行片段
    if !options.tool_snippets.is_empty() {
        let mut tools = String::from("Available tools:\n");
        for snippet in &options.tool_snippets {
            tools.push_str(&format!("- {snippet}\n"));
        }
        sections.insert("tools".into(), tools.trim_end().to_string());
    }

    // rules 节:固定基础规则 + 用户自定义追加(system-prompt.md 的
    // <rules> 标记块,整段原样保留用户排版,不逐行加 "-" 前缀)
    // (工具提示词只进 tools 节)
    let rules: Vec<String> = BASE_RULES.iter().map(|s| s.to_string()).collect();
    let custom_rules = options
        .custom_rules
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if !rules.is_empty() || custom_rules.is_some() {
        let mut text = rules
            .iter()
            .map(|rule| format!("- {rule}"))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(custom) = custom_rules {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(custom);
        }
        sections.insert("rules".into(), text);
    }

    // project_context:AGENTS.md 等上下文文件
    if !options.context_files.is_empty() {
        let mut context = String::new();
        for (path, content) in &options.context_files {
            context.push_str(&format!(
                "<project_instructions path=\"{path}\">\n{content}\n</project_instructions>\n\n"
            ));
        }
        sections.insert("project_context".into(), context.trim_end().to_string());
    }

    // env:运行环境事实 —— 工作目录 + 当前本地时间(模型判断"在哪/今天"的依据)
    if let Some(cwd) = &options.cwd {
        let time = options
            .current_time
            .clone()
            .unwrap_or_else(current_local_time);
        sections.insert(
            "env".into(),
            format!("Working directory: {cwd}\nCurrent time: {time}"),
        );
    }

    // 扩展自定义节
    for (name, text) in &options.sections {
        sections.insert(name.clone(), text.clone());
    }

    // addendum:追加指令
    if let Some(append) = &options.append_system_prompt {
        sections.insert("addendum".into(), append.clone());
    }

    Ok(sections)
}

/// 系统提示词状态(04 文档 buildSystemPromptState):强制整 prompt → 存 content;
/// 结构化 → 存 sections。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SystemPromptState {
    Forced(String),
    Sections(SystemPromptSections),
}

impl SystemPromptState {
    pub fn to_text(&self) -> String {
        match self {
            SystemPromptState::Forced(prompt) => prompt.clone(),
            SystemPromptState::Sections(sections) => sections_to_text(sections),
        }
    }
}

/// sections → 提示词文本:preamble 原样,其余每节包 <name> 标签(04 文档 :175-178)。
pub fn sections_to_text(sections: &SystemPromptSections) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(preamble) = sections.get("preamble") {
        if !preamble.is_empty() {
            parts.push(preamble.clone());
        }
    }
    for (name, text) in sections {
        if name == "preamble" || text.is_empty() {
            continue;
        }
        parts.push(format!("<{name}>\n{text}\n</{name}>"));
    }
    parts.join("\n\n")
}

/// 构建 state:force_system_prompt 优先。
pub fn build_system_prompt_state(
    options: &SystemPromptOptions,
) -> Result<SystemPromptState, CoreError> {
    if let Some(forced) = &options.force_system_prompt {
        return Ok(SystemPromptState::Forced(forced.clone()));
    }
    Ok(SystemPromptState::Sections(build_system_prompt_sections(
        options,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> SystemPromptOptions {
        SystemPromptOptions {
            tool_snippets: vec!["read(path): read a file".into()],
            cwd: Some("/tmp/proj".into()),
            ..Default::default()
        }
    }

    #[test]
    fn sections_render_with_tags_and_preamble_untagged() {
        let sections = build_system_prompt_sections(&options()).unwrap();
        let text = sections_to_text(&sections);
        assert!(text.starts_with("You are Hart"));
        assert!(!text.contains("<preamble>"));
        assert!(text.contains("<tools>\nAvailable tools:"));
        assert!(text.contains("<rules>"));
        assert!(text.contains("<env>\nWorking directory: /tmp/proj"));
    }

    #[test]
    fn invalid_section_name_rejected() {
        let mut opts = options();
        opts.sections.insert("Bad Name".into(), "x".into());
        assert!(build_system_prompt_sections(&opts).is_err());
        let mut opts = options();
        opts.sections.insert("preamble".into(), "x".into());
        assert!(build_system_prompt_sections(&opts).is_err());
        let mut opts = options();
        opts.sections.insert("ok_name-2".into(), "x".into());
        assert!(build_system_prompt_sections(&opts).is_ok());
    }

    #[test]
    fn forced_prompt_bypasses_sections() {
        let mut opts = options();
        opts.force_system_prompt = Some("custom only".into());
        let state = build_system_prompt_state(&opts).unwrap();
        assert_eq!(state.to_text(), "custom only");
        assert!(matches!(state, SystemPromptState::Forced(_)));
    }

    /// 外置提示词语义(cli system-prompt.md):custom_prompt 只换身份句,
    /// env/tools 等动态节保留自动注入。
    #[test]
    fn custom_prompt_replaces_preamble_keeps_dynamic_sections() {
        let mut opts = options();
        opts.custom_prompt = Some("You are my custom agent.".into());
        let text = build_system_prompt_state(&opts).unwrap().to_text();
        assert!(text.starts_with("You are my custom agent."), "{text}");
        assert!(!text.contains("You are Hart, an interactive agent"));
        assert!(text.contains("<tools>\nAvailable tools:"), "{text}");
        assert!(text.contains("<env>\nWorking directory: /tmp/proj"), "{text}");
        assert!(text.contains("<rules>"), "{text}");
    }

    /// AGENTS.md 等上下文文件渲染进 <project_context> 节(带路径的
    /// <project_instructions> 块);为空时无此节。
    #[test]
    fn context_files_render_into_project_context_section() {
        let mut opts = options();
        opts.context_files = vec![("/tmp/proj/AGENTS.md".into(), "project rules".into())];
        let sections = build_system_prompt_sections(&opts).unwrap();
        let context = sections.get("project_context").unwrap();
        assert_eq!(
            context,
            "<project_instructions path=\"/tmp/proj/AGENTS.md\">\nproject rules\n</project_instructions>"
        );
        let text = sections_to_text(&sections);
        assert!(text.contains("<project_context>\n<project_instructions path=\"/tmp/proj/AGENTS.md\">"), "{text}");
        // 无上下文文件 = 无该节
        let sections = build_system_prompt_sections(&options()).unwrap();
        assert!(!sections.contains_key("project_context"));
    }

    /// <env> 节包含工作目录与当前时间;注入固定值时原样渲染。
    #[test]
    fn env_section_contains_working_directory_and_time() {
        let mut opts = options();
        opts.current_time = Some("2026-10-03 14:23:45 +08:00".into());
        let text = build_system_prompt_sections(&opts).unwrap();
        let env = text.get("env").unwrap();
        assert_eq!(
            env,
            "Working directory: /tmp/proj\nCurrent time: 2026-10-03 14:23:45 +08:00"
        );
        // 未注入时取当前本地时间(格式健全性:两行,时间行非空)
        let opts = options();
        let sections = build_system_prompt_sections(&opts).unwrap();
        let env = sections.get("env").unwrap();
        assert!(env.contains("\nCurrent time: "), "{env}");
    }

    /// <rules> 自定义追加(system-prompt.md 的 <rules> 标记块):拼在内置
    /// 规则之后,原样保留排版。
    #[test]
    fn custom_rules_appended_after_base_rules() {
        let mut opts = options();
        opts.custom_rules = Some("Always answer in Chinese.\n- Prefer ripgrep over grep".into());
        let sections = build_system_prompt_sections(&opts).unwrap();
        let rules = sections.get("rules").unwrap();
        assert!(rules.contains("- Be extremely terse: deliver the answer or the change"), "{rules}");
        assert!(rules.contains("\nAlways answer in Chinese.\n- Prefer ripgrep over grep"), "{rules}");
        // 空白自定义规则不追加
        let mut opts = options();
        opts.custom_rules = Some("  \n ".into());
        let sections = build_system_prompt_sections(&opts).unwrap();
        let rules = sections.get("rules").unwrap();
        assert!(!rules.contains("Chinese"), "{rules}");
    }

    /// 外置 system-prompt.md 的 <rules> 标记块拆分:块外 = 身份句,块内 =
    /// 追加规则;无标记块 = 全文身份句(向后兼容);未闭合标记余下全文算规则。
    #[test]
    fn split_prompt_and_rules_extracts_marked_block() {
        // 无标记块:全文是身份句
        let (prompt, rules) = split_prompt_and_rules("You are my agent.");
        assert_eq!(prompt.as_deref(), Some("You are my agent."));
        assert_eq!(rules, None);
        // 标记块:块外身份句 + 块内规则
        let (prompt, rules) = split_prompt_and_rules(
            "You are my agent.\n\n<rules>\nAlways answer in Chinese.\n</rules>\n",
        );
        assert_eq!(prompt.as_deref(), Some("You are my agent."));
        assert_eq!(rules.as_deref(), Some("Always answer in Chinese."));
        // 多个标记块按序拼接;空白块不产生规则
        let (prompt, rules) = split_prompt_and_rules(
            "<rules>A</rules> mid <rules>B</rules>",
        );
        assert_eq!(prompt.as_deref(), Some("mid"));
        assert_eq!(rules.as_deref(), Some("A\nB"));
        // 只有标记块:身份句为空
        let (prompt, rules) = split_prompt_and_rules("<rules>R</rules>");
        assert_eq!(prompt, None);
        assert_eq!(rules.as_deref(), Some("R"));
        // 未闭合:余下全文是规则
        let (prompt, rules) = split_prompt_and_rules("Hi <rules>\nR1\nR2");
        assert_eq!(prompt.as_deref(), Some("Hi"));
        assert_eq!(rules.as_deref(), Some("R1\nR2"));
    }
}
