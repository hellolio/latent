//! 文件清单采集:交互 TUI `@` 文件选择弹窗的数据源。
//!
//! 语义对齐 pi 的 `fd` 采集(`--hidden --exclude .git` + .gitignore 感知),
//! 但进程内用 `ignore` crate 完成,不依赖外部二进制;叠加 search_ignore
//! 检索忽略列表剪枝(`.latentignore` 规则,依赖/构建目录不进候选)。产出
//! 扁平的相对路径列表(文件 + 目录),按 深度 → 目录优先 → 字母 排序后
//! 截断——排序先于截断,保证浅层条目不被深层子树挤掉。

use std::path::Path;

use crate::search_ignore::SearchIgnore;

/// 单个清单条目:相对采集根的路径(`/` 分隔,目录不带尾缀 `/`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingEntry {
    pub path: String,
    pub is_dir: bool,
}

/// 默认采集上限:超过后不再向深层扩展(排序仍在本集合内完成)。
/// 上限取的是一个"弹窗体验足够、遍历开销可忽略"的平衡值。
pub const DEFAULT_MAX_ENTRIES: usize = 2000;

/// 遍历阶段的硬上限(排序截断前);超出说明仓库异常庞大,按已采集的
/// 排序截断即可。4 倍余量给"深层子树挤占"留空间。
const WALK_HARD_CAP: usize = DEFAULT_MAX_ENTRIES * 4;

/// 采集 root 下全部文件与目录(相对路径;root 自身不入列表)。
/// root 不存在或不是目录时返回空列表(弹窗场景静默降级,无错误面)。
pub fn collect_entries(root: &Path, ignore: &SearchIgnore) -> Vec<ListingEntry> {
    collect_entries_capped(root, ignore, DEFAULT_MAX_ENTRIES)
}

