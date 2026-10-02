//! find 工具(05 文档 §8):glob 文件查找,尊重 .gitignore,目录匹配保留尾 `/`。
//!
//! pi 用外部 fd 二进制;本实现用 `ignore` crate 的 override glob(gitignore
//! 语义,含 `/` 的 pattern 自动锚定到搜索根,等价 fd 的 --full-path 行为)。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use rpi_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::truncate::{truncate_head, OutputLimits};

const DEFAULT_LIMIT: usize = 1000;

pub struct FindTool {
    cwd: PathBuf,
    limits: OutputLimits,
    description: String,
}

/// 工厂。
pub fn create_find_tool(cwd: &Path) -> Arc<dyn Tool> {
    create_find_tool_with_limits(cwd, OutputLimits::default())
}

/// 工厂 + 输出上限注入(装配层统一派生值,truncate 模块文档)。
pub fn create_find_tool_with_limits(cwd: &Path, limits: OutputLimits) -> Arc<dyn Tool> {
    let description = format!(
        "Search for files by glob pattern. Returns matching file paths relative to the search \
         directory. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} results or {} \
         bytes (whichever is hit first).",
        limits.effective_max_bytes()
    );
    Arc::new(FindTool {
        cwd: cwd.to_path_buf(),
        limits,
        description,
    })
}

