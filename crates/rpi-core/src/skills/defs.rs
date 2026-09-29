//! skill 发现(pi skills.ts 移植):skill 是数据目录(`<cwd>/.rpi/skills/<name>/SKILL.md`
//! 项目优先 → `~/.rpi/skills/`),frontmatter 只取 name/description 摘要。
//!
//! 解析纪律与 agent 定义一致:手写逐行 `key: value`,未知键忽略,失败单条
//! 诊断跳过不阻断其他文件(07 §8.5)。

use std::collections::BTreeMap;
use std::path::Path;

use crate::subagent::defs::{require_scalar, split_frontmatter};

/// 命名 skill(`<name>/SKILL.md` 的解析结果)。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDef {
    /// frontmatter `name`,缺省目录名
    pub name: String,
    pub description: String,
    /// SKILL.md 路径(load_skill 调用时读取,非启动缓存)
    pub path: std::path::PathBuf,
}

/// 发现两级目录的 skill;同名时项目级覆盖用户级。
/// 返回 (按 name 排序的定义, 诊断消息)。
pub fn discover_skill_defs(cwd: &Path, home: Option<&Path>) -> (Vec<SkillDef>, Vec<String>) {
    let mut diagnostics = Vec::new();
    let mut by_name: BTreeMap<String, SkillDef> = BTreeMap::new();
    // 后扫描的覆盖先扫描的:先用户级,后项目级(项目优先)
    let dirs = [
        home.map(|home| home.join(".rpi").join("skills")),
        Some(cwd.join(".rpi").join("skills")),
    ];
    for dir in dirs.into_iter().flatten() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                diagnostics.push(format!("无法读取 skill 目录 {}: {error}", dir.display()));
                continue;
            }
        };
        let mut skill_dirs: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        skill_dirs.sort();
        for skill_dir in skill_dirs {
            let skill_file = skill_dir.join("SKILL.md");
            // 无 SKILL.md 的目录不是 skill,静默跳过
            if !skill_file.is_file() {
                continue;
            }
            match parse_skill_def_file(&skill_file) {
                Ok(def) => {
                    by_name.insert(def.name.clone(), def);
                }
                Err(message) => diagnostics.push(format!(
                    "skill `{}` 解析失败,已跳过: {message}",
                    skill_file.display()
                )),
            }
        }
    }
    (by_name.into_values().collect(), diagnostics)
}

fn parse_skill_def_file(path: &Path) -> Result<SkillDef, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let dir_name = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .ok_or_else(|| "目录名不是合法 UTF-8".to_string())?;
    let (name, description) = parse_skill_def(&content, dir_name)?;
    Ok(SkillDef {
        name,
        description,
        path: path.to_path_buf(),
    })
}

/// 解析单个 skill 的 frontmatter 摘要;返回 (name, description)。
/// 正文(SKILL.md 的全部指令)不在此解析,由 load_skill 调用时原样读取。
pub fn parse_skill_def(content: &str, default_name: &str) -> Result<(String, String), String> {
    let (frontmatter, _body) = split_frontmatter(content)?;
    let mut name = default_name.to_string();
    let mut description: Option<String> = None;

    for line in frontmatter.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("frontmatter 行缺少 `key: value` 形态: `{trimmed}`"));
        };
        match key.trim() {
            "name" => name = require_scalar("name", value.trim())?,
            "description" => description = Some(require_scalar("description", value.trim())?),
            _ => {} // 未知键忽略:定义是数据文件,向前兼容
        }
    }

    // description 必填:它是模型决定是否加载 skill 的唯一依据
    let description = description
        .ok_or_else(|| "frontmatter 缺少 `description`(模型据其决定是否加载本 skill)".to_string())?;
    validate_skill_name(&name)?;
    Ok((name, description))
}

fn validate_skill_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "skill 名 `{name}` 不合法:仅允许字母/数字开头,含 . _ - 的标识符"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_skill(dir: &Path, name: &str, content: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parses_frontmatter_name_overrides_dir() {
        let (name, description) =
            parse_skill_def("---\nname: review\ndescription: Reviews code\n---\nbody", "dir-name")
                .unwrap();
        assert_eq!(name, "review");
        assert_eq!(description, "Reviews code");
    }

    #[test]
    fn missing_description_is_error() {
        let error = parse_skill_def("---\nname: a\n---\nbody", "a").unwrap_err();
        assert!(error.contains("description"));
    }

    #[test]
    fn unknown_keys_ignored_malformed_line_is_error() {
        parse_skill_def("---\nlicense: MIT\nname: ok\ndescription: d\n---\nbody", "ok").unwrap();
        let error = parse_skill_def("---\nno colon here\n---\nbody", "a").unwrap_err();
        assert!(error.contains("key: value"));
    }

    #[test]
    fn invalid_name_is_error() {
        let error = parse_skill_def("---\nname: has space\ndescription: d\n---\nbody", "a")
            .unwrap_err();
        assert!(error.contains("不合法"));
    }

    #[test]
    fn discovery_project_overrides_user_and_skips_bad_files() {
        let root = tempdir();
        let user_dir = root.join("home").join(".rpi").join("skills");
        let project_dir = root.join("project").join(".rpi").join("skills");
        write_skill(
            &user_dir,
            "shared",
            "---\ndescription: user\n---\nuser body",
        );
        write_skill(&user_dir, "only-user", "---\ndescription: u\n---\nbody");
        write_skill(
            &project_dir,
            "shared",
            "---\nname: shared\ndescription: project\n---\nproject body",
        );
        write_skill(&project_dir, "broken", "---\nname: has space\n---\nbody");
        // 无 SKILL.md 的目录不是 skill
        std::fs::create_dir_all(project_dir.join("empty")).unwrap();
        // 散落的 md 文件不是 skill,静默跳过
        std::fs::write(project_dir.join("loose.md"), "---\ndescription: d\n---\nbody").unwrap();

        let (defs, diagnostics) =
            discover_skill_defs(&root.join("project"), Some(&root.join("home")));
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["only-user", "shared"]);
        let shared = defs.iter().find(|d| d.name == "shared").unwrap();
        assert_eq!(shared.description, "project", "项目级覆盖用户级");
        assert_eq!(shared.path, project_dir.join("shared").join("SKILL.md"));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("broken"));
    }

    #[test]
    fn discovery_missing_dirs_are_empty() {
        let (defs, diagnostics) =
            discover_skill_defs(Path::new("/nonexistent/project"), Some(Path::new("/nonexistent")));
        assert!(defs.is_empty());
        assert!(diagnostics.is_empty());
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rpi-skills-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
