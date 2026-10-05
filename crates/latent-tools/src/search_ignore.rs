//! 检索忽略列表:grep/find/ls/@文件弹窗共享的"无效对象"过滤。
//!
//! 规则唯一来源是 `.latentignore` 文件(gitignore 语法):全局
//! `~/.latent/.latentignore` 在前、项目 `<cwd>/.latentignore` 在后拼接,按
//! gitignore 语义 last-match-wins —— 项目可用 `!` 反选全局规则。没有任何
//! 规则 = 关闭过滤(仅剩工具层写死的 `.git` 排除与遍历自带的 .gitignore
//! 感知)。
//!
//! 匹配语义对齐 .gitignore(经 `ignore` crate 的 `GitignoreBuilder`):
//! - 不含 `/` 的模式匹配任意深度的同名文件/目录;
//! - 含 `/` 的模式锚定到匹配根(装配期 = 会话 cwd,即项目 `.latentignore`
//!   所在目录),与检索子根(grep `path` 参数等)无关;
//! - `#` 注释、`dir/` 仅目录、`**` 跨段、`!` 反选。
//!
//! 与 .gitignore 一致的限制:目录命中忽略后遍历即剪枝,其子项无法用 `!`
//! 复活;不支持子目录级 `.latentignore`。

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// 检索忽略列表:编译后的 gitignore 匹配器 + 正向模式(供提示词规则回显)。
/// 克隆廉价(检索工具 + 装配层共享同一份)。
#[derive(Debug, Clone)]
pub struct SearchIgnore {
    /// 匹配根(装配期 = 会话 cwd);含 `/` 的模式锚定到此根
    root: PathBuf,
    gitignore: Gitignore,
    /// 正向(非 `!`、非注释)模式原文;系统提示词规则回显用
    positive: Vec<String>,
}

impl SearchIgnore {
    /// 从规则行编译(gitignore 语法;顺序即优先级,后行覆盖先行)。非法行
    /// 跳过并经 `on_error(行序号, 诊断)` 上报——手工编辑的配置一行写坏
    /// 不应废掉整个文件,其余行照常生效。
    pub fn from_patterns<I, S, F>(root: impl Into<PathBuf>, patterns: I, mut on_error: F) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
        F: FnMut(usize, String),
    {
        let root = root.into();
        let mut builder = GitignoreBuilder::new(&root);
        let mut positive: Vec<String> = Vec::new();
        for (index, line) in patterns.into_iter().enumerate() {
            let line = line.as_ref();
            // add_line 自行处理注释/空行/尾随空白与 glob 编译
            if let Err(error) = builder.add_line(None, line) {
                on_error(
                    index,
                    format!("invalid ignore pattern `{}`: {error}", line.trim_end()),
                );
                continue;
            }
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with('!') {
                positive.push(trimmed.to_string());
            }
        }
        match builder.build() {
            Ok(gitignore) => SearchIgnore {
                root,
                gitignore,
                positive,
            },
            Err(error) => {
                on_error(usize::MAX, format!("ignore patterns compile failed: {error}"));
                SearchIgnore {
                    root,
                    gitignore: Gitignore::empty(),
                    positive: Vec::new(),
                }
            }
        }
    }

    /// 未配置任何规则(关闭过滤)。
    pub fn is_empty(&self) -> bool {
        self.gitignore.is_empty()
    }

    /// 正向模式原文(供系统提示词规则回显;`!` 反选行不回显)。
    pub fn patterns(&self) -> &[String] {
        &self.positive
    }

    /// 匹配根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `path` 是否命中忽略列表。`path` 可为匹配根下的绝对路径(内部剥根
    /// 前缀)或相对匹配根的路径;`is_dir` 决定 `dir/` 仅目录模式是否命中。
    /// 绝对路径在匹配根之外时,锚定模式不生效、裸名模式仍按路径段匹配。
    pub fn matches(&self, path: &Path, is_dir: bool) -> bool {
        matches!(
            self.gitignore.matched(path, is_dir),
            ignore::Match::Ignore(_)
        )
    }
}