fn parse_args(args: &serde_json::Value) -> Result<(String, Option<String>, usize), String> {
    let obj = args.as_object().ok_or("arguments must be an object")?;
    let pattern = obj
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or("missing required argument `pattern`")?
        .to_string();
    let path = match obj.get("path") {
        Some(v) if v.is_string() => Some(v.as_str().unwrap().to_string()),
        _ => None,
    };
    let limit = match obj.get("limit") {
        Some(v) if !v.is_null() => v.as_u64().ok_or("`limit` must be a positive integer")? as usize,
        _ => DEFAULT_LIMIT,
    };
    Ok((pattern, path, limit))
}

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},
                "path": {"type": "string", "description": "Directory to search in (default: current directory)"},
                "limit": {"type": "integer", "description": "Maximum number of results (default: 1000)"}
            }
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some("Find files by glob pattern (respects .gitignore)".into())
    }

    async fn execute(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let (pattern, path, limit) =
            parse_args(&call.args).map_err(|message| ToolError::Failed {
                name: "find".into(),
                message,
            })?;

        let search_root = match &path {
            Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.cwd.join(p),
            None => self.cwd.clone(),
        };
        if !search_root.is_dir() {
            return Err(ToolError::Failed {
                name: "find".into(),
                message: format!("Path not found: {}", search_root.display()),
            });
        }

        let mut overrides = ignore::overrides::OverrideBuilder::new(&search_root);
        overrides.add(&pattern).map_err(|e| ToolError::Failed {
            name: "find".into(),
            message: format!("invalid glob `{pattern}`: {e}"),
        })?;
        let overrides = overrides.build().map_err(|e| ToolError::Failed {
            name: "find".into(),
            message: format!("invalid glob `{pattern}`: {e}"),
        })?;

        let mut walker = ignore::WalkBuilder::new(&search_root);
        // pi 传 --hidden:包含隐藏文件,但仍尊重 .gitignore;仓库外也应用 .gitignore
        // (fd 的 --no-require-git 行为);.git 目录始终跳过。override 不交给 walker
        // (目录白名单会把父目录也带进来),改为逐 entry 匹配。
        walker
            .hidden(false)
            .require_git(false)
            .filter_entry(|entry| entry.file_name() != ".git");
        let walker = walker.build();

        let mut results: Vec<String> = Vec::new();
        let mut result_limit_reached = false;
        for entry in walker {
            if cancel.is_cancelled() {
                return Err(ToolError::Aborted {
                    name: "find".into(),
                });
            }
            let Ok(entry) = entry else { continue };
            // 根目录自身不算匹配结果
            if entry.path() == search_root {
                continue;
            }
            let path = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !matches!(overrides.matched(path, is_dir), ignore::Match::Whitelist(_)) {
                continue;
            }
            if results.len() >= limit {
                result_limit_reached = true;
                break;
            }
            let relative = path
                .strip_prefix(&search_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            // 目录匹配保留尾 `/`(05 文档)
            let display = if is_dir && !relative.ends_with('/') {
                format!("{relative}/")
            } else {
                relative
            };
            results.push(display);
        }

        if results.is_empty() {
            return Ok(ToolOutput::text("No files found matching pattern"));
        }
        results.sort_by_key(|a| a.to_lowercase());

        let raw = results.join("\n");
        // 结果数已被 limit 封顶,这里只剩字节限
        let truncation = truncate_head(&raw, usize::MAX, self.limits.effective_max_bytes());
        let mut output = truncation.content.clone();
        let mut notices: Vec<String> = Vec::new();
        let mut details = serde_json::Map::new();
        if result_limit_reached {
            notices.push(format!(
                "{limit} results limit reached. Use limit={} for more, or refine pattern",
                limit * 2
            ));
            details.insert("resultLimitReached".into(), json!(limit));
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
fn test_tool(cwd: PathBuf) -> FindTool {
    FindTool {
        cwd,
        limits: OutputLimits::default(),
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

    async fn exec(tool: &FindTool, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        tool.execute(
            ToolCall {
                id: "t".into(),
                name: "find".into(),
                args,
            },
            CancellationToken::new(),
            &Noop,
        )
        .await
    }

    async fn fixture() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rpi-find-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(dir.join("src/nested"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.join("node_modules/pkg"))
            .await
            .unwrap();
        for p in [
            "src/a.ts",
            "src/nested/b.spec.ts",
            "node_modules/pkg/c.ts",
            "readme.md",
        ] {
            tokio::fs::write(dir.join(p), "x").await.unwrap();
        }
        tokio::fs::write(dir.join(".gitignore"), "node_modules/\n")
            .await
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_files_by_glob_and_respects_gitignore() {
        let dir = fixture().await;
        let tool = test_tool(dir.clone());
        let output = exec(&tool, json!({"pattern": "*.ts"})).await.unwrap();
        assert!(output.output.contains("src/a.ts"));
        assert!(output.output.contains("src/nested/b.spec.ts"));
        assert!(
            !output.output.contains("node_modules"),
            "应尊重 .gitignore: {}",
            output.output
        );
        assert!(!output.output.contains("readme.md"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn anchored_pattern_and_directories_keep_trailing_slash() {
        let dir = fixture().await;
        let tool = test_tool(dir.clone());
        // 含 `/` 的 pattern 锚定到搜索根(fd --full-path 等价)
        let output = exec(&tool, json!({"pattern": "src/**/*.spec.ts"}))
            .await
            .unwrap();
        assert!(
            output.output.contains("src/nested/b.spec.ts"),
            "{}",
            output.output
        );
        assert!(!output.output.contains("src/a.ts"));
        // 目录匹配保留尾 `/`
        let output = exec(&tool, json!({"pattern": "nested"})).await.unwrap();
        assert!(output.output.contains("src/nested/"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn limit_reached_and_no_results() {
        let dir = fixture().await;
        let tool = test_tool(dir.clone());
        let output = exec(&tool, json!({"pattern": "*.ts", "limit": 1}))
            .await
            .unwrap();
        assert!(output.output.contains("1 results limit reached"));
        assert_eq!(output.details["resultLimitReached"], json!(1));
        let output = exec(&tool, json!({"pattern": "*.zig"})).await.unwrap();
        assert_eq!(output.output, "No files found matching pattern");
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn missing_path_is_error() {
        let dir = fixture().await;
        let tool = test_tool(dir.clone());
        let err = exec(&tool, json!({"pattern": "*.ts", "path": "missing"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Path not found"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}

