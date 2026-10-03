//! ls 工具(05 文档 §9):目录列表,目录加 `/` 后缀,含 dotfiles,默认 500 条。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use rpi_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::search_ignore::SearchIgnore;
use crate::truncate::{truncate_head, OutputLimits};

const DEFAULT_LIMIT: usize = 500;

pub struct LsTool {
    cwd: PathBuf,
    limits: OutputLimits,
    ignore: Arc<SearchIgnore>,
    description: String,
}

/// 工厂。
pub fn create_ls_tool(cwd: &Path) -> Arc<dyn Tool> {
    create_ls_tool_with_limits(cwd, OutputLimits::default(), Arc::new(SearchIgnore::builtin()))
}

/// 工厂 + 输出上限注入(装配层统一派生值,truncate 模块文档)+ 检索忽略列表
/// (search_ignore 模块;settings `searchIgnore` 配置,未配置 = 内置默认表)。
pub fn create_ls_tool_with_limits(
    cwd: &Path,
    limits: OutputLimits,
    ignore: Arc<SearchIgnore>,
) -> Arc<dyn Tool> {
    let description = format!(
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for \
         directories. Includes dotfiles. Dependency/build directories (node_modules, dist, \
         target, ...) are hidden. Output is truncated to {DEFAULT_LIMIT} entries or {} bytes \
         (whichever is hit first).",
        limits.effective_max_bytes()
    );
    Arc::new(LsTool {
        cwd: cwd.to_path_buf(),
        limits,
        ignore,
        description,
    })
}

fn parse_args(args: &serde_json::Value) -> Result<(Option<String>, usize), String> {
    let obj = args.as_object().ok_or("arguments must be an object")?;
    let path = match obj.get("path") {
        Some(v) if v.is_string() => Some(v.as_str().unwrap().to_string()),
        _ => None,
    };
    let limit = match obj.get("limit") {
        Some(v) if !v.is_null() => v.as_u64().ok_or("`limit` must be a positive integer")? as usize,
        _ => DEFAULT_LIMIT,
    };
    Ok((path, limit))
}

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list (default: current directory)"},
                "limit": {"type": "integer", "description": "Maximum number of entries to return (default: 500)"}
            }
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some("List directory contents".into())
    }

    async fn execute(
        &self,
        call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let (path, limit) = parse_args(&call.args).map_err(|message| ToolError::Failed {
            name: "ls".into(),
            message,
        })?;

        let dir = match &path {
            Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.cwd.join(p),
            None => self.cwd.clone(),
        };
        if !dir.exists() {
            return Err(ToolError::Failed {
                name: "ls".into(),
                message: format!("Path not found: {}", dir.display()),
            });
        }
        if !dir.is_dir() {
            return Err(ToolError::Failed {
                name: "ls".into(),
                message: format!("Not a directory: {}", dir.display()),
            });
        }

        let mut entries: Vec<String> = std::fs::read_dir(&dir)
            .map_err(|e| ToolError::Failed {
                name: "ls".into(),
                message: format!("Cannot read directory: {e}"),
            })?
            .filter_map(|entry| entry.ok())
            // 检索忽略列表(search_ignore 模块):单层列表按条目名判定
            .filter(|entry| !self.ignore.matches(Path::new(&entry.file_name())))
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        // 字母序,大小写不敏感(pi 的 sort)
        entries.sort_by_key(|a| a.to_lowercase());

        let mut results: Vec<String> = Vec::new();
        let mut entry_limit_reached = false;
        for entry in entries {
            if results.len() >= limit {
                entry_limit_reached = true;
                break;
            }
            let suffix = if dir.join(&entry).is_dir() { "/" } else { "" };
            results.push(format!("{entry}{suffix}"));
        }

        if results.is_empty() {
            return Ok(ToolOutput::text("(empty directory)"));
        }

        let raw = results.join("\n");
        // 条目数已被 limit 封顶,这里只剩字节限
        let truncation = truncate_head(&raw, usize::MAX, self.limits.effective_max_bytes());
        let mut output = truncation.content.clone();
        let mut notices: Vec<String> = Vec::new();
        let mut details = serde_json::Map::new();
        if entry_limit_reached {
            notices.push(format!(
                "{limit} entries limit reached. Use limit={} for more",
                limit * 2
            ));
            details.insert("entryLimitReached".into(), json!(limit));
        }
        if truncation.truncated {
            notices.push(format!("{} bytes limit reached", self.limits.effective_max_bytes()));
            details.insert(
                "truncation".into(),
                serde_json::to_value(&truncation).unwrap_or_default(),
            );
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }
        Ok(ToolOutput {
            output,
            details: if details.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::Object(details)
            },
            terminate: false,
        })
    }
}


