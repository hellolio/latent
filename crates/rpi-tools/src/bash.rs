//! bash 工具(05 文档 §4):shell 执行、流式输出聚合(onUpdate 实时可见)、
//! tail 截断、超限落盘(fullOutputPath)、超时/中止;非零 exit 抛错
//! (错误 = 输出 + 状态,05 文档 appendStatus)。
//!
//! powershell(§4 powershell.ts)复用同一 `create_shell_tool` 工厂,只换
//! `ShellToolConfig`。与 pi 的差异:pi 用进程组隔离(detached + killProcessTree)
//! 杀整棵树;本实现用 tokio::process 的 kill_on_drop 直接杀子进程,进程组级
//! 清理待引入 libc/nix 后补齐(见 docs/05 踩坑记录)。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use rpi_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::output_accumulator::{OutputAccumulator, OutputSnapshot};
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

const MAX_TIMEOUT_MS: u128 = 2_147_483_647;

/// shell 工具配置(bash 与 powershell 共用工厂,05 文档 createShellToolDefinition)。
pub(crate) struct ShellToolConfig {
    pub name: &'static str,
    pub description: String,
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Vec<String>,
    /// shell 可执行文件
    pub program: &'static str,
    /// 命令之前的固定参数(如 `["-c"]`)
    pub base_args: &'static [&'static str],
}

pub struct ShellTool {
    config: ShellToolConfig,
    cwd: PathBuf,
}

/// bash 工厂(默认集,05 文档 §1)。
pub fn create_bash_tool(cwd: &Path) -> Arc<dyn Tool> {
    create_shell_tool(bash_config(), cwd)
}

/// powershell 工厂(全量集,05 文档 §4 powershell)。
pub fn create_powershell_tool(cwd: &Path) -> Arc<dyn Tool> {
    create_shell_tool(powershell_config(), cwd)
}

fn bash_config() -> ShellToolConfig {
    ShellToolConfig {
        name: "bash",
        description: "Run a shell command and return its combined stdout/stderr. Non-zero exit \
                      codes return the output plus the exit status as an error. Output is \
                      truncated to the last 2000 lines or 50KB (whichever is hit first); the \
                      full output is saved to a temp file referenced in details."
            .into(),
        prompt_snippet: Some(
            "bash(command, timeout?): runs a shell command in the working directory; output is \
             truncated (tail kept)"
                .into(),
        ),
        prompt_guidelines: vec![
            "Prefer read for inspecting files; use bash for searches, git, builds and quick \
             scripts."
                .into(),
            "Avoid interactive commands; they will hang until timeout.".into(),
        ],
        program: "sh",
        base_args: &["-c"],
    }
}

fn powershell_config() -> ShellToolConfig {
    // Windows 用系统自带 powershell;unix 需要 pwsh(pi 同构)
    #[cfg(windows)]
    let program = "powershell";
    #[cfg(not(windows))]
    let program = "pwsh";
    ShellToolConfig {
        name: "powershell",
        description: "Run a PowerShell command and return its combined stdout/stderr. Non-zero \
                      exit codes return the output plus the exit status as an error."
            .into(),
        prompt_snippet: Some(
            "powershell(command, timeout?): runs a PowerShell command in the working directory; \
             output is truncated (tail kept)"
                .into(),
        ),
        prompt_guidelines: vec![
            "Prefer read for inspecting files; use powershell for searches, git, builds and \
             quick scripts."
                .into(),
            "Avoid interactive commands; they will hang until timeout.".into(),
        ],
        program,
        base_args: &["-NoProfile", "-Command"],
    }
}

fn create_shell_tool(config: ShellToolConfig, cwd: &Path) -> Arc<dyn Tool> {
    Arc::new(ShellTool { config, cwd: cwd.to_path_buf() })
}

struct ParsedArgs {
    command: String,
    timeout_secs: Option<u64>,
}

fn parse_args(name: &str, args: &serde_json::Value) -> Result<ParsedArgs, ToolError> {
    let fail = |message: &str| ToolError::Failed {
        name: name.to_string(),
        message: message.to_string(),
    };
    let obj = args.as_object().ok_or_else(|| fail("arguments must be an object"))?;
    let command = obj
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| fail("missing required argument `command`"))?
        .to_string();
    let timeout_secs = match obj.get("timeout") {
        Some(v) if !v.is_null() => {
            Some(v.as_u64().ok_or_else(|| fail("`timeout` must be a positive integer"))?)
        }
        _ => None,
    };
    Ok(ParsedArgs { command, timeout_secs })
}

