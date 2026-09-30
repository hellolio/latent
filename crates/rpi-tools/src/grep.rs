//! grep 工具(05 文档 §7):内容搜索,尊重 .gitignore,匹配行截到 500 字符。
//!
//! pi 用外部 ripgrep 二进制;本实现用 `ignore` crate 原生遍历(同样默认
//! require_git:仅 git 仓库内应用 .gitignore),语义与 pi 一致。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use regex::RegexBuilder;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use rpi_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::truncate::{truncate_head, truncate_line, DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH};

const DEFAULT_LIMIT: usize = 100;

pub struct GrepTool {
    cwd: PathBuf,
}

/// 工厂。
pub fn create_grep_tool(cwd: &Path) -> Arc<dyn Tool> {
    Arc::new(GrepTool {
        cwd: cwd.to_path_buf(),
    })
}

struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    ignore_case: bool,
    literal: bool,
    context: usize,
    limit: usize,
    sanitize: bool,
}

fn parse_args(args: &serde_json::Value) -> Result<GrepArgs, String> {
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
    let glob = match obj.get("glob") {
        Some(v) if v.is_string() => Some(v.as_str().unwrap().to_string()),
        _ => None,
    };
    let flag = |key: &str| -> Result<bool, String> {
        match obj.get(key) {
            Some(v) if v.is_boolean() => Ok(v.as_bool().unwrap()),
            Some(v) if v.is_null() => Ok(false),
            None => Ok(false),
            _ => Err(format!("`{key}` must be a boolean")),
        }
    };
    let ignore_case = flag("ignoreCase")?;
    let literal = flag("literal")?;
    let context = match obj.get("context") {
        Some(v) if !v.is_null() => {
            v.as_u64()
                .ok_or("`context` must be a non-negative integer")? as usize
        }
        _ => 0,
    };
    let limit = match obj.get("limit") {
        Some(v) if !v.is_null() => {
            let n = v.as_u64().ok_or("`limit` must be a positive integer")? as usize;
            n.max(1)
        }
        _ => DEFAULT_LIMIT,
    };
    Ok(GrepArgs {
        pattern,
        path,
        glob,
        ignore_case,
        literal,
        context,
        limit,
        sanitize: crate::sanitize::parse_sanitize_arg(args),
    })
}

/// 相对搜索根的显示路径;非目录搜索时用文件名(pi 的 formatPath)。
fn format_display_path(entry: &Path, search_root: &Path, search_root_is_dir: bool) -> String {
    if search_root_is_dir {
        if let Ok(relative) = entry.strip_prefix(search_root) {
            return relative.to_string_lossy().replace('\\', "/");
        }
    }
    entry
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| entry.to_string_lossy().to_string())
}

