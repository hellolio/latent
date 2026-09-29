//! agent 类型定义(14 文档 §4.4):定义是数据文件(`<cwd>/.rpi/agents/*.md`
//! 项目优先 → `~/.rpi/agents/*.md`),加类型不重编译。
//!
//! frontmatter 只支持本子集(name/description/model/tools),正文即 system
//! prompt;解析失败单条诊断跳过,不阻断其他文件(对齐扩展错误语义 07 §8.5)。

use std::collections::BTreeMap;
use std::path::Path;

/// 命名 agent 定义(.md 文件的解析结果)。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    /// frontmatter `name`,缺省文件名(不含扩展名)
    pub name: String,
    pub description: String,
    /// `provider/model` spec;None = 继承父会话模型
    pub model: Option<String>,
    /// 工具白名单;None = 用引擎默认只读集
    pub tools: Option<Vec<String>>,
    /// frontmatter 正文(首个 `---` 围栏之后),trim 后即 system prompt
    pub system_prompt: String,
}

/// 发现两级目录的 agent 定义;同名时项目级覆盖用户级。
/// 返回 (按 name 排序的定义, 诊断消息)。
pub fn discover_agent_defs(cwd: &Path, home: Option<&Path>) -> (Vec<AgentDef>, Vec<String>) {
    let mut diagnostics = Vec::new();
    let mut by_name: BTreeMap<String, AgentDef> = BTreeMap::new();
    // 后扫描的覆盖先扫描的:先用户级,后项目级(项目优先)
    let dirs = [
        home.map(|home| home.join(".rpi").join("agents")),
        Some(cwd.join(".rpi").join("agents")),
    ];
    for dir in dirs.into_iter().flatten() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                diagnostics.push(format!("无法读取 agent 目录 {}: {error}", dir.display()));
                continue;
            }
        };
        let mut files: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort();
        for path in files {
            match parse_agent_def_file(&path) {
                Ok(def) => {
                    by_name.insert(def.name.clone(), def);
                }
                Err(message) => diagnostics.push(format!(
                    "agent 定义 `{}` 解析失败,已跳过: {message}",
                    path.display()
                )),
            }
        }
    }
    (by_name.into_values().collect(), diagnostics)
}

fn parse_agent_def_file(path: &Path) -> Result<AgentDef, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| "文件名不是合法 UTF-8".to_string())?;
    parse_agent_def(&content, file_name)
}

/// 解析单个定义:frontmatter 围栏 + 正文;frontmatter 缺省 = 全默认字段。
pub fn parse_agent_def(content: &str, default_name: &str) -> Result<AgentDef, String> {
    let (frontmatter, body) = split_frontmatter(content)?;
    let mut name = default_name.to_string();
    let mut description = String::new();
    let mut model: Option<String> = None;
    let mut tools: Option<Vec<String>> = None;

    let mut lines = frontmatter.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            continue;
        }
        let Some((key, inline_value)) = trimmed.split_once(':') else {
            return Err(format!("frontmatter 行缺少 `key: value` 形态: `{trimmed}`"));
        };
        let key = key.trim();
        let inline_value = inline_value.trim();
        match key {
            "name" => name = require_scalar(key, inline_value)?,
            "description" => description = require_scalar(key, inline_value)?,
            "model" => model = Some(require_scalar(key, inline_value)?),
            "tools" => {
                tools = Some(if inline_value.is_empty() {
                    // dash 列表(下一行起)或同行 CSV
                    let mut items = Vec::new();
                    while let Some(next) = lines.peek() {
                        let next_trimmed = next.trim();
                        if let Some(item) = next_trimmed.strip_prefix("- ") {
                            items.push(item.trim().to_string());
                            lines.next();
                        } else if next_trimmed.is_empty() {
                            lines.next();
                        } else {
                            break;
                        }
                    }
                    if items.is_empty() {
                        return Err("tools 列表为空(dash 列表或 CSV 二选一)".to_string());
                    }
                    items
                } else {
                    inline_value
                        .split(',')
                        .map(|item| item.trim().to_string())
                        .filter(|item| !item.is_empty())
                        .collect()
                });
            }
            _ => {} // 未知键忽略:定义是数据文件,向前兼容
        }
    }

    let system_prompt = body.trim().to_string();
    if system_prompt.is_empty() {
        return Err("正文为空:frontmatter 之后的正文即 system prompt".to_string());
    }
    validate_agent_name(&name)?;
    Ok(AgentDef {
        name,
        description,
        model,
        tools,
        system_prompt,
    })
}