/// 流式读取一个管道到 accumulator,每块更新一次快照(让 TUI 实时可见;
/// pi 的 tool_execution_update 节流在渲染侧)。
async fn pipe_into<S: tokio::io::AsyncRead + Unpin>(
    mut stream: S,
    accumulator: &Mutex<OutputAccumulator>,
    updater: &dyn ToolUpdater,
    fail: impl Fn(String) -> ToolError + Copy,
) -> Result<(), ToolError> {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 65536];
    loop {
        let n = stream.read(&mut buf).await.map_err(|e| fail(format!("command failed: {e}")))?;
        if n == 0 {
            break;
        }
        let tail = {
            let mut acc = accumulator.lock().unwrap();
            acc.append(&buf[..n]);
            acc.tail()
        };
        updater.update(tail).await;
    }
    Ok(())
}

/// 执行 shell:stdout/stderr 流入 accumulator,超时/中止杀进程;返回 exit code。
async fn run(
    config: &ShellToolConfig,
    command: &str,
    cwd: &Path,
    timeout_secs: Option<u64>,
    cancel: &CancellationToken,
    accumulator: Arc<Mutex<OutputAccumulator>>,
    updater: &dyn ToolUpdater,
) -> Result<i32, ToolError> {
    let name = config.name;
    let fail = |message: String| ToolError::Failed { name: name.to_string(), message };
    if timeout_secs.map(|t| (t as u128) * 1000 > MAX_TIMEOUT_MS).unwrap_or(false) {
        return Err(fail("timeout exceeds maximum allowed duration".into()));
    }

    let mut child = tokio::process::Command::new(config.program)
        .args(config.base_args.iter())
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| fail(format!("failed to spawn shell: {e}")))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let read_out = pipe_into(stdout, &accumulator, updater, fail);
    let read_err = pipe_into(stderr, &accumulator, updater, fail);

    let timeout = Duration::from_secs(timeout_secs.unwrap_or(u64::MAX / 2));
    let work = async {
        // stdout/stderr 都流入 accumulator(pi 的 onData)
        let (out, err) = tokio::join!(read_out, read_err);
        out?;
        err?;
        // 超时/中止分支落选后,持有 child 的分支 future 被 drop;
        // kill_on_drop(true) 保证子进程被杀(pi 进程组隔离的简化版)
        let status = child.wait().await.map_err(|e| fail(format!("command failed: {e}")))?;
        // 信号死亡无 exit code:按 shell 惯例换算 128 + signal(05 文档 §4)
        #[cfg(unix)]
        let code = status
            .code()
            .unwrap_or_else(|| 128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0));
        #[cfg(not(unix))]
        let code = status.code().unwrap_or(-1);
        Ok(code)
    };
    tokio::pin!(work);

    tokio::select! {
        result = &mut work => result,
        _ = tokio::time::sleep(timeout) => {
            // 超时恰是长输出超限的场景:聚合已捕获输出(必要时落盘)随错误返回
            // (pi appendStatus:输出 + "Command timed out after N seconds")
            let snapshot = {
                let mut acc = accumulator.lock().unwrap();
                acc.finish();
                acc.snapshot(true)
            };
            let mut message = format!(
                "{}\nCommand timed out after {} seconds",
                snapshot.content,
                timeout_secs.unwrap_or(0)
            );
            if let Some(path) = &snapshot.full_output_path {
                message.push_str(&format!("\n[Full output: {}]", path.display()));
            }
            Err(fail(message))
        }
        // abort → Aborted:循环据此产出 Cancelled("Operation aborted")结果,
        // 并触发 run 的 aborted 硬退出路径
        _ = cancel.cancelled() => Err(ToolError::Aborted { name: name.to_string() }),
    }
}