/// 构建 gitignore 感知 + glob 过滤的遍历器;搜索根不存在时返回 Err。
fn build_walker(search_root: &Path, glob: Option<&str>) -> Result<ignore::Walk, String> {
    let metadata = std::fs::metadata(search_root)
        .map_err(|_| format!("Path not found: {}", search_root.display()))?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(format!("Path not found: {}", search_root.display()));
    }
    let mut walker = ignore::WalkBuilder::new(search_root);
    // pi 传 --hidden:包含隐藏文件;.git 目录始终跳过。仓库外也应用 .gitignore
    // (require_git(false),与 find 的 fd --no-require-git 行为一致)
    walker
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    if metadata.is_file() {
        return Ok(walker.build());
    }
    if let Some(glob) = glob {
        let mut overrides = ignore::overrides::OverrideBuilder::new(search_root);
        overrides
            .add(glob)
            .map_err(|e| format!("invalid glob `{glob}`: {e}"))?;
        let overrides = overrides
            .build()
            .map_err(|e| format!("invalid glob `{glob}`: {e}"))?;
        walker.overrides(overrides);
    }
    Ok(walker.build())
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents for a pattern. Returns matching lines with file paths and line \
         numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is \
         hit first). Long lines are truncated to 500 chars."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": {"type": "string", "description": "Search pattern (regex or literal string)"},
                "path": {"type": "string", "description": "Directory or file to search (default: current directory)"},
                "glob": {"type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"},
                "ignoreCase": {"type": "boolean", "description": "Case-insensitive search (default: false)"},
                "literal": {"type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)"},
                "context": {"type": "integer", "description": "Number of lines to show before and after each match (default: 0)"},
                "limit": {"type": "integer", "description": "Maximum number of matches to return (default: 100)"},
                "sanitize": {"type": "boolean", "description": "Set false to keep raw output including ANSI escape codes and control characters. Defaults to true."}
            }
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some("Search file contents for patterns (respects .gitignore)".into())
    }

    async fn execute(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let args = parse_args(&call.args).map_err(|message| ToolError::Failed {
            name: "grep".into(),
            message,
        })?;

        let search_root = match &args.path {
            Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.cwd.join(p),
            None => self.cwd.clone(),
        };
        let search_root_is_dir = std::fs::metadata(&search_root)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        let walker = build_walker(&search_root, args.glob.as_deref()).map_err(|message| {
            ToolError::Failed {
                name: "grep".into(),
                message,
            }
        })?;

        let pattern = if args.literal {
            regex::escape(&args.pattern)
        } else {
            args.pattern.clone()
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(args.ignore_case)
            .build()
            .map_err(|e| ToolError::Failed {
                name: "grep".into(),
                message: format!("invalid pattern `{}`: {e}", args.pattern),
            })?;

        let mut match_count = 0usize;
        let mut match_limit_reached: Option<usize> = None;
        let mut lines_truncated = false;
        let mut output_lines: Vec<String> = Vec::new();

        'outer: for entry in walker {
            if cancel.is_cancelled() {
                return Err(ToolError::Aborted {
                    name: "grep".into(),
                });
            }
            let Ok(entry) = entry else { continue };
            // rg 只搜文件
            if entry.file_type().map(|t| !t.is_file()).unwrap_or(true) {
                continue;
            }
            let path = entry.into_path();
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            // 二进制文件跳过(rg 默认行为)
            if bytes.contains(&0) {
                continue;
            }
            let content = String::from_utf8_lossy(&bytes);
            let display = format_display_path(&path, &search_root, search_root_is_dir);
            let lines: Vec<&str> = content.lines().collect();

            for (index, line) in lines.iter().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                match_count += 1;
                if match_count > args.limit {
                    break 'outer;
                }
                let line_number = index + 1;
                if args.context == 0 {
                    let (text, was_truncated) =
                        truncate_line(line.trim_end_matches('\r'), GREP_MAX_LINE_LENGTH);
                    lines_truncated |= was_truncated;
                    let text = if args.sanitize {
                        crate::sanitize::sanitize_output(&text)
                    } else {
                        text
                    };
                    output_lines.push(format!("{display}:{line_number}: {text}"));
                } else {
                    let start = line_number.saturating_sub(args.context).max(1);
                    let end = (line_number + args.context).min(lines.len());
                    for current in start..=end {
                        let text = lines[current - 1].replace('\r', "");
                        let (text, was_truncated) = truncate_line(&text, GREP_MAX_LINE_LENGTH);
                        lines_truncated |= was_truncated;
                        let text = if args.sanitize {
                            crate::sanitize::sanitize_output(&text)
                        } else {
                            text
                        };
                        if current == line_number {
                            output_lines.push(format!("{display}:{current}: {text}"));
                        } else {
                            output_lines.push(format!("{display}-{current}- {text}"));
                        }
                    }
                }
                // pi 语义:恰好达到 limit 也视为触顶并停止
                if match_count >= args.limit {
                    match_limit_reached = Some(args.limit);
                    break 'outer;
                }
            }
        }

        if output_lines.is_empty() && match_limit_reached.is_none() {
            return Ok(ToolOutput::text("No matches found"));
        }

        // 匹配数已被 limit 封顶,这里只剩字节限(pi 的 truncateHead 无行数限)
        let raw = output_lines.join("\n");
        let truncation = truncate_head(&raw, usize::MAX, DEFAULT_MAX_BYTES);
        let mut output = truncation.content.clone();
        let mut notices: Vec<String> = Vec::new();
        let mut details = serde_json::Map::new();
        if let Some(limit) = match_limit_reached {
            notices.push(format!(
                "{limit} matches limit reached. Use limit={} for more, or refine pattern",
                limit * 2
            ));
            details.insert("matchLimitReached".into(), json!(limit));
        }
        if truncation.truncated {
            notices.push(format!("{}KB limit reached", DEFAULT_MAX_BYTES / 1024));
            details.insert(
                "truncation".into(),
                serde_json::to_value(&truncation).unwrap_or_default(),
            );
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
            details.insert("linesTruncated".into(), json!(true));
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
mod tests {
    use super::*;

    struct Noop;
    #[async_trait]
    impl ToolUpdater for Noop {
        async fn update(&self, _partial: String) {}
    }

    async fn exec(tool: &GrepTool, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        tool.execute(
            ToolCall {
                id: "t".into(),
                name: "grep".into(),
                args,
            },
            CancellationToken::new(),
            &Noop,
        )
        .await
    }

    async fn fixture() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rpi-grep-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(dir.join("src")).await.unwrap();
        tokio::fs::write(dir.join("src/a.ts"), "let alpha = 1;\nlet beta = 2;\n")
            .await
            .unwrap();
        tokio::fs::write(dir.join("b.txt"), "alpha here\n")
            .await
            .unwrap();
        // node_modules 内的匹配应被 .gitignore 排除
        tokio::fs::create_dir_all(dir.join("node_modules"))
            .await
            .unwrap();
        tokio::fs::write(dir.join("node_modules/c.ts"), "alpha in deps\n")
            .await
            .unwrap();
        tokio::fs::write(dir.join(".gitignore"), "node_modules/\n")
            .await
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_matches_with_paths_and_respects_gitignore() {
        let dir = fixture().await;
        let tool = GrepTool { cwd: dir.clone() };
        let output = exec(&tool, json!({"pattern": "alpha"})).await.unwrap();
        assert!(
            output.output.contains("src/a.ts:1: let alpha = 1;"),
            "{}",
            output.output
        );
        assert!(output.output.contains("b.txt:1: alpha here"));
        assert!(
            !output.output.contains("node_modules"),
            "应尊重 .gitignore: {}",
            output.output
        );
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn glob_filter_and_literal_and_case() {
        let dir = fixture().await;
        let tool = GrepTool { cwd: dir.clone() };
        // glob 过滤:只搜 .ts
        let output = exec(&tool, json!({"pattern": "alpha", "glob": "*.ts"}))
            .await
            .unwrap();
        assert!(output.output.contains("src/a.ts"));
        assert!(!output.output.contains("b.txt"));
        // literal:正则元字符按字面
        tokio::fs::write(dir.join("src/d.ts"), "a.b\naxb\n")
            .await
            .unwrap();
        let output = exec(&tool, json!({"pattern": "a.b", "literal": true}))
            .await
            .unwrap();
        assert!(output.output.contains("a.b"));
        assert!(!output.output.contains("axb"));
        // ignoreCase
        let output = exec(&tool, json!({"pattern": "ALPHA", "ignoreCase": true}))
            .await
            .unwrap();
        assert!(output.output.contains("src/a.ts"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn context_lines_and_match_limit() {
        let dir = fixture().await;
        let tool = GrepTool { cwd: dir.clone() };
        let output = exec(&tool, json!({"pattern": "beta", "context": 1}))
            .await
            .unwrap();
        assert!(output.output.contains("src/a.ts-1- let alpha = 1;"));
        assert!(output.output.contains("src/a.ts:2: let beta = 2;"));
        // limit 触发 notice + details
        let output = exec(&tool, json!({"pattern": "let", "limit": 2}))
            .await
            .unwrap();
        assert!(output.output.contains("2 matches limit reached"));
        assert_eq!(output.details["matchLimitReached"], json!(2));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn long_line_truncated_and_no_match_message() {
        let dir = fixture().await;
        let tool = GrepTool { cwd: dir.clone() };
        let long = format!("hit {}", "x".repeat(2000));
        tokio::fs::write(dir.join("long.txt"), long).await.unwrap();
        let output = exec(&tool, json!({"pattern": "hit"})).await.unwrap();
        assert!(output.output.contains("Some lines truncated to 500 chars"));
        assert_eq!(output.details["linesTruncated"], json!(true));
        let output = exec(&tool, json!({"pattern": "nope-nowhere"}))
            .await
            .unwrap();
        assert_eq!(output.output, "No matches found");
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn missing_path_is_error_and_single_file_search() {
        let dir = fixture().await;
        let tool = GrepTool { cwd: dir.clone() };
        let err = exec(&tool, json!({"pattern": "x", "path": "missing-dir"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Path not found"));
        let output = exec(&tool, json!({"pattern": "alpha", "path": "b.txt"}))
            .await
            .unwrap();
        assert!(output.output.starts_with("b.txt:1:"));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    // ---- sanitize:默认剥离匹配行中的 ANSI 码,sanitize:false 保留 ----
    #[tokio::test]
    async fn sanitizes_ansi_in_matched_lines_by_default() {
        let dir = fixture().await;
        tokio::fs::write(dir.join("c.txt"), "\u{1b}[32malpha\u{1b}[0m here\n")
            .await
            .unwrap();
        let tool = GrepTool { cwd: dir.clone() };
        let output = exec(&tool, json!({"pattern": "alpha", "path": "c.txt"}))
            .await
            .unwrap();
        assert!(
            !output.output.contains('\u{1b}'),
            "默认应剥离 ANSI 码: {:?}",
            output.output
        );
        assert!(output.output.contains("alpha here"));

        let output = exec(
            &tool,
            json!({"pattern": "alpha", "path": "c.txt", "sanitize": false}),
        )
        .await
        .unwrap();
        assert!(
            output.output.contains('\u{1b}'),
            "sanitize:false 应保留 ANSI 码: {:?}",
            output.output
        );
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
