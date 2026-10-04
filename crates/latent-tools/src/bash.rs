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

use latent_agent::{Tool, ToolCall, ToolError, ToolOutput, ToolUpdater};

use crate::output_accumulator::{OutputAccumulator, OutputSnapshot};
use crate::truncate::{OutputLimits, DEFAULT_MAX_LINES};

const MAX_TIMEOUT_MS: u128 = 2_147_483_647;

/// 会话环境注入(T9,05 文档 LATENT_* 清单):执行时按需快照会话上下文
/// (LATENT_SESSION_ID/LATENT_SESSION_FILE/LATENT_PROVIDER/LATENT_MODEL/LATENT_REASONING_LEVEL)。
/// 返回空 = 无会话上下文(行为同未配置)。
pub type SessionEnvFn = Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>;

/// 后台任务完成通知接缝:工具侧只产出通知文本,投递方式(follow_up 唤醒
/// 模型等)由装配层注入。None = 不武装自动转后台。
#[async_trait]
pub trait BackgroundNotifier: Send + Sync {
    async fn notify(&self, text: String);
}

/// shell 运行时限策略:模型未传 `timeout` 时的默认超时,以及运行超过阈值
/// 自动转后台的秒数。转后台只在生效超时大于阈值时武装:默认 120s 超时
/// 大于 60s 阈值,不带 timeout 的长命令也会在 60s 转后台(完成时经
/// notifier 通知);显式传更短 timeout 的命令按超时杀灭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellTimeoutPolicy {
    pub default_timeout_secs: u64,
    pub background_after_secs: u64,
}

impl Default for ShellTimeoutPolicy {
    fn default() -> Self {
        ShellTimeoutPolicy {
            default_timeout_secs: 120,
            background_after_secs: 60,
        }
    }
}

/// spawn 前命令改写钩子(T10,pi 的 spawnHook):检查/改写命令;返回 Err =
/// 拒绝执行(工具直接产出错误结果,不 spawn——与扩展错误语义 07 §8.5 一致)。
#[async_trait]
pub trait ShellSpawnHook: Send + Sync {
    async fn rewrite(&self, command: String) -> Result<String, String>;
}

/// shell 工具装配选项(T9/T10):会话环境、命令前缀、spawn 改写钩子、
/// 运行时限策略、后台完成通知。缺省 = 默认时限、不武装后台化。
#[derive(Clone, Default)]
pub struct ShellSpawnOptions {
    pub session_env: Option<SessionEnvFn>,
    pub command_prefix: Option<String>,
    pub spawn_hook: Option<Arc<dyn ShellSpawnHook>>,
    pub timeouts: ShellTimeoutPolicy,
    pub background_notifier: Option<Arc<dyn BackgroundNotifier>>,
}

/// shell 工具配置(bash 与 powershell 共用工厂,05 文档 createShellToolDefinition)。
pub(crate) struct ShellToolConfig {
    pub name: &'static str,
    pub description: String,
    pub prompt_snippet: Option<String>,
    /// shell 可执行文件
    pub program: &'static str,
    /// 命令之前的固定参数(如 `["-c"]`)
    pub base_args: &'static [&'static str],
    /// 输出自我约束上限(与 agent 转录裁剪同源派生,truncate 模块文档)
    pub limits: OutputLimits,
}

pub struct ShellTool {
    config: ShellToolConfig,
    cwd: PathBuf,
    spawn: ShellSpawnOptions,
    /// 后台任务序号(转后台结果里的 task id 用,实例内单调)
    background_counter: std::sync::atomic::AtomicU64,
}

/// bash 工厂(默认集,05 文档 §1)。
pub fn create_bash_tool(cwd: &Path) -> Arc<dyn Tool> {
    create_shell_tool(
        bash_config(OutputLimits::default(), &ShellTimeoutPolicy::default()),
        cwd,
        ShellSpawnOptions::default(),
    )
}

/// bash 工厂 + 会话环境注入(T9,11 计划 §5)。
pub fn create_bash_tool_with_session_env(cwd: &Path, env: SessionEnvFn) -> Arc<dyn Tool> {
    create_shell_tool(
        bash_config(OutputLimits::default(), &ShellTimeoutPolicy::default()),
        cwd,
        ShellSpawnOptions {
            session_env: Some(env),
            ..Default::default()
        },
    )
}

/// bash 工厂 + 完整装配选项(T9/T10)。
pub fn create_bash_tool_with(cwd: &Path, spawn: ShellSpawnOptions) -> Arc<dyn Tool> {
    create_shell_tool(bash_config(OutputLimits::default(), &spawn.timeouts), cwd, spawn)
}

