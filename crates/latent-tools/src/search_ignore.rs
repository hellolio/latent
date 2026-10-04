//! 检索忽略列表:grep/find/ls 三个检索工具共享的"无效对象"过滤。
//!
//! 动机:依赖目录与构建产物(node_modules、dist、target…)即使不在
//! .gitignore 里,检索它们也毫无意义,只会淹没结果、浪费轮次。本模块把
//! "要忽略什么"收敛为一个可注入配置:未配置 = 内置默认表;settings.json
//! `searchIgnore` 配置后**整体覆盖**默认表(空数组 = 完全关闭过滤)。
//!
//! 匹配语义(对相对搜索根的路径判定):
//! - 不含 `/` 的条目:匹配相对路径的**任意一段**(目录名/文件名,如
//!   `node_modules` 命中任意深度的同名目录);
//! - 含 `/` 的条目:按 glob 锚定匹配相对路径整体(如 `**/*.min.js`)。

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// 内置默认忽略表:常见依赖目录、构建产物与工具缓存。
/// settings `searchIgnore` 未配置时生效;配置后整体覆盖。
const DEFAULT_PATTERNS: &[&str] = &[
    // 依赖目录(JS/Python/PHP/Go/iOS)
    "node_modules",
    "vendor",
    "Pods",
    // 构建产物
    "dist",
    "build",
    "out",
    "target",
    "obj",
    "DerivedData",
    "coverage",
    // 语言/工具缓存
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".turbo",
    ".parcel-cache",
    ".cache",
    // 虚拟环境
    ".venv",
    "venv",
    ".tox",
    // 框架产物
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".output",
    // IDE / 构建系统内部目录
    ".idea",
    ".vs",
    ".gradle",
    ".terraform",
];

/// 检索忽略列表:编译后的 glob 集合 + 原始条目(供提示词规则回显)。
/// 克隆廉价(三个检索工具 + 装配层共享同一份)。
#[derive(Debug, Clone, Default)]
pub struct SearchIgnore {
    /// 原始条目(去空白后;供系统提示词规则回显与诊断)
    patterns: Vec<String>,
    /// 不含 `/` 的条目:匹配任一路径段
    components: GlobSet,
    /// 含 `/` 的条目:锚定匹配相对路径整体(literal_separator,`*` 不跨段)
    anchored: GlobSet,
}

impl SearchIgnore {
    /// 从条目列表编译;非法 glob 返回 Err(配置错误要显式暴露,不静默丢弃)。
    /// 空白条目跳过;空列表 = 关闭过滤(两集合皆空,matches 恒 false)。
    pub fn from_patterns<I, S>(patterns: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut kept: Vec<String> = Vec::new();
        let mut components = GlobSetBuilder::new();
        let mut anchored = GlobSetBuilder::new();
        for pattern in patterns {
            let pattern = pattern.as_ref().trim();
            if pattern.is_empty() {
                continue;
            }
            // strip 尾 `/`:目录条目两种写法等价(`node_modules/` ≈ `node_modules`)
            let pattern = pattern.trim_end_matches('/');
            if pattern.is_empty() {
                continue;
            }
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("invalid ignore pattern `{pattern}`: {e}"))?;
            if pattern.contains('/') {
                anchored.add(glob);
            } else {
                components.add(glob);
            }
            kept.push(pattern.to_string());
        }
        Ok(SearchIgnore {
            components: components.build().map_err(|e| e.to_string())?,
            anchored: anchored.build().map_err(|e| e.to_string())?,
            patterns: kept,
        })
    }

    /// 内置默认表(`searchIgnore` 未配置时)。
    pub fn builtin() -> Self {
        // 静态表不含非法 glob,unwrap 安全
        Self::from_patterns(DEFAULT_PATTERNS.iter().copied()).expect("builtin patterns valid")
    }

    /// 未配置任何条目(空列表 = 关闭过滤)。
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// 原始条目(供提示词规则回显)。
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// 相对搜索根的路径是否命中忽略列表:任一路径段命中组件条目,或
    /// 整条相对路径命中锚定条目。绝对路径请先 `strip_prefix` 搜索根。
    pub fn matches(&self, relative: &Path) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        for component in relative.components() {
            let name = component.as_os_str();
            // `.`/`..` 等特殊段不参与匹配
            if name == "." || name == ".." || name.is_empty() {
                continue;
            }
            if self.components.is_match(name) {
                return true;
            }
        }
        let text = relative.to_string_lossy().replace('\\', "/");
        self.anchored.is_match(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(ignore: &SearchIgnore, path: &str) -> bool {
        ignore.matches(Path::new(path))
    }

    #[test]
    fn component_patterns_match_at_any_depth() {
        let ignore = SearchIgnore::from_patterns(["node_modules", "dist/"]).unwrap();
        assert!(matches(&ignore, "node_modules"));
        assert!(matches(&ignore, "node_modules/react/index.js"));
        assert!(matches(&ignore, "src/node_modules/x.ts"));
        // 尾 `/` 已剥:目录条目两种写法等价
        assert!(matches(&ignore, "a/b/dist/c.js"));
        assert!(!matches(&ignore, "src/a.ts"));
        assert!(!matches(&ignore, "distribution/x.ts"), "段级完整匹配,非前缀");
    }

    #[test]
    fn anchored_patterns_match_relative_path() {
        let ignore = SearchIgnore::from_patterns(["**/*.min.js", "src/generated/**"]).unwrap();
        assert!(matches(&ignore, "dist/bundle.min.js"));
        assert!(matches(&ignore, "src/generated/tables.rs"));
        assert!(!matches(&ignore, "src/app.js"));
        assert!(!matches(&ignore, "other/generated/x.rs"), "锚定到搜索根");
    }

    #[test]
    fn empty_list_disables_filtering() {
        let ignore = SearchIgnore::from_patterns(Vec::<String>::new()).unwrap();
        assert!(ignore.is_empty());
        assert!(!matches(&ignore, "node_modules/x.js"));
        // 空白条目 = 未配置
        let ignore = SearchIgnore::from_patterns(["  ", ""]).unwrap();
        assert!(ignore.is_empty());
    }

    #[test]
    fn invalid_glob_is_an_error() {
        let err = SearchIgnore::from_patterns(["a[.ts"]).unwrap_err();
        assert!(err.contains("invalid ignore pattern"), "{err}");
    }

    #[test]
    fn builtin_covers_common_noise() {
        let ignore = SearchIgnore::builtin();
        for path in [
            "node_modules/react/index.js",
            "dist/bundle.js",
            "target/debug/build.rs",
            "src/__pycache__/mod.cpython.pyc",
            ".venv/lib/python.py",
            ".next/server/page.js",
        ] {
            assert!(matches(&ignore, path), "应忽略 {path}");
        }
        assert!(!matches(&ignore, "src/main.rs"));
        assert!(!matches(&ignore, "Cargo.toml"));
        assert!(!matches(&ignore, ".github/workflows/ci.yml"));
    }

    #[test]
    fn traversal_segments_do_not_match() {
        // 相对路径里的 `.`/`..` 段不参与匹配(不会因 "." 命中 ".idea" 类条目)
        let ignore = SearchIgnore::from_patterns([".idea"]).unwrap();
        assert!(matches(&ignore, "./.idea/x"));
        assert!(!matches(&ignore, "./idea/x"));
    }
}
