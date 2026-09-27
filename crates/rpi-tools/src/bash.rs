//! bash 工具(05 文档 §4):shell 执行、流式输出聚合(onUpdate 实时可见)、
//! tail 截断、超限落盘、超时/中止;非零 exit 抛错(错误 = 输出 + 状态,
//! 05 文档 appendStatus)。会话环境变量注入(T9)、commandPrefix/spawnHook
//! (T10)、进程组杀灭(T11)。
//!
//! powershell(§4 powershell.ts)复用同一 `create_shell_tool` 工厂,只换
//! `ShellToolConfig`。进程组隔离对齐 pi 的 killProcessTree:Unix 下
//! `process_group(0)` 使子进程自成组长,超时/中止时 kill(-pgid) 清整棵树
//! (含孙进程);非 Unix 保持 kill_on_drop 兜底。

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

/// 会话环境注入(T9,05 文档 PI_* 清单):执行时按需快照会话上下文
/// (PI_SESSION_ID/PI_SESSION_FILE/PI_PROVIDER/PI_MODEL/PI_REASONING_LEVEL)。
/// 返回空 = 无会话上下文(行为同未配置)。
pub type SessionEnvFn = Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>;

/// spawn 前命令改写钩子(T10,pi 的 spawnHook):检查/改写命令;返回 Err =
/// 拒绝执行(工具直接产出错误结果,不 spawn——与扩展错误语义 07 §8.5 一致)。
#[async_trait]
pub trait ShellSpawnHook: Send + Sync {
    async fn rewrite(&self, command: String) -> Result<String, String>;
}

/// shell 工具装配选项(T9/T10):会话环境、命令前缀、spawn 改写钩子。
/// 缺省全部 None = 现状行为。
#[derive(Clone, Default)]
pub struct ShellSpawnOptions {
    pub session_env: Option<SessionEnvFn>,
    pub command_prefix: Option<String>,
    pub spawn_hook: Option<Arc<dyn ShellSpawnHook>>,
}

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
    spawn: ShellSpawnOptions,
}

/// bash 工厂(默认集,05 文档 §1)。
pub fn create_bash_tool(cwd: &Path) -> Arc<dyn Tool> {
    create_shell_tool(bash_config(), cwd, ShellSpawnOptions::default())
}

/// bash 工厂 + 会话环境注入(T9,11 计划 §5)。
pub fn create_bash_tool_with_session_env(cwd: &Path, env: SessionEnvFn) -> Arc<dyn Tool> {
    create_shell_tool(
        bash_config(),
        cwd,
        ShellSpawnOptions {
            session_env: Some(env),
            ..Default::default()
        },
    )
}

/// bash 工厂 + 完整装配选项(T9/T10)。
pub fn create_bash_tool_with(cwd: &Path, spawn: ShellSpawnOptions) -> Arc<dyn Tool> {
    create_shell_tool(bash_config(), cwd, spawn)
}

/// powershell 工厂(全量集,05 文档 §4 powershell)。
pub fn create_powershell_tool(cwd: &Path) -> Arc<dyn Tool> {
    create_shell_tool(powershell_config(), cwd, ShellSpawnOptions::default())
}