#[cfg(test)]
fn test_tool(cwd: PathBuf) -> LsTool {
    LsTool {
        cwd,
        limits: OutputLimits::default(),
        ignore: Arc::new(SearchIgnore::builtin()),
        description: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;
    #[async_trait]
    impl ToolUpdater for Noop {
        async fn update(&self, _partial: String) {}
    }

    async fn exec(tool: &LsTool, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        tool.execute(
            ToolCall {
                id: "t".into(),
                name: "ls".into(),
                args,
            },
            CancellationToken::new(),
            &Noop,
        )
        .await
    }

    #[tokio::test]
    async fn lists_entries_sorted_with_dir_suffix_and_dotfiles() {
        let dir = std::env::temp_dir().join(format!("rpi-ls-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(dir.join("sub")).await.unwrap();
        for p in [".hidden", "Zebra.txt", "apple.txt", "sub/inner.txt"] {
            tokio::fs::write(dir.join(p), "x").await.unwrap();
        }
        let tool = test_tool(dir.clone());
        let output = exec(&tool, json!({})).await.unwrap();
        let lines: Vec<&str> = output.output.lines().collect();
        assert_eq!(lines, vec![".hidden", "apple.txt", "sub/", "Zebra.txt"]);
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn empty_directory_and_limit() {
        let dir = std::env::temp_dir().join(format!("rpi-ls-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let tool = test_tool(dir.clone());
        let output = exec(&tool, json!({})).await.unwrap();
        assert_eq!(output.output, "(empty directory)");

        for i in 0..3 {
            tokio::fs::write(dir.join(format!("f{i}.txt")), "x")
                .await
                .unwrap();
        }
        let output = exec(&tool, json!({"limit": 2})).await.unwrap();
        assert!(output.output.contains("2 entries limit reached"));
        assert_eq!(output.details["entryLimitReached"], json!(2));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn missing_path_and_file_path_are_errors() {
        let dir = std::env::temp_dir().join(format!("rpi-ls-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("f.txt"), "x").await.unwrap();
        let tool = test_tool(dir.clone());
        let err = exec(&tool, json!({"path": "missing"})).await.unwrap_err();
        assert!(err.to_string().contains("Path not found"));
        let err = exec(&tool, json!({"path": "f.txt"})).await.unwrap_err();
        assert!(err.to_string().contains("Not a directory"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    // ---- 检索忽略列表(search_ignore):依赖/构建目录不出现在列表里 ----
    #[tokio::test]
    async fn hides_dependency_and_build_dirs() {
        let dir = std::env::temp_dir().join(format!("rpi-ls-ignore-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(dir.join("src")).await.unwrap();
        tokio::fs::create_dir_all(dir.join("node_modules")).await.unwrap();
        tokio::fs::create_dir_all(dir.join("dist")).await.unwrap();
        tokio::fs::write(dir.join("a.ts"), "x").await.unwrap();
        let tool = test_tool(dir.clone());
        let output = exec(&tool, json!({})).await.unwrap();
        let lines: Vec<&str> = output.output.lines().collect();
        assert_eq!(lines, vec!["a.ts", "src/"]);
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}