/// bash 工厂 + 完整装配选项 + 输出上限注入(装配层统一派生值)。
pub fn create_bash_tool_with_limits(
    cwd: &Path,
    spawn: ShellSpawnOptions,
    limits: OutputLimits,
) -> Arc<dyn Tool> {
    create_shell_tool(bash_config(limits, &spawn.timeouts), cwd, spawn)
}

/// powershell 工厂(全量集,05 文档 §4 powershell)。
pub fn create_powershell_tool(cwd: &Path) -> Arc<dyn Tool> {
    create_shell_tool(
        powershell_config(OutputLimits::default()),
        cwd,
        ShellSpawnOptions::default(),
    )
}

/// powershell 工厂 + 完整装配选项(T9/T10,powershell 同理)。
pub fn create_powershell_tool_with(cwd: &Path, spawn: ShellSpawnOptions) -> Arc<dyn Tool> {
    create_shell_tool(powershell_config(OutputLimits::default()), cwd, spawn)
}

fn bash_config(limits: OutputLimits, timeouts: &ShellTimeoutPolicy) -> ShellToolConfig {
    ShellToolConfig {
        name: "bash",
        description: format!(
            "Run a shell command and return its combined stdout/stderr. Non-zero exit \
             codes return the output plus the exit status as an error. Output is \
             truncated to the last {DEFAULT_MAX_LINES} lines or {} bytes (whichever is hit \
             first); the full output is saved to a temp file referenced in details. \
             A command still running after {}s is moved to the background: the call \
             returns immediately with a task id and an output file, and completion \
             (exit code and output tail) is reported automatically. Pass a shorter \
             timeout to kill a command sooner. Batch independent commands into one \
             call with `;` or `&&`, or issue several calls in a single turn.",
            limits.effective_max_bytes(),
            timeouts.background_after_secs
        ),
        prompt_snippet: Some(
            "bash(command, timeout?): runs a shell command in the working directory; output is \
             truncated (tail kept); long-running commands are backgrounded and reported on \
             completion"
                .into(),
        ),
        program: "sh",
        base_args: &["-c"],
        limits,
    }
}

fn powershell_config(limits: OutputLimits) -> ShellToolConfig {
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
        program,
        base_args: &["-NoProfile", "-Command"],
        limits,
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
        background_counter: std::sync::atomic::AtomicU64::new(0),
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

/// 管道读取任务(owned:'static,后台化后继续存活的独立任务):字节流入
/// accumulator,tail 经通道转发给 execute 作用域内的 relay(后台化后 relay
/// 已随 execute 结束,通道关闭,此处静默丢弃)。
async fn pipe_task<S: tokio::io::AsyncRead + Unpin>(
    mut stream: S,
    accumulator: Arc<Mutex<OutputAccumulator>>,
    tails: tokio::sync::mpsc::UnboundedSender<String>,
    fail: impl Fn(String) -> ToolError + Send + 'static,
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
        let _ = tails.send(tail);
    }
    Ok(())
}

/// 信号死亡无 exit code:按 shell 惯例换算 128 + signal(05 文档 §4)。
fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        status.code().unwrap_or_else(|| {
            128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0)
        })
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

/// 后台 watcher:持有 child 与管道任务,等进程退出后排空输出,经 notifier
/// 上报完成(exit code + 输出尾段)。latent 进程退出时 watcher 被 drop,
/// kill_on_drop(true) 兜底杀灭整棵进程树。
async fn watch_background(
    task_id: String,
    mut child: tokio::process::Child,
    pipe_out: tokio::task::JoinHandle<Result<(), ToolError>>,
    pipe_err: tokio::task::JoinHandle<Result<(), ToolError>>,
    accumulator: Arc<Mutex<OutputAccumulator>>,
    started: std::time::Instant,
    notifier: Arc<dyn BackgroundNotifier>,
) {
    let code = match child.wait().await {
        Ok(status) => Ok(exit_code(status)),
        Err(e) => Err(e.to_string()),
    };
    let _ = pipe_out.await;
    let _ = pipe_err.await;
    let snapshot = {
        let mut acc = accumulator.lock().unwrap();
        acc.finish();
        acc.snapshot(true)
    };
    let elapsed = started.elapsed().as_secs();
    let mut message = match code {
        Ok(0) => format!("[latent] background task {task_id} finished successfully (elapsed {elapsed}s)."),
        Ok(code) => {
            format!("[latent] background task {task_id} failed: exit code {code} (elapsed {elapsed}s).")
        }
        Err(error) => format!("[latent] background task {task_id} could not be reaped: {error}"),
    };
    let tail = crate::truncate::truncate_tail(&snapshot.content, 40, 2000).content;
    if !tail.trim().is_empty() {
        message.push_str(&format!("\nOutput tail:\n{tail}"));
    }
    if let Some(path) = &snapshot.full_output_path {
        message.push_str(&format!("\n[Full output: {}]", path.display()));
    }
    notifier.notify(message).await;
}