/// powershell 工厂 + 完整装配选项(T9/T10,powershell 同理)。
pub fn create_powershell_tool_with(cwd: &Path, spawn: ShellSpawnOptions) -> Arc<dyn Tool> {
    create_shell_tool(powershell_config(), cwd, spawn)
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

fn create_shell_tool(
    config: ShellToolConfig,
    cwd: &Path,
    spawn: ShellSpawnOptions,
) -> Arc<dyn Tool> {
    Arc::new(ShellTool {
        config,
        cwd: cwd.to_path_buf(),
        spawn,
    })
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
    let obj = args
        .as_object()
        .ok_or_else(|| fail("arguments must be an object"))?;
    let command = obj
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| fail("missing required argument `command`"))?
        .to_string();
    let timeout_secs = match obj.get("timeout") {
        Some(v) if !v.is_null() => Some(
            v.as_u64()
                .ok_or_else(|| fail("`timeout` must be a positive integer"))?,
        ),
        _ => None,
    };
    Ok(ParsedArgs {
        command,
        timeout_secs,
    })
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
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| fail(format!("command failed: {e}")))?;
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
    tool: &ShellTool,
    command: &str,
    timeout_secs: Option<u64>,
    cancel: &CancellationToken,
    accumulator: Arc<Mutex<OutputAccumulator>>,
    updater: &dyn ToolUpdater,
) -> Result<i32, ToolError> {
    let config = &tool.config;
    let spawn_options = &tool.spawn;
    let cwd = &tool.cwd;
    let name = config.name;
    let fail = |message: String| ToolError::Failed {
        name: name.to_string(),
        message,
    };
    if timeout_secs
        .map(|t| (t as u128) * 1000 > MAX_TIMEOUT_MS)
        .unwrap_or(false)
    {
        return Err(fail("timeout exceeds maximum allowed duration".into()));
    }

    let mut cmd = tokio::process::Command::new(config.program);
    cmd.args(config.base_args.iter())
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // T9:PI_* 会话环境注入 —— 用户进程环境已有同名变量则不覆盖(05 §4)
    if let Some(env_fn) = &spawn_options.session_env {
        for (key, value) in env_fn() {
            if std::env::var_os(&key).is_none() {
                cmd.env(key, value);
            }
        }
    }
    // T11:Unix 下子进程自成进程组(process_group(0) = setpgid(0,0),
    // pgid = 子进程 pid),超时/中止时 kill(-pgid) 清整棵树(含孙进程);
    // 非 Unix 保持现状
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| fail(format!("failed to spawn shell: {e}")))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let read_out = pipe_into(stdout, &accumulator, updater, fail);
    let read_err = pipe_into(stderr, &accumulator, updater, fail);

    let timeout = Duration::from_secs(timeout_secs.unwrap_or(u64::MAX / 2));

    // 分支枚举:kill 需要在 select 结束(work 对 child 的借用终止)之后进行,
    // 故 select 连同 work future 收进内层块,块结束即释放借用
    enum Exit {
        Done(Result<i32, ToolError>),
        TimedOut,
        Cancelled,
    }
    let exit = {
        let work = async {
            // stdout/stderr 都流入 accumulator(pi 的 onData)
            let (out, err) = tokio::join!(read_out, read_err);
            out?;
            err?;
            // 超时/中止分支落选后,持有 child 的分支 future 被 drop;
            // kill_on_drop(true) 兜底(pi 进程组隔离的最后一道)
            let status = child
                .wait()
                .await
                .map_err(|e| fail(format!("command failed: {e}")))?;
            // 信号死亡无 exit code:按 shell 惯例换算 128 + signal(05 文档 §4)
            #[cfg(unix)]
            let code = status.code().unwrap_or_else(|| {
                128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0)
            });
            #[cfg(not(unix))]
            let code = status.code().unwrap_or(-1);
            Ok(code)
        };
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => Exit::Done(result),
            _ = tokio::time::sleep(timeout) => Exit::TimedOut,
            // abort → Aborted:循环据此产出 Cancelled("Operation aborted")结果,
            // 并触发 run 的 aborted 硬退出路径
            _ = cancel.cancelled() => Exit::Cancelled,
        }
    };

    match exit {
        Exit::Done(result) => result,
        Exit::TimedOut | Exit::Cancelled => {
            // T11:先杀整棵进程树(忽略 ESRCH:进程可能已死),再 wait 收尸
            kill_process_tree(&mut child);
            let _ = child.wait().await;
            match exit {
                Exit::Cancelled => Err(ToolError::Aborted {
                    name: name.to_string(),
                }),
                _ => {
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
            }
        }
    }
}

