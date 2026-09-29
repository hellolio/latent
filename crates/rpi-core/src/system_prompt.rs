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
    pub cwd: Option<String>,
}

/// 有序命名节(04 文档 §3.2):除 preamble 外每节包 <name>...</name> 标签。
pub type SystemPromptSections = BTreeMap<String, String>;

const BASE_RULES: &[&str] = &[
    "Be concise, precise, and rigorous.",
    "Make minimal changes and preserve existing behavior.",
    "Ask before making non-trivial design or implementation decisions.",
    "Inspect relevant files before modifying them.",
    "Verify changes when practical.",
    "Avoid interactive commands.",
];

/// 构建 sections 状态(preamble 无标签,tools/rules/addendum/project_context/cwd
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

    // rules 节:固定基础规则(工具提示词只进 tools 节,不再并入 rules)
    let rules: Vec<String> = BASE_RULES.iter().map(|s| s.to_string()).collect();
    if !rules.is_empty() {
        let text = rules
            .iter()
            .map(|rule| format!("- {rule}"))
            .collect::<Vec<_>>()
            .join("\n");
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

    // cwd
    if let Some(cwd) = &options.cwd {
        sections.insert("cwd".into(), format!("Working directory: {cwd}"));
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
        assert!(text.contains("<cwd>\nWorking directory: /tmp/proj"));
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
    /// cwd/tools 等动态节保留自动注入。
    #[test]
    fn custom_prompt_replaces_preamble_keeps_dynamic_sections() {
        let mut opts = options();
        opts.custom_prompt = Some("You are my custom agent.".into());
        let text = build_system_prompt_state(&opts).unwrap().to_text();
        assert!(text.starts_with("You are my custom agent."), "{text}");
        assert!(!text.contains("You are Hart, an interactive agent"));
        assert!(text.contains("<tools>\nAvailable tools:"), "{text}");
        assert!(text.contains("<cwd>\nWorking directory: /tmp/proj"), "{text}");
        assert!(text.contains("<rules>"), "{text}");
    }
}