/// run 的成功产物:正常退出码,或"已转后台"的立即结算结果(execute 据此
/// 短路返回,不再走 exit code 结算路径)。
enum RunOutcome {
    Code(i32),
    Background(ToolOutput),
}

/// 执行 shell:stdout/stderr 经常驻管道任务流入 accumulator(超时/中止杀
/// 进程;生效超时大于后台阈值时,运行超阈值自动转后台 —— 进程继续跑,
/// tool result 立即结算,完成经 notifier 上报)。
async fn run(
    tool: &ShellTool,
    command: &str,
    timeout_secs: Option<u64>,
    cancel: &CancellationToken,
    accumulator: Arc<Mutex<OutputAccumulator>>,
    updater: &dyn ToolUpdater,
) -> Result<RunOutcome, ToolError> {
    let config = &tool.config;
    let spawn_options = &tool.spawn;
    let cwd = &tool.cwd;
    let name = config.name;
    let fail = move |message: String| ToolError::Failed {
        name: name.to_string(),
        message,
    };
    let timeouts = spawn_options.timeouts;
    let effective_timeout_secs = timeout_secs.unwrap_or(timeouts.default_timeout_secs);
    if (effective_timeout_secs as u128) * 1000 > MAX_TIMEOUT_MS {
        return Err(fail("timeout exceeds maximum allowed duration".into()));
    }
    // 转后台只在生效超时严格大于阈值时武装:阈值先到 = 超时杀灭的语义保持
    // (默认 120s 超时 > 60s 阈值 = 不带 timeout 的长命令也会在 60s 转后台)
    let background_armed = spawn_options.background_notifier.is_some()
        && timeouts.background_after_secs > 0
        && timeouts.background_after_secs < effective_timeout_secs;

    let mut cmd = tokio::process::Command::new(config.program);
    cmd.args(config.base_args.iter())
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // T9:LATENT_* 会话环境注入 —— 用户进程环境已有同名变量则不覆盖(05 §4)
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

    // 管道任务常驻('static):Done 路径由 relay 排空;后台化后继续独立
    // 运行,输出持续落 accumulator/临时文件。tail 经通道转发,TUI 实时可见
    let (tail_tx, tail_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let pipe_out = tokio::spawn(pipe_task(
        stdout,
        accumulator.clone(),
        tail_tx.clone(),
        fail,
    ));
    let pipe_err = tokio::spawn(pipe_task(stderr, accumulator.clone(), tail_tx, fail));

    let timeout = Duration::from_secs(effective_timeout_secs);
    let started = std::time::Instant::now();

    // 分支枚举:kill/移交需要 child 的借用(work future)结束后进行,
    // 故 select 连同 work future 收进内层块,块结束即释放借用
    enum Exit {
        Done(Result<i32, ToolError>),
        TimedOut,
        Background,
        Cancelled,
    }
    let exit = {
        // relay:tail → updater(TUI 实时可见);后台化/中止时随本 future
        // 一起被 drop,转发停止,管道任务静默继续
        let relay = async {
            let mut tail_rx = tail_rx;
            while let Some(tail) = tail_rx.recv().await {
                updater.update(tail).await;
            }
        };
        tokio::pin!(relay);
        let work = async {
            // 超时/中止/后台分支落选后,持有 child 的分支 future 被 drop;
            // kill_on_drop(true) 兜底(pi 进程组隔离的最后一道)
            let status = child
                .wait()
                .await
                .map_err(|e| fail(format!("command failed: {e}")))?;
            Ok::<i32, ToolError>(exit_code(status))
        };
        tokio::pin!(work);
        tokio::select! {
            // join!:relay 与 work 并行推进,work 决定返回值;relay 先结束
            //(命令提前关闭输出)不影响继续等 work
            ((), result) = async { tokio::join!(&mut relay, &mut work) } => {
                Exit::Done(result)
            }
            _ = tokio::time::sleep(timeout) => Exit::TimedOut,
            _ = tokio::time::sleep(Duration::from_secs(timeouts.background_after_secs)),
                if background_armed => Exit::Background,
            // abort → Aborted:循环据此产出 Cancelled("Operation aborted")结果,
            // 并触发 run 的 aborted 硬退出路径
            _ = cancel.cancelled() => Exit::Cancelled,
        }
    };

    match exit {
        Exit::Done(result) => {
            // work 完成后 relay 已排空通道(管道任务 EOF),收它们的错误
            for handle in [pipe_out, pipe_err] {
                match handle.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(join_error) => return Err(fail(format!("output pipe failed: {join_error}"))),
                }
            }
            result.map(RunOutcome::Code)
        }
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
                        snapshot.content, effective_timeout_secs
                    );
                    if let Some(path) = &snapshot.full_output_path {
                        message.push_str(&format!("\n[Full output: {}]", path.display()));
                    }
                    Err(fail(message))
                }
            }
        }
        Exit::Background => {
            // 移交 watcher:进程继续跑,tool result 立即结算(模型不盲等);
            // 强制全量输出落盘,模型可随时读输出文件查进度
            let task_number = tool
                .background_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let task_id = format!("{name}-{task_number}");
            let full_path = {
                let mut acc = accumulator.lock().unwrap();
                acc.force_temp_file()
            };
            let snapshot = {
                let mut acc = accumulator.lock().unwrap();
                acc.snapshot(false)
            };
            let notifier = spawn_options
                .background_notifier
                .clone()
                .expect("background branch only runs when a notifier is armed");
            tokio::spawn(watch_background(
                task_id.clone(),
                child,
                pipe_out,
                pipe_err,
                accumulator,
                started,
                notifier,
            ));
            let mut message = format!(
                "Command still running in background (task {task_id}). It will be reported \
                 here when it finishes; you may continue with other work in the meantime."
            );
            if let Some(path) = &full_path {
                message.push_str(&format!("\nFull output is being written to: {}", path.display()));
            }
            if !snapshot.content.trim().is_empty() {
                message.push_str(&format!("\n\nOutput so far:\n{}", snapshot.content));
            }
            let mut details = serde_json::Map::new();
            if let Some(path) = &full_path {
                details.insert(
                    "fullOutputPath".into(),
                    json!(path.display().to_string()),
                );
            }
            Ok(RunOutcome::Background(ToolOutput {
                output: message,
                details: if details.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Object(details)
                },
                terminate: false,
            }))
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
/// 工具结果字节级保真,不做 ANSI/控制字符净化(对齐 pi:净化只在 `!`
/// 裸命令路径);截断管体积,不管内容。
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
        let timeout_description = format!(
            "Optional timeout in seconds. Omit for the default ({}s). Commands running \
             past {}s are moved to the background and reported on completion.",
            self.spawn.timeouts.default_timeout_secs,
            self.spawn.timeouts.background_after_secs
        );
        json!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {"type": "string", "description": "The shell command to execute"},
                "timeout": {"type": "integer", "description": timeout_description}
            }
        })
    }

    fn prompt_snippet(&self) -> Option<String> {
        self.config.prompt_snippet.clone()
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
        let mut sandboxed = false;
        if let Some(hook) = &self.spawn.spawn_hook {
            match hook.rewrite(command.clone()).await {
                Ok(rewritten) => {
                    sandboxed = is_sandbox_wrapper(&rewritten);
                    effective = rewritten;
                }
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
            self.config.limits.max_lines,
            self.config.limits.effective_max_bytes(),
        )));
        let outcome = run(
            self,
            &effective,
            timeout_secs,
            &cancel,
            accumulator.clone(),
            updater,
        )
        .await?;

        // 已转后台:结果在转后台结算点已备好,直接返回(进程由 watcher 托管)
        let code = match outcome {
            RunOutcome::Background(output) => return Ok(output),
            RunOutcome::Code(code) => code,
        };

        // 结束聚合并取最终快照
        let snapshot = {
            let mut acc = accumulator.lock().unwrap();
            acc.finish();
            acc.snapshot(true)
        };
        let output = settle_output(snapshot);

        if code != 0 {
            let mut message = format!("{}\nCommand exited with code {code}", output.output);
            // 沙箱拦截的事后提示:EPERM(seatbelt/landlock/seccomp 统一表现)
            // 常来自内核拒绝而非命令本身;概率性判断,只补信息不改判定
            if sandboxed && output.output.contains("Operation not permitted") {
                message.push_str(SANDBOX_DENIAL_NOTICE);
            }
            return Err(ToolError::Failed {
                name: self.config.name.into(),
                message,
            });
        }
        Ok(output)
    }
}