impl Default for SearchIgnore {
    fn default() -> Self {
        Self::from_patterns(PathBuf::new(), Vec::<String>::new(), |_, _| {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("latent-si-{name}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 构建辅助:坏行直接 panic(正例不含非法行)。
    fn build(root: &Path, patterns: &[&str]) -> SearchIgnore {
        SearchIgnore::from_patterns(root, patterns, |_, error| {
            panic!("unexpected bad pattern: {error}")
        })
    }

    #[test]
    fn bare_patterns_match_entry_at_any_depth() {
        let root = temp_root("bare");
        let ignore = build(&root, &["node_modules", "dist/"]);
        // 裸名模式命中任意深度的**条目本身**(gitignore 语义);子项是否
        // 被忽略由消费点对目录条目剪枝保证,模式不匹配子项路径
        assert!(ignore.matches(&root.join("node_modules"), true));
        assert!(ignore.matches(&root.join("src/node_modules"), true));
        assert!(!ignore.matches(&root.join("node_modules/react/index.js"), false));
        // 尾 `/` = 仅目录:目录命中,同名文件不命中
        assert!(ignore.matches(&root.join("a/b/dist"), true));
        assert!(!ignore.matches(&root.join("a/b/dist"), false), "`dist/` 仅目录");
        assert!(!ignore.matches(&root.join("src/a.ts"), false));
        assert!(!ignore.matches(&root.join("distribution"), true), "段级完整匹配,非前缀");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn anchored_patterns_resolve_against_root() {
        let root = temp_root("anchored");
        let ignore = build(&root, &["**/*.min.js", "src/generated/**", "/top-only.txt"]);
        assert!(ignore.matches(&root.join("dist/bundle.min.js"), false));
        assert!(ignore.matches(&root.join("src/generated/tables.rs"), false));
        assert!(!ignore.matches(&root.join("src/app.js"), false));
        assert!(!ignore.matches(&root.join("other/generated/x.rs"), false), "锚定到匹配根");
        // 前导 `/` 只匹配根自身层级
        assert!(ignore.matches(&root.join("top-only.txt"), false));
        assert!(!ignore.matches(&root.join("sub/top-only.txt"), false));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn negation_overrides_earlier_rules() {
        let root = temp_root("negation");
        // last-match-wins:`!` 反选更早的规则
        let ignore = build(&root, &["*.log", "!keep.log"]);
        assert!(ignore.matches(&root.join("a.log"), false));
        assert!(!ignore.matches(&root.join("keep.log"), false), "`!` 反选生效");
        // 拼接语义:后面的源(项目)反选前面的源(全局)
        let ignore = build(&root, &["target/", "!target/"]);
        assert!(!ignore.matches(&root.join("target/a"), true));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let root = temp_root("comments");
        let ignore = build(&root, &["# 注释行", "", "   ", "build"]);
        assert_eq!(ignore.patterns(), ["build"]);
        assert!(!ignore.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn empty_patterns_disable_filtering() {
        let root = temp_root("empty");
        let ignore = build(&root, &[]);
        assert!(ignore.is_empty());
        assert!(ignore.patterns().is_empty());
        assert!(!ignore.matches(&root.join("node_modules/x.js"), false));
        // Default 同样是"不过滤"
        assert!(SearchIgnore::default().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn invalid_pattern_is_reported_and_skipped() {
        let root = temp_root("invalid");
        let errors = std::sync::Mutex::new(Vec::new());
        let ignore = SearchIgnore::from_patterns(&root, ["a\\", "dist"], |index, error| {
            errors.lock().unwrap().push((index, error));
        });
        // 坏行上报,其余行照常生效(条目本身命中)
        assert!(ignore.matches(&root.join("dist"), true));
        let errors = errors.into_inner().unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].0, 0);
        assert!(errors[0].1.contains("invalid ignore pattern"), "{}", errors[0].1);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