/// 同 `collect_entries`,可自定义排序截断上限。
pub fn collect_entries_capped(
    root: &Path,
    ignore: &SearchIgnore,
    max_entries: usize,
) -> Vec<ListingEntry> {
    if max_entries == 0 || !root.is_dir() {
        return Vec::new();
    }
    let mut walker = ignore::WalkBuilder::new(root);
    // pi 传 --hidden:包含隐藏文件;.git 始终跳过;仓库外也应用 .gitignore
    // (require_git(false),与 grep/find 的遍历口径一致)
    walker
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    // 检索忽略列表(.latentignore 规则):命中目录直接剪枝(不进入遍历);
    // 深度 0(采集根本身)不参与过滤——显式指定的根永不剪枝
    if !ignore.is_empty() {
        let ignore = ignore.clone();
        walker.filter_entry(move |entry| {
            if entry.file_name() == ".git" {
                return false;
            }
            if entry.depth() == 0 {
                return true;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            !ignore.matches(entry.path(), is_dir)
        });
    }
    let mut collected: Vec<(usize, ListingEntry)> = Vec::new();
    for entry in walker.build().flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        collected.push((
            entry.depth(),
            ListingEntry {
                path: relative.to_string_lossy().replace('\\', "/"),
                is_dir,
            },
        ));
        if collected.len() >= WALK_HARD_CAP {
            break;
        }
    }
    collected.sort_by(|a, b| {
        (a.0, !a.1.is_dir, &a.1.path).cmp(&(b.0, !b.1.is_dir, &b.1.path))
    });
    collected.truncate(max_entries);
    collected.into_iter().map(|(_, entry)| entry).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("latent-listing-{name}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("src/ui")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/react")).unwrap();
        std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
        for p in [
            "README.md",
            "Cargo.toml",
            ".hidden",
            "src/main.rs",
            "src/lib.rs",
            "src/ui/button.rs",
            "node_modules/react/index.js",
            ".git/HEAD",
        ] {
            std::fs::write(dir.join(p), "x").unwrap();
        }
        dir
    }

    fn paths(entries: &[ListingEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.path.as_str()).collect()
    }

    /// 以采集根为匹配根构建忽略列表(坏行 panic:测试不含非法行)。
    fn ignore_for(root: &Path, patterns: &[&str]) -> SearchIgnore {
        SearchIgnore::from_patterns(root, patterns, |_, error| panic!("{error}"))
    }

    #[test]
    fn collects_files_and_dirs_pruning_noise() {
        let dir = setup("basic");
        let entries = collect_entries(&dir, &ignore_for(&dir, &["node_modules"]));
        let all = paths(&entries);
        // 文件与目录都在;隐藏文件包含(pi --hidden)
        for expected in ["README.md", "Cargo.toml", ".hidden", "src", "src/main.rs", "src/ui", "src/ui/button.rs"] {
            assert!(all.contains(&expected), "缺少 {expected}: {all:?}");
        }
        // .git 与检索忽略目录(node_modules)整棵剪枝
        assert!(!all.iter().any(|p| p.contains(".git")), "{all:?}");
        assert!(!all.iter().any(|p| p.contains("node_modules")), "{all:?}");
        // 目录条目不带尾缀
        let src = entries.iter().find(|e| e.path == "src").unwrap();
        assert!(src.is_dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn orders_by_depth_dirs_first_then_alpha() {
        let dir = setup("order");
        let entries = collect_entries(&dir, &ignore_for(&dir, &["node_modules"]));
        let all = paths(&entries);
        // 深度 1 的目录(src)在深度 1 的文件前;文件按字母序
        let src_pos = all.iter().position(|p| *p == "src").unwrap();
        let hidden_pos = all.iter().position(|p| *p == ".hidden").unwrap();
        let readme_pos = all.iter().position(|p| *p == "README.md").unwrap();
        assert!(src_pos < hidden_pos && hidden_pos < readme_pos, "{all:?}");
        // 深度 2 在深度 1 之后
        let main_pos = all.iter().position(|p| *p == "src/main.rs").unwrap();
        assert!(main_pos > readme_pos, "{all:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cap_truncates_after_sorting() {
        let dir = setup("cap");
        let entries = collect_entries_capped(&dir, &ignore_for(&dir, &["node_modules"]), 3);
        // 排序后截断:深度 1 的 src 在最前,深层条目被截掉
        assert_eq!(paths(&entries), vec!["src", ".hidden", "Cargo.toml"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_or_missing_root_yields_empty() {
        assert!(collect_entries(Path::new("/nonexistent-latent-xyz"), &SearchIgnore::default()).is_empty());
        let file = std::env::temp_dir().join(format!("latent-listing-file-{}", uuid::Uuid::now_v7()));
        std::fs::write(&file, "x").unwrap();
        assert!(collect_entries(&file, &SearchIgnore::default()).is_empty(), "文件根不是目录");
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn respects_gitignore() {
        let dir = setup("gitignore");
        std::fs::create_dir_all(dir.join("generated")).unwrap();
        std::fs::write(dir.join("generated/out.rs"), "x").unwrap();
        std::fs::write(dir.join(".gitignore"), "generated/\n").unwrap();
        let entries = collect_entries(&dir, &SearchIgnore::default());
        let all = paths(&entries);
        assert!(!all.iter().any(|p| p.contains("generated")), "gitignore 应生效: {all:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn latentignore_rules_prune_and_negation_cannot_resurrect_inside_ignored_dir() {
        let dir = setup("latentignore");
        std::fs::write(dir.join("NOTES.md"), "x").unwrap();
        std::fs::create_dir_all(dir.join("build/keep")).unwrap();
        std::fs::write(dir.join("build/keep/x.rs"), "x").unwrap();
        // build 被忽略 → 整棵剪枝;目录内 ! 反选无法复活(git 同款限制)
        let entries = collect_entries(&dir, &ignore_for(&dir, &["build/", "!build/keep/"]));
        let all = paths(&entries);
        assert!(!all.iter().any(|p| p.contains("build")), "{all:?}");
        // 反选顶层文件:同规则集下 NOTES.md 被过滤、README.md 恢复可见
        let entries = collect_entries(&dir, &ignore_for(&dir, &["*.md", "!README.md"]));
        let all = paths(&entries);
        assert!(all.contains(&"README.md"), "{all:?}");
        assert!(!all.contains(&"NOTES.md"), "{all:?}");
        assert!(all.contains(&"Cargo.toml"), "非 .md 条目不受影响: {all:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