const SANDBOX_DENIAL_NOTICE: &str = "\n[latent] sandbox notice: this command ran sandboxed \
(plan mode:read-only, no network); the failure above is likely caused by the sandbox, not the command.";

/// 改写后的命令是否被沙箱后端包装(seatbelt/bwrap/landlock helper)。
fn is_sandbox_wrapper(rewritten: &str) -> bool {
    rewritten.contains("sandbox-exec")
        || rewritten.contains("bwrap")
        || rewritten.contains("--latent-landlock-helper")
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
            config: bash_config(OutputLimits::default(), &ShellTimeoutPolicy::default()),
            cwd: cwd.to_path_buf(),
            spawn: ShellSpawnOptions::default(),
            background_counter: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn bash_with(cwd: &Path, spawn: ShellSpawnOptions) -> Arc<ShellTool> {
        Arc::new(ShellTool {
            config: bash_config(OutputLimits::default(), &spawn.timeouts),
            cwd: cwd.to_path_buf(),
            spawn,
            background_counter: std::sync::atomic::AtomicU64::new(0),
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

    // ---- 字节级保真:工具结果不做 ANSI 净化(对齐 pi,净化只在 ! 路径) ----
    #[tokio::test]
    async fn preserves_ansi_output_raw() {
        let tool = bash_at(&std::env::temp_dir());
        let output = exec(
            &tool,
            serde_json::json!({
                "command": "printf '\\033[32mgreen\\033[0m plain\\n'"
            }),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            output.output.contains("\u{1b}[32m"),
            "工具结果应保留 ANSI 码原文: {:?}",
            output.output
        );
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
            config: powershell_config(OutputLimits::default()),
            cwd: std::env::temp_dir(),
            spawn: ShellSpawnOptions::default(),
            background_counter: std::sync::atomic::AtomicU64::new(0),
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

    // ---- T9:LATENT_* 会话环境注入 ----

    #[tokio::test]
    async fn session_env_is_injected_into_command() {
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                session_env: Some(Arc::new(|| {
                    vec![("LATENT_TEST_MODEL".into(), "test-model".into())]
                })),
                ..Default::default()
            },
        );
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo $LATENT_TEST_MODEL"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output.output.trim(), "test-model");
    }

    #[tokio::test]
    async fn session_env_does_not_override_user_env() {
        // 用户进程环境已有同名变量 → 不覆盖(05 §4;变量名独占避免并行测试互扰)
        let key = "LATENT_TEST_KEEP";
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
            serde_json::json!({"command": "echo $LATENT_TEST_KEEP"}),
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
        let side_effect = std::env::temp_dir().join("latent_spawn_hook_should_not_exist");
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

    struct StaticRewriteHook(&'static str);
    #[async_trait]
    impl ShellSpawnHook for StaticRewriteHook {
        async fn rewrite(&self, _command: String) -> Result<String, String> {
            Ok(self.0.into())
        }
    }

    // ---- 沙箱拦截的事后提示 ----

    #[tokio::test]
    async fn sandbox_denial_appends_notice() {
        // 改写后的命令含沙箱包装特征 + 输出含 EPERM 文案 → 追加提示
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                spawn_hook: Some(Arc::new(StaticRewriteHook(
                    "echo sandbox-exec; echo Operation not permitted; exit 7",
                ))),
                ..Default::default()
            },
        );
        let err = exec(
            &tool,
            serde_json::json!({"command": "anything"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("sandbox notice"), "{err}");
    }

    #[tokio::test]
    async fn non_sandbox_eperm_and_clean_failure_get_no_notice() {
        // 非沙箱命令输出 EPERM 文案 → 不追加提示
        let plain = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                spawn_hook: Some(Arc::new(StaticRewriteHook(
                    "echo Operation not permitted; exit 7",
                ))),
                ..Default::default()
            },
        );
        let err = exec(
            &plain,
            serde_json::json!({"command": "anything"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(!err.to_string().contains("sandbox notice"), "{err}");

        // 沙箱命令但失败与 EPERM 无关 → 不追加提示
        let sandboxed = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                spawn_hook: Some(Arc::new(StaticRewriteHook(
                    "echo sandbox-exec; exit 3",
                ))),
                ..Default::default()
            },
        );
        let err = exec(
            &sandboxed,
            serde_json::json!({"command": "anything"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(!err.to_string().contains("sandbox notice"), "{err}");
    }

    // ---- T11:进程组杀灭(孙进程清理) ----

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_grandchildren_in_process_group() {
        let pidfile = std::env::temp_dir().join(format!("latent_pgid_test_{}", std::process::id()));
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

    // ---- P2:默认超时(模型未传 timeout 参数时生效,settings 可配) ----

    fn shell_with_policy(cwd: &Path, policy: ShellTimeoutPolicy) -> Arc<ShellTool> {
        Arc::new(ShellTool {
            config: bash_config(OutputLimits::default(), &policy),
            cwd: cwd.to_path_buf(),
            // 运行时策略经 spawn 选项注入(config 只承载描述文案)
            spawn: ShellSpawnOptions {
                timeouts: policy,
                ..Default::default()
            },
            background_counter: std::sync::atomic::AtomicU64::new(0),
        })
    }

    #[tokio::test]
    async fn default_timeout_kills_when_param_omitted() {
        let tool = shell_with_policy(
            &std::env::temp_dir(),
            ShellTimeoutPolicy {
                default_timeout_secs: 1,
                background_after_secs: 180,
            },
        );
        let err = exec(
            &tool,
            serde_json::json!({"command": "sleep 30"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("timed out after 1 seconds"),
            "默认超时应生效并写明秒数: {message}"
        );
    }

    // ---- P3:超阈值自动转后台 + 完成通知 ----

    #[derive(Default)]
    struct CollectingNotifier(Mutex<Vec<String>>);
    #[async_trait]
    impl BackgroundNotifier for CollectingNotifier {
        async fn notify(&self, text: String) {
            self.0.lock().unwrap().push(text);
        }
    }

    #[tokio::test]
    async fn long_running_command_promoted_to_background_and_reported() {
        let notifier = Arc::new(CollectingNotifier::default());
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                timeouts: ShellTimeoutPolicy {
                    default_timeout_secs: 300,
                    background_after_secs: 1,
                },
                background_notifier: Some(notifier.clone()),
                ..Default::default()
            },
        );
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo early-marker; sleep 3; echo late-marker"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        // 立即结算:task id + 输出文件指引,晚到的输出不在结果里
        assert!(
            output.output.contains("still running in background"),
            "{}",
            output.output
        );
        assert!(output.output.contains("task bash-1"), "{}", output.output);
        assert!(
            output.output.contains("Full output is being written to:"),
            "{}",
            output.output
        );
        assert!(!output.output.contains("late-marker"), "{}", output.output);
        let path = output.details["fullOutputPath"].as_str().unwrap().to_string();

        // 完成通知:exit code + 输出尾段(含转后台后才产出的行)
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let notification = loop {
            let texts = notifier.0.lock().unwrap().clone();
            if let Some(text) = texts.last() {
                break text.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "未在期限内收到完成通知"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(
            notification.contains("background task bash-1 finished successfully"),
            "{notification}"
        );
        assert!(notification.contains("late-marker"), "{notification}");
        // 全量输出落盘:转后台之后写入的行也在文件里
        let full = std::fs::read_to_string(&path).unwrap();
        assert!(full.contains("early-marker"), "{full}");
        assert!(full.contains("late-marker"), "{full}");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn explicit_timeout_not_longer_than_threshold_still_kills() {
        // 显式 timeout == 阈值:不武装后台(严格大于才武装),按超时杀灭
        let notifier = Arc::new(CollectingNotifier::default());
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                timeouts: ShellTimeoutPolicy {
                    default_timeout_secs: 300,
                    background_after_secs: 2,
                },
                background_notifier: Some(notifier.clone()),
                ..Default::default()
            },
        );
        let err = exec(
            &tool,
            serde_json::json!({"command": "sleep 30", "timeout": 2}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timed out after 2 seconds"), "{err}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            notifier.0.lock().unwrap().is_empty(),
            "超时杀灭路径不得触发后台完成通知"
        );
    }

    #[tokio::test]
    async fn background_not_armed_without_notifier() {
        // 无通知器 = 不武装后台化:长 timeout 的命令仍正常阻塞到退出
        let tool = bash_with(
            &std::env::temp_dir(),
            ShellSpawnOptions {
                timeouts: ShellTimeoutPolicy {
                    default_timeout_secs: 30,
                    background_after_secs: 1,
                },
                ..Default::default()
            },
        );
        let output = exec(
            &tool,
            serde_json::json!({"command": "echo plain; sleep 2"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output.output.trim(), "plain");
    }
}
