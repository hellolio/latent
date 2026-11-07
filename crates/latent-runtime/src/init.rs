//! `/init` 命令:把提示词作为 user 消息提交,驱动模型分析代码库并创建/更新
//! 项目根 AGENTS.md(新会话装配期经 `load_agents_md` 注入为项目上下文)。
//!
//! 提示词外置覆盖(用户可编辑):项目 `.latent/init-prompt.md` → 全局数据目录
//! `init-prompt.md`,首个存在且非空的文件**整体替换**内置提示词。查找模式与
//! compact-prompt.md 同型,但读取时机不同:`/init` 是低频命令,dispatch 时
//! 读取,会话中途修改即时生效(不遵循外置提示词装配期读一次的约定)。
//! 用户附加要求(`/init <附加要求>`)以空行拼在提示词尾部。

use std::path::Path;

/// 内置默认提示词(参照 Claude Code /init,三块:常用命令/大图架构/目录索引)。
pub const DEFAULT_INIT_PROMPT: &str = r#"Please analyze this codebase and create (or update) an AGENTS.md file at the repository root, which will be given to future instances of latent to operate in this repository.

What to add:
1. Commands that will be commonly used, such as how to build, lint, and run tests, including how to run a single test. Only list commands you verified from manifests, CI configs, or docs; never invent one.
2. High-level architecture: layering, module boundaries, dependency direction, and key invariants — the big picture that requires reading multiple files to understand.
3. Directory index: for each first-party module/directory, a table listing every source file with a one-line description of its responsibility. Skip build outputs, generated/vendored code, and lockfiles.

Usage notes:
- If AGENTS.md already exists, improve it in place: keep accurate content, fix only what is stale or missing, never rewrite blindly.
- Do not include generic advice or fabricated sections; when in doubt, omit.
- Language: match existing AGENTS.md/README.md; if none, use English.
- Begin the file with exactly this header:

# AGENTS.md

This file provides guidance to latent (an AI coding agent) when working with code in this repository.

When done, remind the user: AGENTS.md loads into context when a new session starts."#;

/// 组装 `/init` 提示词:外置覆盖优先(无则内置默认),附加要求以空行拼在尾部。
pub fn build_prompt(arg: Option<&str>) -> String {
    assemble(load_prompt_override().as_deref(), arg)
}

/// 纯函数面(可测):base 覆盖优先于内置默认,非空附加要求拼尾部。
fn assemble(base_override: Option<&str>, arg: Option<&str>) -> String {
    let base = base_override.unwrap_or(DEFAULT_INIT_PROMPT).trim();
    match arg.map(str::trim).filter(|arg| !arg.is_empty()) {
        Some(arg) => format!("{base}\n\n{arg}"),
        None => base.to_string(),
    }
}

/// dispatch 时读取:项目 `.latent/init-prompt.md` → 全局数据目录
/// `init-prompt.md`,首个存在且非空的文件生效。
pub fn load_prompt_override() -> Option<String> {
    let cwd = std::env::current_dir().ok();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let dir = latent_core::latent_dir(home.as_deref());
    prompt_override_from(cwd.as_deref(), dir.as_deref())
}

/// 纯函数面(可测):按 项目 → 全局 找 init-prompt.md,空白文件视为未配置。
fn prompt_override_from(cwd: Option<&Path>, latent_dir: Option<&Path>) -> Option<String> {
    let mut paths = Vec::new();
    if let Some(cwd) = cwd {
        paths.push(cwd.join(".latent/init-prompt.md"));
    }
    if let Some(dir) = latent_dir {
        paths.push(dir.join("init-prompt.md"));
    }
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Some(trimmed.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "latent_init_prompt_test_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn override_lookup_prefers_project_then_global_and_skips_blank() {
        // 双双缺失 → None
        let project = TempDir::new("missing");
        let global = TempDir::new("missing");
        assert_eq!(
            prompt_override_from(Some(&project.0), Some(&global.0)),
            None
        );

        // 项目优先
        let project = TempDir::new("project");
        project.write(".latent/init-prompt.md", "project init prompt\n");
        let global = TempDir::new("project");
        global.write("init-prompt.md", "global init prompt\n");
        assert_eq!(
            prompt_override_from(Some(&project.0), Some(&global.0)).as_deref(),
            Some("project init prompt")
        );

        // 项目空白文件视为未配置 → 回退全局
        let project = TempDir::new("blank");
        project.write(".latent/init-prompt.md", "   \n\t");
        let global = TempDir::new("blank");
        global.write("init-prompt.md", "global init prompt\n");
        assert_eq!(
            prompt_override_from(Some(&project.0), Some(&global.0)).as_deref(),
            Some("global init prompt")
        );
    }

    #[test]
    fn assemble_uses_default_appends_arg_and_honors_override() {
        // 无覆盖无参数 = 内置默认(裁剪后)
        assert_eq!(assemble(None, None), DEFAULT_INIT_PROMPT);

        // 附加要求以空行拼在尾部(空白参数视为无)
        let with_arg = assemble(None, Some("  重点是测试命令  "));
        assert!(with_arg.starts_with(DEFAULT_INIT_PROMPT));
        assert!(with_arg.ends_with("\n\n重点是测试命令"));
        assert_eq!(assemble(None, Some("   ")), DEFAULT_INIT_PROMPT);

        // 外置覆盖整体替换内置默认
        let overridden = assemble(Some("custom prompt"), Some("focus"));
        assert_eq!(overridden, "custom prompt\n\nfocus");
    }
}