/// frontmatter 围栏切分(`---` 围栏 + 正文;无围栏整篇当正文)。
/// skills 定义复用同一解析纪律。
pub(crate) fn split_frontmatter(content: &str) -> Result<(&str, &str), String> {
    let content = content.trim_start_matches('\u{feff}');
    let Some(rest) = content.strip_prefix("---") else {
        return Ok(("", content));
    };
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    let Some(rest) = rest.strip_prefix('\n') else {
        return Ok(("", content));
    };
    let mut closing = None;
    let mut offset = 0;
    for line in rest.lines() {
        if line.trim_end() == "---" {
            closing = Some(offset);
            break;
        }
        offset += line.len() + 1; // +1 = '\n'
    }
    match closing {
        Some(end) => {
            let frontmatter = &rest[..end];
            let body = rest[end + 4.min(rest.len() - end)..].trim_start_matches('\n');
            Ok((frontmatter, body))
        }
        // 无闭合围栏:整篇当正文(容错)
        None => Ok(("", rest)),
    }
}

pub(crate) fn require_scalar(key: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("`{key}` 需要同行标量值"));
    }
    Ok(value.to_string())
}

fn validate_agent_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "agent 名 `{name}` 不合法:仅允许字母/数字开头,含 . _ - 的标识符"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_frontmatter() {
        let def = parse_agent_def(
            "---\nname: reviewer\ndescription: Reviews code\nmodel: anthropic/claude-sonnet-4-5\ntools:\n  - read\n  - grep\n---\n\nYou are a reviewer.\n",
            "fallback",
        )
        .unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Reviews code");
        assert_eq!(def.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
        assert_eq!(def.tools, Some(vec!["read".to_string(), "grep".to_string()]));
        assert_eq!(def.system_prompt, "You are a reviewer.");
    }

    #[test]
    fn csv_tools_and_default_name() {
        let def = parse_agent_def(
            "---\ndescription: d\ntools: read, grep ,find\n---\nbody",
            "scan-code",
        )
        .unwrap();
        assert_eq!(def.name, "scan-code");
        assert_eq!(
            def.tools,
            Some(vec!["read".into(), "grep".into(), "find".into()])
        );
        assert_eq!(def.system_prompt, "body");
        assert_eq!(def.model, None);
    }

    #[test]
    fn no_frontmatter_means_whole_body_is_prompt() {
        let def = parse_agent_def("Just do the thing.", "plain").unwrap();
        assert_eq!(def.name, "plain");
        assert_eq!(def.system_prompt, "Just do the thing.");
    }

    #[test]
    fn unclosed_frontmatter_falls_back_to_body() {
        let def = parse_agent_def("---\nname: broken\nbody text", "x").unwrap();
        assert_eq!(def.name, "x");
        assert_eq!(def.system_prompt, "name: broken\nbody text");
    }

    #[test]
    fn empty_body_is_error() {
        let error = parse_agent_def("---\nname: a\n---\n   \n", "a").unwrap_err();
        assert!(error.contains("正文为空"));
    }

    #[test]
    fn invalid_name_is_error() {
        let error = parse_agent_def("---\nname: has space\n---\nbody", "a").unwrap_err();
        assert!(error.contains("不合法"));
        let error = parse_agent_def("---\nname: -lead\n---\nbody", "a").unwrap_err();
        assert!(error.contains("不合法"));
    }

    #[test]
    fn unknown_keys_ignored_malformed_line_is_error() {
        parse_agent_def("---\nfutureKey: x\nname: ok\n---\nbody", "a").unwrap();
        let error = parse_agent_def("---\nno colon here\n---\nbody", "a").unwrap_err();
        assert!(error.contains("key: value"));
    }

    #[test]
    fn empty_inline_value_is_error() {
        let error = parse_agent_def("---\nname:\n---\nbody", "a").unwrap_err();
        assert!(error.contains("`name` 需要同行标量值"));
    }

    #[test]
    fn discovery_project_overrides_user_and_skips_bad_files() {
        let root = tempdir();
        let user_dir = root.join("home").join(".rpi").join("agents");
        let project_dir = root.join("project").join(".rpi").join("agents");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(user_dir.join("shared.md"), "---\ndescription: user\n---\nuser prompt").unwrap();
        std::fs::write(user_dir.join("only-user.md"), "---\ndescription: u\n---\nprompt").unwrap();
        std::fs::write(project_dir.join("shared.md"), "---\ndescription: project\n---\nproject prompt").unwrap();
        std::fs::write(project_dir.join("broken.md"), "---\nname: has space\n---\nprompt").unwrap();
        std::fs::write(project_dir.join("notes.txt"), "ignored").unwrap();

        let (defs, diagnostics) = discover_agent_defs(&root.join("project"), Some(&root.join("home")));
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["only-user", "shared"]);
        let shared = defs.iter().find(|d| d.name == "shared").unwrap();
        assert_eq!(shared.system_prompt, "project prompt", "项目级覆盖用户级");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("broken.md"));
    }

    #[test]
    fn discovery_missing_dirs_are_empty() {
        let (defs, diagnostics) =
            discover_agent_defs(Path::new("/nonexistent/project"), Some(Path::new("/nonexistent")));
        assert!(defs.is_empty());
        assert!(diagnostics.is_empty());
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rpi-subagent-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