/// T11:杀子进程整棵进程树(Unix)。`process_group(0)` 已使子进程自成组长,
/// pgid = pid;kill(-pgid) 覆盖其全部后代。进程已死(ESRCH)静默忽略。
#[cfg(unix)]
fn kill_process_tree(child: &mut tokio::process::Child) {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = signal::kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_tree(child: &mut tokio::process::Child) {
    // 非 Unix 无进程组,kill_on_drop(true) 只在 drop 时杀;若子进程已挂死,
    // 下方 wait() 永不返回,超时/中止分支自身会挂死 → 先强制杀再收尸
    let _ = child.start_kill();
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
        details.insert(
            "truncation".into(),
            serde_json::to_value(&snapshot.truncation).unwrap_or_default(),
        );
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
        let ParsedArgs {
            command,
            timeout_secs,
        } = parse_args(self.config.name, &call.args)?;

        // T10:hook 先改写(检查的是用户命令),prefix 最后前置;
        // hook 返回 Err = 拒绝执行,直接产出错误结果、不 spawn(07 §8.5)
        let mut effective = command.clone();
        if let Some(hook) = &self.spawn.spawn_hook {
            match hook.rewrite(command.clone()).await {
                Ok(rewritten) => effective = rewritten,
                Err(reason) => {
                    return Err(ToolError::Failed {
                        name: self.config.name.into(),
                        message: format!("command rejected by spawn hook: {reason}"),
                    });
                }
            }
        }
        if let Some(prefix) = effective_prefix(&self.spawn.command_prefix) {
            // 换行拼接(pi bash.ts:prefix 用于 shell setup commands,可含多条语句)
            effective = format!("{prefix}\n{effective}");
        }

        updater.update(format!("$ {effective}")).await;

        let accumulator = Arc::new(Mutex::new(OutputAccumulator::new(
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES,
        )));
        let code = run(
            self,
            &effective,
            timeout_secs,
            &cancel,
            accumulator.clone(),
            updater,
        )
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

/// T10:空前缀(全空白)= 无操作。
fn effective_prefix(prefix: &Option<String>) -> Option<String> {
    prefix
        .as_ref()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
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

    async fn exec(
        tool: &ShellTool,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        tool.execute(
            ToolCall {
                id: "t".into(),
                name: tool.name().into(),
                args,
            },
            cancel,
            &Noop,
        )
        .await
    }

    fn bash_at(cwd: &Path) -> Arc<ShellTool> {
        Arc::new(ShellTool {
            config: bash_config(),
            cwd: cwd.to_path_buf(),
            spawn: ShellSpawnOptions::default(),
        })
    }

    fn bash_with(cwd: &Path, spawn: ShellSpawnOptions) -> Arc<ShellTool> {
        Arc::new(ShellTool {
            config: bash_config(),
            cwd: cwd.to_path_buf(),
            spawn,
        })
    }

    #[tokio::test]
    async fn runs_command_and_returns_output() {
        let tool = bash_at(&std::env::temp_dir());
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo hello"}),
            CancellationToken::new(),
        )
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
        let err = exec(
            &tool,
            serde_json::json!({"command": "echo boom >&2; exit 3"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("boom"), "错误应携带输出: {message}");
        assert!(message.contains("Command exited with code 3"));
    }

    #[tokio::test]
    async fn timeout_kills_command() {
        let tool = bash_at(&std::env::temp_dir());
        let err = exec(
            &tool,
            serde_json::json!({"command": "sleep 30", "timeout": 1}),
            CancellationToken::new(),
        )
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
        assert!(
            output.output.contains("Full output:"),
            "超限应提示落盘: {}",
            &output.output[..200]
        );
        assert!(output.details["fullOutputPath"].is_string());
        let path = output.details["fullOutputPath"].as_str().unwrap();
        let full = std::fs::read_to_string(path).unwrap();
        assert_eq!(full.lines().count(), 2000, "临时文件应有完整输出");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn powershell_runs_when_available() {
        // pwsh 不在(多数开发机)则跳过;工厂与 schema 由 all_tools 测试覆盖
        if std::process::Command::new("pwsh")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tool = ShellTool {
            config: powershell_config(),
            cwd: std::env::temp_dir(),
            spawn: ShellSpawnOptions::default(),
        };
        let output = exec(
            &tool,
            serde_json::json!({"command": "Write-Output hello"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output.output.trim(), "hello");
    }

    // ---- T9:PI_* 会话环境注入 ----

    #[tokio::test]
    async fn session_env_is_injected_into_command() {
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                session_env: Some(Arc::new(|| {
                    vec![("PI_RPI_TEST_MODEL".into(), "test-model".into())]
                })),
                ..Default::default()
            },
        );
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo $PI_RPI_TEST_MODEL"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output.output.trim(), "test-model");
    }

    #[tokio::test]
    async fn session_env_does_not_override_user_env() {
        // 用户进程环境已有同名变量 → 不覆盖(05 §4;变量名独占避免并行测试互扰)
        let key = "PI_RPI_TEST_KEEP";
        // 进程内临时设置;测试进程独占该变量名
        let guard = std::env::var_os(key).map(|v| v.to_string_lossy().to_string());
        std::env::set_var(key, "keep");
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                session_env: Some(Arc::new(|| vec![(key.to_string(), "injected".into())])),
                ..Default::default()
            },
        );
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo $PI_RPI_TEST_KEEP"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output.output.trim(), "keep");
        match guard {
            Some(previous) => std::env::set_var(key, previous),
            None => std::env::remove_var(key),
        }
    }

    #[tokio::test]
    async fn empty_session_env_behaves_like_default() {
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                session_env: Some(Arc::new(Vec::new)),
                ..Default::default()
            },
        );
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo ok"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output.output.trim(), "ok");
    }

    // ---- T10:commandPrefix / spawnHook ----

    #[tokio::test]
    async fn command_prefix_is_prepended() {
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                command_prefix: Some("echo wrapped;".into()),
                ..Default::default()
            },
        );
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo hello"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(output.output.contains("wrapped"), "{}", output.output);
        assert!(output.output.contains("hello"), "{}", output.output);
    }

    #[tokio::test]
    async fn blank_prefix_is_noop() {
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                command_prefix: Some("   ".into()),
                ..Default::default()
            },
        );
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo plain"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output.output.trim(), "plain");
    }

    struct RewritingHook;
    #[async_trait]
    impl ShellSpawnHook for RewritingHook {
        async fn rewrite(&self, command: String) -> Result<String, String> {
            // 记录收到的原始命令,验证 hook 检查的是用户命令而非 prefix 拼接结果
            assert_eq!(command, "echo secret");
            Ok("echo rewritten".into())
        }
    }

    struct RejectingHook;
    #[async_trait]
    impl ShellSpawnHook for RejectingHook {
        async fn rewrite(&self, _command: String) -> Result<String, String> {
            Err("forbidden by policy".into())
        }
    }

    #[tokio::test]
    async fn spawn_hook_rewrites_command_and_sees_original() {
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                spawn_hook: Some(Arc::new(RewritingHook)),
                command_prefix: Some("echo wrapped;".into()),
                ..Default::default()
            },
        );
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo secret"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(output.output.contains("rewritten"), "{}", output.output);
        assert!(
            output.output.contains("wrapped"),
            "prefix 在 hook 之后前置: {}",
            output.output
        );
        assert!(!output.output.contains("secret"), "{}", output.output);
    }

    #[tokio::test]
    async fn spawn_hook_error_rejects_without_spawning() {
        let side_effect = std::env::temp_dir().join("rpi_spawn_hook_should_not_exist");
        let _ = std::fs::remove_file(&side_effect);
        let script = format!("touch {}", side_effect.display());
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                spawn_hook: Some(Arc::new(RejectingHook)),
                ..Default::default()
            },
        );
        let err = exec(
            &tool,
            serde_json::json!({"command": script}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("forbidden by policy"), "{err}");
        assert!(!side_effect.exists(), "hook 拒绝后不得产生子进程副作用");
    }

    // ---- T11:进程组杀灭(孙进程清理) ----

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_grandchildren_in_process_group() {
        let pidfile = std::env::temp_dir().join(format!("rpi_pgid_test_{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let path = pidfile.display().to_string();
        let tool = bash_at(&std::env::temp_dir());
        // 先 fork 孙进程并写其 pid,再挂住主 shell
        let err = exec(
            &tool,
            serde_json::json!({
                "command": format!("sleep 300 & echo $! > {path}; sleep 300"),
                "timeout": 1
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");

        // SIGKILL 后稍等回收,再验证孙进程已死(kill -0 失败 = 进程不存在)
        tokio::time::sleep(Duration::from_millis(300)).await;
        let grandchild = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        let probe = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {grandchild}"))
            .status()
            .unwrap();
        assert!(!probe.success(), "孙进程 {grandchild} 应已被进程组杀灭回收");
        let _ = std::fs::remove_file(&pidfile);
    }
}