/// 输出 + details(pi 的 BashToolDetails {truncation?, fullOutputPath?});
/// 截断时追加 `[Showing lines X-Y of N. Full output: <tmpfile>]` 提示。
fn settle_output(snapshot: OutputSnapshot) -> ToolOutput {
    let mut output = snapshot.content.clone();
    let mut details = serde_json::Map::new();
    if snapshot.truncation.truncated {
        let total = snapshot.truncation.total_lines;
        let shown = snapshot.truncation.output_lines;
        if let Some(path) = &snapshot.full_output_path {
            output.push_str(&format!(
                "\n\n[Showing lines {}-{} of {total}. Full output: {}]",
                total.saturating_sub(shown) + 1,
                total,
                path.display()
            ));
        }
        details.insert("truncation".into(), serde_json::to_value(&snapshot.truncation).unwrap_or_default());
        if let Some(path) = &snapshot.full_output_path {
            details.insert("fullOutputPath".into(), json!(path.display().to_string()));
        }
    }
    ToolOutput {
        output,
        details: if details.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(details)
        },
        terminate: false,
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        self.config.name
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {"type": "string", "description": "The shell command to execute"},
                "timeout": {"type": "integer", "description": "Optional timeout in seconds"}
            }
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        self.config.prompt_snippet.clone()
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        self.config.prompt_guidelines.clone()
    }

    async fn execute(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        let ParsedArgs { command, timeout_secs } = parse_args(self.config.name, &call.args)?;

        updater.update(format!("$ {command}")).await;

        let accumulator = Arc::new(Mutex::new(OutputAccumulator::new(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)));
        let code = run(&self.config, &command, &self.cwd, timeout_secs, &cancel, accumulator.clone(), updater)
            .await?;

        // 结束聚合并取最终快照
        let snapshot = {
            let mut acc = accumulator.lock().unwrap();
            acc.finish();
            acc.snapshot(true)
        };
        let output = settle_output(snapshot);

        if code != 0 {
            return Err(ToolError::Failed {
                name: self.config.name.into(),
                message: format!("{}\nCommand exited with code {code}", output.output),
            });
        }
        Ok(output)
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

    #[derive(Default)]
    struct Collecting(Mutex<Vec<String>>);
    #[async_trait]
    impl ToolUpdater for Collecting {
        async fn update(&self, partial: String) {
            self.0.lock().unwrap().push(partial);
        }
    }

    async fn exec(tool: &ShellTool, args: serde_json::Value, cancel: CancellationToken) -> Result<ToolOutput, ToolError> {
        tool.execute(ToolCall { id: "t".into(), name: tool.name().into(), args }, cancel, &Noop).await
    }

    fn bash_at(cwd: &Path) -> Arc<ShellTool> {
        Arc::new(ShellTool { config: bash_config(), cwd: cwd.to_path_buf() })
    }

    #[tokio::test]
    async fn runs_command_and_returns_output() {
        let tool = bash_at(&std::env::temp_dir());
        let output = exec(&tool, serde_json::json!({"command": "echo hello"}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(output.output.trim(), "hello");
    }

    #[tokio::test]
    async fn streams_updates_while_running() {
        let tool = bash_at(&std::env::temp_dir());
        let collecting = Collecting::default();
        tool.execute(
            ToolCall {
                id: "t".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "echo first; sleep 0.2; echo second"}),
            },
            CancellationToken::new(),
            &collecting,
        )
        .await
        .unwrap();
        let updates = collecting.0.lock().unwrap();
        assert!(updates.len() >= 2, "流式期间应产生多次 update: {updates:?}");
        assert!(updates.iter().any(|u| u.contains("first")));
        assert!(updates.last().unwrap().contains("second"));
    }

    #[tokio::test]
    async fn non_zero_exit_is_error_with_status() {
        let tool = bash_at(&std::env::temp_dir());
        let err = exec(&tool, serde_json::json!({"command": "echo boom >&2; exit 3"}), CancellationToken::new())
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("boom"), "错误应携带输出: {message}");
        assert!(message.contains("Command exited with code 3"));
    }

    #[tokio::test]
    async fn timeout_kills_command() {
        let tool = bash_at(&std::env::temp_dir());
        let err = exec(&tool, serde_json::json!({"command": "sleep 30", "timeout": 1}), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn abort_kills_command() {
        let tool = bash_at(&std::env::temp_dir());
        let cancel = CancellationToken::new();
        let canceller = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                cancel.cancel();
            }
        });
        let err = exec(&tool, serde_json::json!({"command": "sleep 30"}), cancel)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("aborted"));
        canceller.await.unwrap();
    }

    #[tokio::test]
    async fn huge_output_spills_to_temp_file_with_hint() {
        let tool = bash_at(&std::env::temp_dir());
        // 2000 行 x 60 字节 ≈ 120KB,超 50KB 字节限
        let output = exec(
            &tool,
            serde_json::json!({"command": "for i in $(seq 1 2000); do printf 'line %06d xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' $i; done"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(output.output.contains("Full output:"), "超限应提示落盘: {}", &output.output[..200]);
        assert!(output.details["fullOutputPath"].is_string());
        let path = output.details["fullOutputPath"].as_str().unwrap();
        let full = std::fs::read_to_string(path).unwrap();
        assert_eq!(full.lines().count(), 2000, "临时文件应有完整输出");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn powershell_runs_when_available() {
        // pwsh 不在(多数开发机)则跳过;工厂与 schema 由 all_tools 测试覆盖
        if std::process::Command::new("pwsh").arg("--version").output().is_err() {
            return;
        }
        let tool = ShellTool { config: powershell_config(), cwd: std::env::temp_dir() };
        let output = exec(&tool, serde_json::json!({"command": "Write-Output hello"}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(output.output.trim(), "hello");
    }
}
