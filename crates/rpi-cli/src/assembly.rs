//! cli 装配(07 §8.7 步骤 3):settings(`mcpServers`)→ spawn 扩展 → 装配
//! `ExtensionHooks`;诊断打 stderr。`Agent`/`AgentSession` 不感知 MCP —— MCP
//! 扩展只经两条接缝(Subscriber 事件汇 + LoopHooks 包装)注入业务核。
//!
//! `build_session` 是四种模式(print/json/rpc/interactive)共享的装配点
//! (08 文档:模式只是同一业务核的不同 I/O 壳);`run_session` 是 print
//! 模式旧行为的便捷封装,端到端测试复用。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use rpi_agent::{AgentEvent, RunStop};
use rpi_core::{
    create_agent_session, create_extension_event_bus, ApprovalHooks, ApprovalRules, ApprovalUi,
    AgentSession, AgentSessionConfig, AgentSessionEvent, ExtensionHooks, ExtensionUi, HeadlessApproval,
    HeadlessApprovalUi, McpServerSpec, NoopUi, PermissionEngine, SandboxConfig,
    SessionMode,
    SessionSharedSubscriber, SessionSink, SessionSubscriber, SandboxPolicy as CoreSandboxPolicy,
    SystemPromptOptions,
};

/// 子代理工厂的模型解析器共享句柄(装配期与 /model 同源)。
type ModelResolverFn = Arc<dyn Fn(&str) -> Result<rpi_ai::Model, String> + Send + Sync>;

/// 读取扩展进程声明:项目 `.rpi/settings.json` → 全局 `~/.rpi/settings.json`,
/// mcpServers 列表拼接(07 §8.7 步骤 3)。
pub fn load_mcp_server_specs() -> Vec<McpServerSpec> {
    let mut specs = Vec::new();
    let mut paths = Vec::new();
    if let Ok(current) = std::env::current_dir() {
        paths.push(current.join(".rpi/settings.json"));
    }
    if let Some(home) = dirs_home() {
        paths.push(home.join(".rpi/settings.json"));
    }
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<SettingsFile>(&text) {
            Ok(settings) => specs.extend(settings.mcp_servers.unwrap_or_default()),
            Err(error) => {
                eprintln!("[rpi] 扩展 settings 解析失败 {}: {error}", path.display());
            }
        }
    }
    specs
}

#[derive(serde::Deserialize)]
struct SettingsFile {
    #[serde(rename = "mcpServers", alias = "mcp_servers", default)]
    mcp_servers: Option<Vec<McpServerSpec>>,
    /// T10:shell 命令统一前缀(settings 提供,如 `commandPrefix: "timeout 300"`)
    #[serde(rename = "commandPrefix", alias = "command_prefix", default)]
    command_prefix: Option<String>,
    /// bash 默认超时秒数(`bashTimeoutSecs`,模型未传 timeout 参数时生效;
    /// 未配置 = 默认 120)
    #[serde(rename = "bashTimeoutSecs", alias = "bash_timeout_secs", default)]
    bash_timeout_secs: Option<u64>,
    /// bash 自动转后台阈值秒数(`backgroundAfterSecs`:生效超时大于该值时,
    /// 运行超过该秒数即结算 tool result 并转后台;未配置 = 默认 60)
    #[serde(
        rename = "backgroundAfterSecs",
        alias = "background_after_secs",
        default
    )]
    background_after_secs: Option<u64>,
    /// 上下文快照开关(`contextSnapshot`):true = 每次模型请求落 context_ref
    /// 快照;未配置 = 关闭(不产生快照)
    #[serde(rename = "contextSnapshot", alias = "context_snapshot", default)]
    context_snapshot: Option<bool>,
    /// 激活工具集(`tools`):名字列表,如 `["bash"]`;未配置 = 全部激活;
    /// **空数组 `[]` = 显式不激活任何工具**
    #[serde(rename = "tools", alias = "active_tools", default)]
    tools: Option<Vec<String>>,
    /// 检索忽略列表(`searchIgnore`):grep/find/ls 过滤 + 系统提示词规则;
    /// 未配置 = 内置默认表;**空数组 `[]` = 关闭过滤**
    #[serde(rename = "searchIgnore", alias = "search_ignore", default)]
    search_ignore: Option<Vec<String>>,
    /// 超长 tool result 进转录的字符上限(头尾裁剪);未配置 = 默认 20000
    #[serde(rename = "toolResultMaxChars", alias = "tool_result_max_chars", default)]
    tool_result_max_chars: Option<usize>,
    /// 图片输入开关(`blockImages`):true = 发送给模型前把转录里的 Image 块
    /// 替换为文本占位符(模型不支持图片时无论此值都会替换);未配置 = false
    #[serde(rename = "blockImages", alias = "block_images", default)]
    block_images: Option<bool>,
    /// 自动压缩阈值(`compaction` 节)
    #[serde(rename = "compaction", alias = "auto_compact", default)]
    compaction: Option<CompactionConfig>,
    /// 会话模式(13 文档 §12):plan | confirm | full-access
    #[serde(rename = "sessionMode", alias = "session_mode", default)]
    session_mode: Option<String>,
    /// headless(print/json)遇审批请求的策略:deny | auto-approve
    #[serde(rename = "headlessApproval", alias = "headless_approval", default)]
    headless_approval: Option<String>,
    /// 后台 subagent 遇审批请求的策略(14 文档 §4.3,默认 deny):
    /// deny | auto-approve
    #[serde(
        rename = "subagentAsyncApproval",
        alias = "subagent_async_approval",
        default
    )]
    subagent_async_approval: Option<String>,
    /// 沙箱细节(Confirm 模式 WorkspaceWrite;Plan 固定 ReadOnly,FullAccess 固定关)
    #[serde(rename = "sandbox", default)]
    sandbox: Option<SandboxConfig>,
    /// 审批规则(Confirm 模式下免审/必禁)
    #[serde(rename = "approval", default)]
    approval: Option<ApprovalSettings>,
}

/// settings `approval` 节。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ApprovalSettings {
    allow_commands: Option<Vec<String>>,
    deny_commands: Option<Vec<String>>,
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// 纯函数面(可测):按 项目 → 全局 顺序读 `.rpi/settings.json`,
/// 解析失败的文件跳过。
fn read_settings_files(cwd: Option<&Path>, home: Option<&Path>) -> Vec<SettingsFile> {
    let mut paths = Vec::new();
    if let Some(cwd) = cwd {
        paths.push(cwd.join(".rpi/settings.json"));
    }
    if let Some(home) = home {
        paths.push(home.join(".rpi/settings.json"));
    }
    let mut settings = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<SettingsFile>(&text) {
            Ok(parsed) => settings.push(parsed),
            Err(_) => continue,
        }
    }
    settings
}

/// T10:读取 shell 命令前缀,项目 settings 优先于全局 settings。
pub fn load_shell_command_prefix() -> Option<String> {
    let cwd = std::env::current_dir().ok();
    let home = dirs_home();
    shell_command_prefix_from(cwd.as_deref(), home.as_deref())
}

/// shell 运行时限 settings(`bashTimeoutSecs`/`backgroundAfterSecs`):
/// 项目 settings 优先于全局,首个配置生效;未配置键 = 默认值。
pub fn load_shell_timeout_policy() -> rpi_tools::ShellTimeoutPolicy {
    let cwd = std::env::current_dir().ok();
    let home = dirs_home();
    shell_timeout_policy_from(cwd.as_deref(), home.as_deref())
}

/// 纯函数面(可测):两键独立解析,各自按 项目 → 全局 首个配置生效。
fn shell_timeout_policy_from(
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> rpi_tools::ShellTimeoutPolicy {
    let files = read_settings_files(cwd, home);
    let mut policy = rpi_tools::ShellTimeoutPolicy::default();
    if let Some(secs) = files.iter().find_map(|settings| settings.bash_timeout_secs) {
        policy.default_timeout_secs = secs;
    }
    if let Some(secs) = files
        .iter()
        .find_map(|settings| settings.background_after_secs)
    {
        policy.background_after_secs = secs;
    }
    policy
}

/// 纯函数面(可测):按 项目 → 全局 顺序读 `.rpi/settings.json` 的 commandPrefix,
/// 空白值跳过,首个非空生效。
fn shell_command_prefix_from(cwd: Option<&Path>, home: Option<&Path>) -> Option<String> {
    for settings in read_settings_files(cwd, home) {
        if let Some(prefix) = settings.command_prefix {
            if !prefix.trim().is_empty() {
                return Some(prefix);
            }
        }
    }
    None
}

/// 上下文快照开关(`contextSnapshot`):项目 settings 优先于全局;
/// 未配置 = false(默认关,不产生快照)。
pub fn load_context_snapshot_enabled() -> bool {
    let cwd = std::env::current_dir().ok();
    let home = dirs_home();
    context_snapshot_enabled_from(cwd.as_deref(), home.as_deref()).unwrap_or(false)
}

/// 纯函数面(可测):首个配置了 contextSnapshot 的 settings 生效。
fn context_snapshot_enabled_from(cwd: Option<&Path>, home: Option<&Path>) -> Option<bool> {
    read_settings_files(cwd, home)
        .into_iter()
        .find_map(|settings| settings.context_snapshot)
}

/// 激活工具集(settings `tools`):项目 settings 优先于全局,首个配置生效;
/// 条目去首尾空白、丢空项。**键未配置 = None(全部激活);键配置了(即使清空后
/// 为空)= Some(空)= 显式不激活任何工具**。
pub fn load_active_tool_names() -> Option<Vec<String>> {
    let cwd = std::env::current_dir().ok();
    let home = dirs_home();
    active_tool_names_from(cwd.as_deref(), home.as_deref())
}

/// 纯函数面(可测)。
fn active_tool_names_from(cwd: Option<&Path>, home: Option<&Path>) -> Option<Vec<String>> {
    let configured = read_settings_files(cwd, home)
        .into_iter()
        .find_map(|settings| settings.tools)?;
    Some(
        configured
            .into_iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect(),
    )
}

/// 检索忽略列表(settings `searchIgnore`):项目 settings 优先于全局,首个配置
/// 生效;未配置 = 内置默认表(node_modules/dist/target 等);条目非法 glob =
/// 诊断 + 回退内置默认表(配置错误显式暴露,不阻断会话)。
pub fn load_search_ignore() -> rpi_tools::SearchIgnore {
    let cwd = std::env::current_dir().ok();
    let home = dirs_home();
    search_ignore_from(cwd.as_deref(), home.as_deref())
}

/// 纯函数面(可测)。
fn search_ignore_from(cwd: Option<&Path>, home: Option<&Path>) -> rpi_tools::SearchIgnore {
    match read_settings_files(cwd, home)
        .into_iter()
        .find_map(|settings| settings.search_ignore)
    {
        Some(patterns) => rpi_tools::SearchIgnore::from_patterns(patterns).unwrap_or_else(|error| {
            eprintln!("[rpi] settings `searchIgnore` 解析失败,回退内置默认表: {error}");
            rpi_tools::SearchIgnore::builtin()
        }),
        None => rpi_tools::SearchIgnore::builtin(),
    }
}

/// 校验并解析激活工具集:配置的名字必须存在于候选工具(含扩展工具)中,
/// 否则报错列出可用名单;返回按配置顺序的名字列表。
fn resolve_active_tools(
    configured: &[String],
    tools: &[Arc<dyn rpi_agent::Tool>],
) -> Result<Vec<String>, String> {
    let known: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    for name in configured {
        if !known.contains(&name.as_str()) {
            return Err(format!(
                "settings `tools` 配置了未知工具 `{name}`(可用: {})",
                known.join(", ")
            ));
        }
    }
    Ok(configured.to_vec())
}

/// 系统提示词外置文件(用户可编辑):项目 `.rpi/system-prompt.md` → 全局
/// `~/.rpi/system-prompt.md`,首个存在且非空的文件生效。文件内容经
/// `<rules>` 标记块拆分:块外内容**替换身份句(preamble)**,块内内容追加进
/// `<rules>` 节(内置规则之后);无标记块 = 全文是身份句(向后兼容)。
/// 其余 section(`<env>`、`<tools>`)仍自动注入;未配置 = 用内置默认身份句。
/// 仅在程序启动/新建会话(装配期)读取一次,会话中途修改文件不生效。
pub fn load_system_prompt_override() -> (Option<String>, Option<String>) {
    let cwd = std::env::current_dir().ok();
    let home = dirs_home();
    system_prompt_override_from(cwd.as_deref(), home.as_deref())
        .map(|text| rpi_core::split_prompt_and_rules(&text))
        .unwrap_or((None, None))
}

/// 纯函数面(可测):按 项目 → 全局 找 system-prompt.md,空白文件视为未配置。
fn system_prompt_override_from(cwd: Option<&Path>, home: Option<&Path>) -> Option<String> {
    let mut paths = Vec::new();
    if let Some(cwd) = cwd {
        paths.push(cwd.join(".rpi/system-prompt.md"));
    }
    if let Some(home) = home {
        paths.push(home.join(".rpi/system-prompt.md"));
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

/// 装配产物:四种模式共享的业务核句柄。`session_manager` 供 rpc 模式的
/// get_tree/get_entries/fork 命令查询会话树(rpi-session 是可选组件);
/// `manager_holder` 是可切换指针 —— /new 运行期新建 session 时整体换目标,
/// sink/compactor/PI_* 环境闭包每次使用都读当前值。
/// `subagent_registry` 供 /new 与会话退出时 abort 全部存活 subagent 运行。
pub struct BuiltSession {
    pub session: Arc<AgentSession>,
    pub session_manager: Option<Arc<rpi_session::SessionManager>>,
    pub manager_holder: SessionManagerHolder,
    /// 装配生效的压缩配置(/session 展示用)
    pub compaction_config: CompactionConfig,
    /// rpc 审批通道(run_rpc_mode 路由 approval_response 用;其他模式 None)
    pub rpc_approval: Option<Arc<crate::modes::rpc::RpcApprovalUi>>,
    /// 后台 subagent 运行注册表(14 文档 §4.3;None = 未装配)
    pub subagent_registry: Option<Arc<rpi_core::SubagentRegistry>>,
    /// /subagent 平行会话工厂(与主会话同源依赖;None = 未装配)
    pub subagent_factory: Option<Arc<rpi_core::SubagentSessionFactory>>,
}

/// 可切换的会话存储句柄(内部 `Arc<RwLock<Option<Arc<SessionManager>>>>`)。
#[derive(Clone, Default)]
pub struct SessionManagerHolder(
    Arc<std::sync::RwLock<Option<Arc<rpi_session::SessionManager>>>>,
);

impl SessionManagerHolder {
    pub fn new(manager: Option<Arc<rpi_session::SessionManager>>) -> Self {
        SessionManagerHolder(Arc::new(std::sync::RwLock::new(manager)))
    }

    pub fn get(&self) -> Option<Arc<rpi_session::SessionManager>> {
        self.0.read().unwrap().clone()
    }

    pub fn set(&self, manager: Option<Arc<rpi_session::SessionManager>>) {
        *self.0.write().unwrap() = manager;
    }
}

/// /new(pi 无对应命令,rpi 扩展):新建 session 文件并把全部持久化句柄
/// (sink/compactor/PI_* 环境)切过去。旧文件不做任何操作 —— append-only
/// 语义下它天然处于已保存状态。模型/思考级别作为设置态 entry 写入新文件,
/// 保持新会话自描述。流式期间调用方须先行拒绝。返回新 session 文件路径
/// (内存会话为 None)。
pub async fn switch_new_session(
    session: &AgentSession,
    holder: &SessionManagerHolder,
) -> Result<Option<std::path::PathBuf>, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let old = holder.get();
    let parent_session = old.as_ref().map(|manager| manager.session_id().to_string());
    let new_manager: Arc<rpi_session::SessionManager> =
        match old.as_ref().and_then(|manager| manager.file_path()) {
            Some(old_file) => {
                let dir = old_file
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                rpi_session::create_session_in_dir(
                    dir,
                    &cwd.display().to_string(),
                    parent_session.as_deref(),
                    None,
                )
                .map_err(|e| e.to_string())?
                .into()
            }
            None => rpi_session::create_session(None::<String>)
                .map_err(|e| e.to_string())?
                .into(),
        };
    let new_path = new_manager.file_path().map(|p| p.to_path_buf());
    holder.set(Some(new_manager));

    // 清空转录与队列(错误状态一并复位);模型/思考级别/会话模式随新文件落
    // 设置态 entry,保持新会话自描述
    session.agent().reset().map_err(|e| e.to_string())?;
    let snapshot = session.agent().state_snapshot();
    if let Some(model) = snapshot.model {
        session.set_model(model).await;
    }
    session.set_thinking_level(snapshot.thinking_level).await;
    session.set_mode(session.default_mode()).await.map_err(|e| e.to_string())?;
    Ok(new_path)
}

/// /session 切回历史会话(rpi 扩展):加载既有 JSONL 续写(append-only,
/// 同一文件继续追加),按投影重建 seed 消息与设置态(thinking level、激活
/// 工具集、会话模式)。模型不随文件恢复(与 --continue 一致:沿用当前模型)。
/// 流式期间调用方须先行拒绝。返回 session 文件路径。
pub async fn switch_resume_session(
    session: &AgentSession,
    holder: &SessionManagerHolder,
    file: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, String> {
    let manager: Arc<rpi_session::SessionManager> =
        rpi_session::create_session(Some(file))
            .map_err(|e| e.to_string())?
            .into();
    let path = manager.file_path().map(|p| p.to_path_buf());
    holder.set(Some(manager.clone()));

    // 清空转录与队列(错误状态一并复位)后按当前分支投影重建上下文
    session.agent().reset().map_err(|e| e.to_string())?;
    let context = rpi_session::build_session_context(
        &manager.branch_entries(),
        manager.get_leaf_id().as_deref(),
    );
    session
        .agent()
        .set_messages(context.messages)
        .map_err(|e| e.to_string())?;
    let thinking = parse_thinking_level(&context.thinking_level);
    session.set_thinking_level(thinking).await;
    if let Some(tools) = context.active_tools {
        session
            .set_active_tools_by_name(&tools)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mode = context
        .mode
        .as_deref()
        .and_then(SessionMode::parse)
        .unwrap_or_else(|| session.default_mode());
    session.set_mode(mode).await.map_err(|e| e.to_string())?;
    Ok(path)
}

/// 会话存储策略:`Memory` 纯内存(测试);`New` 在目录下新建
/// `<session-id>.jsonl`;`Resume` 打开既有 JSONL 续聊(`--continue`)。#[derive(Debug, Clone)]
pub enum SessionStore {
    Memory,
    New { dir: std::path::PathBuf },
    Resume { file: std::path::PathBuf },
}

pub struct BuildOptions {
    pub provider: Arc<dyn rpi_ai::Provider>,
    pub model: rpi_ai::Model,
    /// 模式各自的 `ExtensionUi` 实现(print/json 为 no-op,rpc 为反向通道,
    /// interactive 为 TUI 真实现 —— 接缝 #5)
    pub ui: Arc<dyn ExtensionUi>,
    pub extension_specs: Vec<McpServerSpec>,
    /// T10:spawn 前命令改写钩子(可接扩展管线;缺省 None = 不改写)。
    /// commandPrefix 来自 settings(`load_shell_command_prefix`),不经本字段。
    pub spawn_hook: Option<Arc<dyn rpi_tools::ShellSpawnHook>>,
    /// 会话存储策略(CLI 默认文件持久化,`--continue` 续聊)
    pub session_store: SessionStore,
    /// 上下文快照开关(context_ref):None = 关;Some(true/false) = 显式指定。
    /// CLI 入口(main)负责把 settings 的 `contextSnapshot` 解析成此字段 ——
    /// build_session 不直接读用户 settings,测试不依赖本机配置。
    pub context_snapshot: Option<bool>,
    /// 激活工具集覆盖:None = 全部激活;Some(list) = 显式指定(空列表 = 不激活
    /// 任何工具),名字须存在于候选工具。CLI 入口(main)负责把 settings 的
    /// `tools` 键解析成此字段 —— build_session 不直接读用户 settings,测试
    /// 不依赖本机配置。
    pub active_tools: Option<Vec<String>>,
    /// 检索忽略列表(grep/find/ls 过滤 + 系统提示词规则;settings `searchIgnore`,
    /// main 解析;未配置 = 内置默认表)
    pub search_ignore: rpi_tools::SearchIgnore,
    /// 超长 tool result 字符上限:None = 默认 20000;Some(0) = 不裁剪。
    pub tool_result_max_chars: Option<usize>,
    /// 图片输入开关(settings `blockImages`):true = 发送前 Image 块替换为
    /// 文本占位符(默认 false = 允许图片进转录)。
    pub block_images: bool,
    /// 自动压缩阈值(默认 = CompactionSettings::default)。
    pub compaction: CompactionConfig,
    /// CLI 显式会话模式(--session-mode/--plan/--yolo;None = 未显式指定,
    /// resume 走 entry 恢复,新会话走 default_session_mode)。
    pub session_mode: Option<SessionMode>,
    /// 新会话默认模式(settings `sessionMode` → CLI 覆盖 → Plan)。
    pub default_session_mode: SessionMode,
    /// 沙箱细节(Confirm 模式 WorkspaceWrite;CLI --sandbox-* 已在 main 合并)
    pub sandbox: SandboxConfig,
    /// 审批规则(settings `approval`)
    pub approval: ApprovalRules,
    /// 后台 subagent 审批策略(settings `subagentAsyncApproval`,默认 deny;
    /// 14 文档 §4.3)
    pub subagent_async_approval: HeadlessApproval,
    /// 审批 UI(接缝 #5:interactive 传 TuiApprovalUi,rpc 传 RpcApprovalUi,
    /// print/json 传 HeadlessApprovalUi;None = 按 deny 策略兜底)
    pub approval_ui: Option<Arc<dyn ApprovalUi>>,
    /// rpc 审批通道(供 run_rpc_mode 路由 approval_response;与 approval_ui
    /// 指向同一实例)
    pub rpc_approval: Option<Arc<crate::modes::rpc::RpcApprovalUi>>,
}

/// T9:PI_* 会话环境快照闭包。装配期建共享 cell(`Weak<AgentSession>`),
/// session 建好后回填;工具执行时按需读取,无 session(空 cell)返回空 =
/// 现状行为。禁用可变全局状态(policy §2):经工具配置参数注入。
/// manager 经 holder 读取:/new 切换后 PI_SESSION_* 跟随新会话。
fn session_env_fn(
    session_cell: Arc<Mutex<Weak<AgentSession>>>,
    manager_holder: SessionManagerHolder,
) -> rpi_tools::SessionEnvFn {
    Arc::new(move || {
        let Some(session) = session_cell.lock().unwrap().upgrade() else {
            return Vec::new();
        };
        let snapshot = session.agent().state_snapshot();
        let mut env: Vec<(String, String)> = Vec::new();
        if let Some(model) = &snapshot.model {
            env.push(("PI_PROVIDER".into(), model.provider.clone()));
            env.push(("PI_MODEL".into(), model.id.clone()));
        }
        if let Some(level) = snapshot.thinking_level {
            env.push((
                "PI_REASONING_LEVEL".into(),
                thinking_level_name(level).into(),
            ));
        }
        if let Some(manager) = manager_holder.get() {
            env.push(("PI_SESSION_ID".into(), manager.session_id().to_string()));
            if let Some(path) = manager.file_path() {
                env.push(("PI_SESSION_FILE".into(), path.display().to_string()));
            }
        }
        env
    })
}

fn thinking_level_name(level: rpi_ai::ThinkingLevel) -> &'static str {
    match level {
        rpi_ai::ThinkingLevel::Minimal => "minimal",
        rpi_ai::ThinkingLevel::Low => "low",
        rpi_ai::ThinkingLevel::Medium => "medium",
        rpi_ai::ThinkingLevel::High => "high",
        rpi_ai::ThinkingLevel::Xhigh => "xhigh",
        rpi_ai::ThinkingLevel::Max => "max",
    }
}

/// session 投影设置态 → ThinkingLevel("off" = None)。`/thinking` 命令复用。
pub fn parse_thinking_level(name: &str) -> Option<rpi_ai::ThinkingLevel> {
    match name {
        "minimal" => Some(rpi_ai::ThinkingLevel::Minimal),
        "low" => Some(rpi_ai::ThinkingLevel::Low),
        "medium" => Some(rpi_ai::ThinkingLevel::Medium),
        "high" => Some(rpi_ai::ThinkingLevel::High),
        "xhigh" => Some(rpi_ai::ThinkingLevel::Xhigh),
        "max" => Some(rpi_ai::ThinkingLevel::Max),
        _ => None,
    }
}

/// 从用户 settings 解析出的会话运行期开关。装配层(`build_session`)不直接读
/// 用户 settings —— CLI 入口(main)解析一次后显式传入,测试/嵌入方自行构造,
/// 行为不依赖本机配置文件。
#[derive(Debug, Clone)]
pub struct SessionSettings {
    /// settings `contextSnapshot`(默认关)
    pub context_snapshot: bool,
    /// settings `tools`:None = 全部激活;Some(空) = 不激活任何工具
    pub active_tools: Option<Vec<String>>,
    /// settings `searchIgnore`:grep/find/ls 检索忽略列表 + 系统提示词规则
    /// (未配置 = 内置默认表)
    pub search_ignore: rpi_tools::SearchIgnore,
    /// settings `toolResultMaxChars`:超长 tool result 进转录的字符上限
    /// (头尾裁剪);None = 默认 20000,0 = 不裁剪
    pub tool_result_max_chars: Option<usize>,
    /// settings `blockImages`:发送前把转录里的 Image 块替换为文本占位符
    pub block_images: bool,
    /// settings 压缩配置(自动压缩阈值,见 `CompactionConfig`)
    pub compaction: CompactionConfig,
    /// settings `sessionMode`:新会话初始档位(默认 Plan;13 文档 §12)
    pub session_mode: SessionMode,
    /// settings `headlessApproval`:print/json 遇审批请求的策略(默认 deny)
    pub headless_approval: HeadlessApproval,
    /// settings `subagentAsyncApproval`:后台 subagent 遇审批请求的策略
    /// (默认 deny;14 文档 §4.3 fail-closed)
    pub subagent_async_approval: HeadlessApproval,
    /// settings `sandbox`(Confirm 模式 WorkspaceWrite 细节;CLI --sandbox-*
    /// 在 main 合并)
    pub sandbox: SandboxConfig,
    /// settings `approval`(allow/deny 前缀规则)
    pub approval: ApprovalRules,
}

/// 压缩设置(settings `compaction` 节;自动压缩阈值可配置)。
/// `reserveTokens` 语义:**>= 1.0 = 绝对 token 数**;**0 < v < 1.0 = context_window
/// 的百分比**(0.1 = 10%,随模型窗口缩放;100% 无法表达)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionConfig {
    /// 自动压缩总开关
    pub enabled: bool,
    /// 上下文保留预算(绝对 token 数或窗口百分比)
    pub reserve_tokens: f64,
    /// 压缩时保留的近期原文 token 预算
    pub keep_recent_tokens: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        let d = rpi_session::CompactionSettings::default();
        CompactionConfig {
            enabled: d.enabled,
            reserve_tokens: d.reserve_tokens,
            keep_recent_tokens: d.keep_recent_tokens,
        }
    }
}

impl From<CompactionConfig> for rpi_session::CompactionSettings {
    fn from(config: CompactionConfig) -> Self {
        rpi_session::CompactionSettings {
            enabled: config.enabled,
            reserve_tokens: config.reserve_tokens,
            keep_recent_tokens: config.keep_recent_tokens,
        }
    }
}

impl Default for SessionSettings {
    fn default() -> Self {
        SessionSettings {
            context_snapshot: false,
            active_tools: None,
            search_ignore: rpi_tools::SearchIgnore::builtin(),
            tool_result_max_chars: None,
            block_images: false,
            compaction: CompactionConfig::default(),
            session_mode: SessionMode::Plan,
            headless_approval: HeadlessApproval::Deny,
            subagent_async_approval: HeadlessApproval::Deny,
            sandbox: SandboxConfig::default(),
            approval: ApprovalRules::default(),
        }
    }
}

/// 会话模式与 headless 审批策略的 settings 解析(纯函数,可测)。
fn parse_session_mode(name: Option<&String>) -> SessionMode {
    name.and_then(|value| SessionMode::parse(value)).unwrap_or(SessionMode::Plan)
}

fn parse_headless_approval(name: Option<&String>) -> HeadlessApproval {
    match name.map(|value| value.trim()) {
        Some("auto-approve") | Some("auto_approve") | Some("autoApprove") => {
            HeadlessApproval::AutoApprove
        }
        _ => HeadlessApproval::Deny,
    }
}

/// 检索忽略表 → 系统提示词规则(空列表 = 关闭过滤 = 无规则)。主会话检索多经
/// bash(rg/find/ls),工具层过滤只覆盖只读工具集;这里把同一份忽略表同步给
/// 模型,约束任意路径的检索行为。
fn search_ignore_rule(ignore: &rpi_tools::SearchIgnore) -> Option<String> {
    if ignore.is_empty() {
        return None;
    }
    Some(format!(
        "When searching or listing files (find/grep/rg/ls), never descend into dependency or \
         build-output paths: {}. Exclude them from every search unless the user asks \
         explicitly.",
        ignore.patterns().join(", ")
    ))
}

/// <rules> 追加规则合并:用户外置规则(system-prompt.md)在前,装配级追加在后。
fn merge_rules(user: Option<String>, extra: Option<String>) -> Option<String> {
    match (user, extra) {
        (Some(mut user), Some(extra)) => {
            user.push('\n');
            user.push_str(&extra);
            Some(user)
        }
        (Some(user), None) => Some(user),
        (None, extra) => extra,
    }
}

/// CLI 入口用:按 项目 → 全局 顺序解析 settings 的运行期开关(项目优先,
/// 各键独立回退全局)。
pub fn load_session_settings() -> SessionSettings {
    let cwd = std::env::current_dir().ok();
    let home = dirs_home();
    let files = read_settings_files(cwd.as_deref(), home.as_deref());
    let tool_result_max_chars = files
        .iter()
        .find_map(|settings| settings.tool_result_max_chars);
    let block_images = files
        .iter()
        .find_map(|settings| settings.block_images)
        .unwrap_or(false);
    let compaction = files
        .iter()
        .find_map(|settings| settings.compaction.clone())
        .unwrap_or_default();
    let session_mode = files
        .iter()
        .find_map(|settings| settings.session_mode.clone());
    let headless_approval = files
        .iter()
        .find_map(|settings| settings.headless_approval.clone());
    let subagent_async_approval = files
        .iter()
        .find_map(|settings| settings.subagent_async_approval.clone());
    let sandbox = files
        .iter()
        .find_map(|settings| settings.sandbox.clone())
        .unwrap_or_default();
    let approval = files
        .iter()
        .find_map(|settings| settings.approval.clone())
        .map(|approval| ApprovalRules {
            allow_commands: approval.allow_commands.unwrap_or_default(),
            deny_commands: approval.deny_commands.unwrap_or_default(),
        })
        .unwrap_or_default();
    SessionSettings {
        context_snapshot: load_context_snapshot_enabled(),
        active_tools: load_active_tool_names(),
        search_ignore: search_ignore_from(cwd.as_deref(), home.as_deref()),
        tool_result_max_chars,
        block_images,
        compaction,
        session_mode: parse_session_mode(session_mode.as_ref()),
        headless_approval: parse_headless_approval(headless_approval.as_ref()),
        subagent_async_approval: parse_headless_approval(subagent_async_approval.as_ref()),
        sandbox,
        approval,
    }
}

/// rpi-sandbox 平台后端 → rpi-core 策略类型的映射(装配层职责:core 不依赖
/// rpi-sandbox,可拆卸判据)。
fn map_policy(policy: &CoreSandboxPolicy) -> rpi_sandbox::SandboxPolicy {
    match policy {
        CoreSandboxPolicy::ReadOnly { network_access } => {
            rpi_sandbox::SandboxPolicy::ReadOnly {
                network_access: *network_access,
            }
        }
        CoreSandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
        } => rpi_sandbox::SandboxPolicy::WorkspaceWrite {
            writable_roots: writable_roots.clone(),
            network_access: *network_access,
        },
        CoreSandboxPolicy::DangerFullAccess => rpi_sandbox::SandboxPolicy::DangerFullAccess,
    }
}

/// 沙箱包装钩子(13 文档 §7.6):按当前模式取 SandboxPolicy 包装 shell 命令。
/// 只做纯包装,不拒绝 —— 拒绝是权限引擎的事;平台无沙箱时原样返回
/// (Confirm 已升级为逐命令审批,Plan 由只读判定兜底 —— 判定通过即执行,
/// 降级矩阵 §7.5)。
struct SandboxSpawnHook {
    engine: Arc<PermissionEngine>,
    cwd: std::path::PathBuf,
    sandbox_cache: Mutex<HashMap<SessionMode, Option<Arc<dyn rpi_sandbox::Sandbox>>>>,
}

impl SandboxSpawnHook {
    fn sandbox_for_current_mode(&self) -> Option<Arc<dyn rpi_sandbox::Sandbox>> {
        let mode = self.engine.mode();
        if !self.engine.sandbox_available() {
            return None;
        }
        let cached = self.sandbox_cache.lock().unwrap().get(&mode).cloned();
        if let Some(cached) = cached {
            return cached;
        }
        let policy = self.engine.policy();
        if matches!(policy, CoreSandboxPolicy::DangerFullAccess) {
            self.sandbox_cache.lock().unwrap().insert(mode, None);
            return None;
        }
        let helper_exe = std::env::current_exe().ok();
        let created = rpi_sandbox::create_sandbox(&map_policy(&policy), helper_exe.as_deref())
            .map_err(|error| format!("沙箱构造失败: {error}"))
            .ok()
            .flatten();
        self.sandbox_cache.lock().unwrap().insert(mode, created.clone());
        created
    }
}

#[async_trait]
impl rpi_tools::ShellSpawnHook for SandboxSpawnHook {
    async fn rewrite(&self, command: String) -> Result<String, String> {
        match self.sandbox_for_current_mode() {
            None => Ok(command),
            Some(sandbox) => sandbox.wrap_command(&command, &self.cwd),
        }
    }
}

/// 外部 spawn 钩子与沙箱钩子的串联:外部先改写(检查的是用户命令),
/// 沙箱最后包整条命令(13 文档 §7.6)。
struct ChainedSpawnHook {
    first: Option<Arc<dyn rpi_tools::ShellSpawnHook>>,
    second: Arc<dyn rpi_tools::ShellSpawnHook>,
}

#[async_trait]
impl rpi_tools::ShellSpawnHook for ChainedSpawnHook {
    async fn rewrite(&self, command: String) -> Result<String, String> {
        let command = match &self.first {
            Some(first) => first.rewrite(command).await?,
            None => command,
        };
        self.second.rewrite(command).await
    }
}

/// includeContent 后台抓取完成通知:仿 subagent supervisor 的
/// wait_idle → follow_up → continue_run 唤醒链。
struct AgentFollowUpNotifier {
    agent: Arc<Mutex<Weak<rpi_agent::Agent>>>,
}

#[async_trait]
impl rpi_web::BackgroundNotifier for AgentFollowUpNotifier {
    async fn notify(&self, text: String) {
        let Some(agent) = self.agent.lock().unwrap().upgrade() else {
            return; // 会话已释放:通知无处投递,丢弃
        };
        tokio::spawn(async move {
            agent.wait_idle().await;
            agent.follow_up(rpi_agent::AgentMessage::user(text));
            let _ = agent.continue_run().await;
        });
    }
}

/// bash 后台任务完成通知:同一唤醒链(rpi_tools::BackgroundNotifier 接缝)。
struct ShellBackgroundNotifier {
    agent: Arc<Mutex<Weak<rpi_agent::Agent>>>,
}

#[async_trait]
impl rpi_tools::BackgroundNotifier for ShellBackgroundNotifier {
    async fn notify(&self, text: String) {
        let Some(agent) = self.agent.lock().unwrap().upgrade() else {
            return; // 会话已释放:通知无处投递,丢弃
        };
        tokio::spawn(async move {
            agent.wait_idle().await;
            agent.follow_up(rpi_agent::AgentMessage::user(text));
            let _ = agent.continue_run().await;
        });
    }
}

/// 共享装配:扩展连接失败不阻断(诊断打 stderr,07 §8.5)。
pub async fn build_session(options: BuildOptions) -> Result<BuiltSession, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let BuildOptions {
        provider,
        model,
        ui,
        extension_specs,
        spawn_hook: external_spawn_hook,
        session_store,
        context_snapshot,
        active_tools,
        search_ignore,
        tool_result_max_chars,
        block_images,
        compaction,
        session_mode,
        default_session_mode,
        sandbox,
        approval,
        subagent_async_approval,
        approval_ui,
        rpc_approval,
    } = options;

    // 扩展:settings → spawn → 总线;连接失败 = 诊断 + 跳过(绝不击穿宿主)
    let diagnostics = rpi_core::create_diagnostics_sink();
    let (bus, extension_tools) =
        create_extension_event_bus(extension_specs, ui.clone(), diagnostics.clone()).await;
    for diagnostic in diagnostics.lock().unwrap().iter() {
        eprintln!(
            "[rpi][extension:{}] {}",
            diagnostic.extension, diagnostic.message
        );
    }

    // 运行期诊断可见:后台 drain 打 stderr(07 §8.5 面向 mode 可见)
    rpi_core::spawn_diagnostics_printer(diagnostics.clone());

    // 权限系统(13 文档):平台沙箱探测 → 引擎 → 审批钩子(洋葱最外层)。
    // 降级矩阵 §7.5:无沙箱平台 Plan 剔除 bash,Confirm 逐命令审批,绝不静默裸跑
    let availability = rpi_sandbox::detect_availability();
    let sandbox_available = !matches!(availability, rpi_sandbox::SandboxAvailability::None);
    let engine = Arc::new(PermissionEngine::new(
        default_session_mode,
        sandbox.clone(),
        approval.clone(),
        cwd.clone(),
        sandbox_available,
    ));
    if default_session_mode != SessionMode::FullAccess && !sandbox_available {
        eprintln!(
            "[rpi][sandbox] 未检测到可用沙箱({:?}),Confirm 模式将逐命令请求批准,Plan 模式 bash 仅放行只读判定通过的命令",
            availability
        );
    }

    // 共享订阅者(审批事件/重试事件/总线共用同一列表)
    let subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>> = Arc::new(Mutex::new(Vec::new()));

    // 决策类埋点:Approval(最外)→ Extension(最内)→ Passthrough。
    // 审批先问(便宜、人审),批准后才轮到扩展埋点(13 文档 §4.1)。
    // 模式节不经 hooks:apply_mode 把当前模式提示词作为持久 ModeSection
    // 消息 append 进转录(位置永久固定,append-only 保 KV 缓存前缀)。
    let base_inner_hooks: Arc<dyn rpi_agent::LoopHooks> = if bus.is_empty() {
        Arc::new(rpi_agent::PassthroughHooks)
    } else {
        Arc::new(ExtensionHooks::new(
            Arc::new(rpi_agent::PassthroughHooks),
            bus.clone(),
        ))
    };
    let approval_ui: Arc<dyn ApprovalUi> = approval_ui
        .unwrap_or_else(|| Arc::new(HeadlessApprovalUi { policy: HeadlessApproval::Deny }));
    let approval_ui_for_factory = approval_ui.clone();
    let child_hooks: Arc<dyn rpi_agent::LoopHooks> = Arc::new(ApprovalHooks::new(
        base_inner_hooks.clone(),
        engine.clone(),
        approval_ui.clone(),
        subscribers.clone(),
    ));
    let hooks: Arc<dyn rpi_agent::LoopHooks> = Arc::new(ApprovalHooks::new(
        base_inner_hooks,
        engine.clone(),
        approval_ui,
        subscribers.clone(),
    ));
    // 会话树管理器先于工具装配创建:T9 的 PI_* 环境闭包需要读 session id/file。
    // 默认文件持久化(`~/.rpi/sessions/<项目前缀>/<时间>__<session-id>.jsonl`),Memory 仅测试用
    let session_manager: Arc<rpi_session::SessionManager> = match &session_store {
        SessionStore::Memory => rpi_session::create_session(None::<String>)
            .map_err(|e| e.to_string())?
            .into(),
        SessionStore::New { dir } => {
            rpi_session::create_session_in_dir(dir, &cwd.display().to_string(), None, None)
                .map_err(|e| e.to_string())?
                .into()
        }
        SessionStore::Resume { file } => rpi_session::create_session(Some(file))
            .map_err(|e| e.to_string())?
            .into(),
    };
    let manager_holder = SessionManagerHolder::new(Some(session_manager.clone()));
    // 上下文快照(context_ref):调用方显式开启(main 从 settings `contextSnapshot`
    // 解析)才装配 —— 经 `StreamOptions.on_payload` 观测**发送前的原始请求体**
    // (第一手),原样落盘到 session 旁 .ctx 目录 + context_ref entry。默认关,
    // 不装配 = 零开销。内存会话即使开启也在 manager 内部跳过
    let stream_options = if context_snapshot.unwrap_or(false) {
        let manager = session_manager.clone();
        rpi_ai::StreamOptions {
            on_payload: Some(Arc::new(move |body: &mut serde_json::Value| {
                if let Err(error) = manager.append_context_snapshot(body) {
                    eprintln!("[rpi] context snapshot append failed: {error}");
                }
            })),
            ..rpi_ai::StreamOptions::default()
        }
    } else {
        rpi_ai::StreamOptions::default()
    };

    // transcript 统一:resume 时从 Session projection 回填初始转录(source of
    // truth → context);设置态(thinking level、激活工具集、会话模式)一并恢复
    let (seed_messages, seed_thinking_level, seed_active_tools, seed_mode) = match &session_store {
        SessionStore::Resume { .. } => {
            let context = rpi_session::build_session_context(
                &session_manager.branch_entries(),
                session_manager.get_leaf_id().as_deref(),
            );
            let mode = context.mode.as_deref().and_then(SessionMode::parse);
            (context.messages, context.thinking_level, context.active_tools, mode)
        }
        _ => (Vec::new(), "off".to_string(), None, None),
    };
    // 模式优先级(13 文档 §8.1):CLI 显式 > resume 的 ModeChange entry > 默认
    let resume_or_default = match &session_store {
        SessionStore::Resume { .. } => seed_mode.unwrap_or(default_session_mode),
        _ => default_session_mode,
    };
    let effective_mode = session_mode.unwrap_or(resume_or_default);
    engine.set_mode(effective_mode);

    // T9/T10:shell 工具装配选项 —— PI_* 会话环境 + settings 命令前缀 +
    // 沙箱包装钩子(13 文档 §7.6;外部 spawn 钩子先改写,沙箱最后包整条命令)
    // + 运行时限策略(默认超时/转后台阈值,settings `bashTimeoutSecs`/
    // `backgroundAfterSecs`)+ 后台完成通知(wait_idle → follow_up 唤醒链,
    // agent 弱引在会话建好后回填)
    let session_cell: Arc<Mutex<Weak<AgentSession>>> = Arc::new(Mutex::new(Weak::new()));
    let shell_bg_cell: Arc<Mutex<Weak<rpi_agent::Agent>>> = Arc::new(Mutex::new(Weak::new()));
    let sandbox_hook: Arc<dyn rpi_tools::ShellSpawnHook> = Arc::new(SandboxSpawnHook {
        engine: engine.clone(),
        cwd: cwd.clone(),
        sandbox_cache: Mutex::new(HashMap::new()),
    });
    let spawn_hook: Arc<dyn rpi_tools::ShellSpawnHook> = match external_spawn_hook {
        Some(external) => Arc::new(ChainedSpawnHook {
            first: Some(external),
            second: sandbox_hook,
        }),
        None => sandbox_hook,
    };
    let shell = rpi_tools::ShellSpawnOptions {
        session_env: Some(session_env_fn(
            session_cell.clone(),
            manager_holder.clone(),
        )),
        command_prefix: load_shell_command_prefix(),
        spawn_hook: Some(spawn_hook),
        timeouts: load_shell_timeout_policy(),
        background_notifier: Some(Arc::new(ShellBackgroundNotifier {
            agent: shell_bg_cell.clone(),
        })),
    };

    // 内置工具 + 扩展注册工具(McpTool,名字带扩展前缀)
    // 工具输出自我上限与 agent 转录裁剪同源派生(settings `toolResultMaxChars`,
    // None = 默认 20k,Some(0) = 关闭裁剪 → 工具回退自身默认;派生 = 上限 - 2k
    // 余量,见 rpi_agent::tool_self_output_limit)
    let agent_tool_result_max_chars =
        tool_result_max_chars.unwrap_or(rpi_agent::DEFAULT_TOOL_RESULT_MAX_CHARS);
    let tool_output_limits = if agent_tool_result_max_chars == 0 {
        rpi_tools::OutputLimits::default()
    } else {
        rpi_tools::OutputLimits {
            max_chars: rpi_agent::tool_self_output_limit(agent_tool_result_max_chars),
            ..Default::default()
        }
    };
    let mut tools = rpi_tools::create_tools_at_with_shell_and_limits(&cwd, shell, tool_output_limits)
        .all()
        .to_vec();
    tools.extend(extension_tools);

    // 重试装饰器(pi 的 retryAssistantCall 注入点)+ AutoRetry 事件面
    let retry_hooks = rpi_core::create_session_retry_hooks(subscribers.clone());
    let provider = rpi_core::create_retrying_provider(
        provider,
        rpi_ai::RetryPolicy::default(),
        Some(retry_hooks),
    );

    // 进程内 web 扩展(rpi-web,16 文档):搜索/抓取/检索/取证四工具,
    // 随会话常驻激活(tools 数组从首请求起恒定,保 prompt 缓存前缀)。
    // 配置 ~/.rpi/web-search.json + 项目 .rpi/web-search.json;零配置可用
    // (auto 链兜底 duckduckgo)。rpi-core 能力经 trait 注入,保持 rpi-web
    // 不依赖 rpi-core
    let web_home = dirs_home();
    let web_config = rpi_web::config::load_web_search_config(Some(&cwd), web_home.as_deref());
    rpi_web::storage::set_fetch_cache_dir(
        rpi_web::config::config_dir(web_home.as_deref()).join("web-search-cache"),
    );
    let web_resolver =
        rpi_core::create_model_resolver_from_config(Some(&cwd), web_home.as_deref());
    // 当前主模型:会话建好后回填的弱引(agent 状态快照取 model)
    let web_agent_cell: Arc<Mutex<Weak<rpi_agent::Agent>>> = Arc::new(Mutex::new(Weak::new()));
    let web_agent_for_model = web_agent_cell.clone();
    let web_llm = rpi_web::llm::LlmDeps {
        provider: provider.clone(),
        resolve_model: Arc::new(move |spec: &str| web_resolver.resolve(spec)),
        current_model: Arc::new(move || {
            let agent = web_agent_for_model.lock().unwrap().upgrade()?;
            agent.state_snapshot().model
        }),
    };
    let web_cache_limits = web_config.cache_limits.unwrap_or_default();
    let web_context = Arc::new(rpi_web::tools::WebContext {
        config: web_config,
        cache_limits: web_cache_limits,
        llm: web_llm,
        notifier: Some(Arc::new(AgentFollowUpNotifier {
            agent: web_agent_cell.clone(),
        })),
        tool_result_max_chars: agent_tool_result_max_chars,
        cwd: cwd.clone(),
    });
    tools.extend(rpi_web::tools::create_web_tools(web_context));

    // 进程内 subagent 引擎(14 文档 §4):task 工具编译进二进制,agent 类型
    // 定义是数据文件(.rpi/agents/*.md,项目优先);解析失败诊断打 stderr 跳过。
    // 递归防护 = 子工具面裁剪;后台审批按 settings 策略(默认 deny,fail-closed)
    let subagent_home = dirs_home();
    let (agent_defs, subagent_diagnostics) =
        rpi_core::discover_agent_defs(&cwd, subagent_home.as_deref());
    for diagnostic in &subagent_diagnostics {
        eprintln!("[rpi][subagent] {diagnostic}");
    }
    // skill 机制(pi skills.ts 移植):skill 是数据目录(.rpi/skills/<name>/SKILL.md,
    // 项目优先);摘要经 load_skill 工具描述暴露给模型,execute 按名读全文进上下文。
    // push 必须先于 subagent 装配(tool_pool = tools.clone()):子 agent 定义
    // 的 tools 白名单写 load_skill 才可加载,默认只读集不含。
    let (skill_defs, skill_diagnostics) =
        rpi_core::discover_skill_defs(&cwd, subagent_home.as_deref());
    for diagnostic in &skill_diagnostics {
        eprintln!("[rpi][skills] {diagnostic}");
    }
    tools.push(Arc::new(rpi_core::LoadSkillTool::new(rpi_core::LoadSkillDeps {
        skills: skill_defs,
    })));
    let subagent_parent_cell: Arc<Mutex<Weak<rpi_agent::Agent>>> = Arc::new(Mutex::new(Weak::new()));
    // /model 同源的解析面(models.json + 内置 provider 默认表);父模型缺省
    // 继承自父会话快照,显式 `model` 参数走本解析器
    let subagent_resolver =
        rpi_core::create_model_resolver_from_config(Some(&cwd), subagent_home.as_deref());
    let factory_resolver =
        rpi_core::create_model_resolver_from_config(Some(&cwd), subagent_home.as_deref());
    let resolve_model: ModelResolverFn = Arc::new(move |spec: &str| subagent_resolver.resolve(spec));
    let factory_resolve_model: ModelResolverFn =
        Arc::new(move |spec: &str| factory_resolver.resolve(spec));
    // 检索忽略列表:Arc 共享给只读工具集与系统提示词规则(同一份配置)
    let search_ignore = Arc::new(search_ignore);
    let read_only_tool_set =
        rpi_tools::read_only_tools_with_limits(&cwd, tool_output_limits, search_ignore.clone());
    // 子会话落盘工厂:与主会话同一套 rpi-session 机制(消息/usage/快照 entry
    // 完全一致),文件名 `<时间>__<tag>__<id>.jsonl`(tag = run id / agent 名,
    // 落在 `<dir>/<项目前缀>/` 项目目录下);
    // 纯内存会话不落盘。contextSnapshot 开启时子会话同样记录真实上下文
    let child_store_factory: Option<rpi_core::ChildStoreFactory> = match &session_store {
        SessionStore::Memory => None,
        _ => {
            let dir: std::path::PathBuf = match &session_store {
                SessionStore::New { dir } => dir.clone(),
                SessionStore::Resume { file } => file
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_default(),
                SessionStore::Memory => unreachable!(),
            };
            let project = cwd.display().to_string();
            let snapshot_enabled = context_snapshot.unwrap_or(false);
            Some(Arc::new(move |tag: &str| {
                let manager: Arc<rpi_session::SessionManager> = rpi_session::create_session_in_dir(
                    &dir,
                    &project,
                    None,
                    Some(tag),
                )
                .map_err(|e| e.to_string())?
                .into();
                let sink: Arc<dyn rpi_core::SessionSink> = Arc::new(SessionManagerSink(
                    SessionManagerHolder::new(Some(manager.clone())),
                ));
                let mut stream_options = rpi_ai::StreamOptions::default();
                if snapshot_enabled {
                    let snapshot_manager = manager.clone();
                    stream_options.on_payload =
                        Some(Arc::new(move |body: &mut serde_json::Value| {
                            if let Err(error) = snapshot_manager.append_context_snapshot(body) {
                                eprintln!(
                                    "[rpi][subagent] context snapshot append failed: {error}"
                                );
                            }
                        }));
                }
                Ok(rpi_core::ChildStore {
                    sink,
                    stream_options,
                })
            }))
        }
    };
    let subagent_tool = Arc::new(rpi_core::SubagentTool::new(rpi_core::SubagentDeps {
        provider: provider.clone(),
        hooks: child_hooks.clone(),
        engine: engine.clone(),
        subscribers: subscribers.clone(),
        default_tools: read_only_tool_set.clone(),
        tool_pool: tools.clone(),
        resolve_model,
        parent: subagent_parent_cell.clone(),
        async_approval: subagent_async_approval,
        cwd: cwd.clone(),
        home: subagent_home.clone(),
        agent_defs,
        child_store_factory: child_store_factory.clone(),
    }));
    tools.push(subagent_tool.clone());

    let compaction_settings: rpi_session::CompactionSettings = compaction.clone().into();
    let compactor: Arc<dyn rpi_core::ContextCompactor> = Arc::new(SessionCompactor {
        manager_holder: manager_holder.clone(),
        provider: provider.clone(),
        settings: compaction_settings,
    });
    // 外置提示词 + 自定义规则:同一个文件(.rpi/system-prompt.md)一次读出
    let prompt_override = load_system_prompt_override();
    // 激活工具集:会话内切换过(ToolSetChange entry)则按记录恢复并校验;
    // 否则用调用方显式传入(main 从 settings `tools` 解析),未传 = 全部;
    // 配置了未知工具名直接报错(配置错误要显式暴露)
    // web 工具(rpi-web)随会话常驻激活,tools 数组从首请求起恒定(保缓存)
    let active_tool_names = match (&seed_active_tools, &active_tools) {
        (Some(names), _) | (None, Some(names)) => {
            Some(resolve_active_tools(names, &tools)?)
        }
        (None, None) => None,
    };

    let session = Arc::new(
        create_agent_session(AgentSessionConfig {
            provider: provider.clone(),
            model,
            hooks,
            ui,
            extensions: rpi_core::ExtensionRegistry::default(),
            tools: tools.clone(),
            active_tool_names,
            system_prompt: SystemPromptOptions {
                cwd: Some(cwd.display().to_string()),
                // 用户外置提示词(.rpi/system-prompt.md,项目→全局):块外内容
                // 替换身份句(preamble),<rules> 标记块内容追加进 <rules> 节;
                // <env>/<tools> 等动态节仍自动注入。装配期读一次,会话中途
                // 修改不生效。检索忽略规则拼在用户规则之后(同一份 searchIgnore)
                custom_prompt: prompt_override.0,
                custom_rules: merge_rules(prompt_override.1, search_ignore_rule(&search_ignore)),
                ..Default::default()
            },
            limits: rpi_agent::TurnLimits::default(),
            stream_options,
            session_sink: Some(Arc::new(SessionManagerSink(manager_holder.clone()))),
            seed_messages,
            compactor: Some(compactor),
            subscribers: Some(subscribers.clone()),
            permission: Some(engine.clone()),
        })
        .await
        .map_err(|e| e.to_string())?,
    );

    // 会话模式应用(13 文档 §8.1/§9.3):新会话文件落 ModeChange entry 自描述;
    // resume 只恢复不落盘(entry 已有)。模式节消息由 apply_mode append 进转录:
    // 新会话/老会话文件缺节点时追加,resume 历史已有相同节点则去重跳过
    session.set_default_mode(default_session_mode);
    if matches!(session_store, SessionStore::New { .. }) {
        session.set_mode(effective_mode).await.map_err(|e| e.to_string())?;
    } else {
        session
            .apply_mode_without_persist(effective_mode)
            .await
            .map_err(|e| e.to_string())?;
    }

    // resume 设置态恢复:直接回填 Agent 状态(不再落 thinking_level_change entry)
    if let Some(level) = parse_thinking_level(&seed_thinking_level) {
        session.agent().set_thinking_level(Some(level));
    }
    // 超长 tool result 裁剪上限(settings `toolResultMaxChars`;None = 默认值,
    // Agent 构造时已设,这里只处理显式覆盖)
    if let Some(max_chars) = tool_result_max_chars {
        session.agent().set_tool_result_max_chars(max_chars);
    }
    // 图片输入开关(settings `blockImages`;false = 允许图片进转录)
    session.agent().set_block_images(block_images);

    // T9:session 建好后回填共享 cell,PI_* 环境闭包此后可按需快照
    *session_cell.lock().unwrap() = Arc::downgrade(&session);
    // subagent:回填父会话弱引(模型继承 + supervisor 唤醒),启动空闲唤醒任务
    *subagent_parent_cell.lock().unwrap() = Arc::downgrade(session.agent());
    subagent_tool.spawn_supervisor();
    // web 扩展:回填 agent 弱引(后台完成通知 + 当前主模型)
    *web_agent_cell.lock().unwrap() = Arc::downgrade(session.agent());
    // bash 后台任务:回填 agent 弱引(完成通知经 follow_up 唤醒)
    *shell_bg_cell.lock().unwrap() = Arc::downgrade(session.agent());

    // 装配期诊断(编译期扩展 init 失败跳过等)
    for diagnostic in session.extension_diagnostics() {
        eprintln!(
            "[rpi][extension:{}] {}",
            diagnostic.extension, diagnostic.message
        );
    }

    // 观察类埋点:总线作为订阅者挂上两条事件汇(07 §8.3 接入方式)
    if !bus.is_empty() {
        session
            .agent()
            .subscribe(bus.clone() as Arc<dyn rpi_agent::Subscriber>);
        session.subscribe(bus.clone() as SessionSharedSubscriber);
    }

    Ok(BuiltSession {
        session,
        session_manager: Some(session_manager),
        manager_holder,
        compaction_config: compaction,
        rpc_approval,
        subagent_registry: Some(subagent_tool.registry()),
        subagent_factory: Some(Arc::new(rpi_core::SubagentSessionFactory {
            provider: provider.clone(),
            approval_ui: approval_ui_for_factory,
            engine: engine.clone(),
            sandbox: sandbox.clone(),
            rules: approval.clone(),
            cwd: cwd.clone(),
            sandbox_available,
            default_tools: read_only_tool_set,
            tool_pool: tools,
            resolve_model: factory_resolve_model,
            child_store_factory,
        })),
    })
}

/// print 模式便捷封装(旧行为,端到端测试复用):装配 + 跑一轮 prompt,
/// 流式增量直接打 stdout。`extra_subscriber` 供测试断言事件。
pub struct SessionRequest {
    pub provider: Arc<dyn rpi_ai::Provider>,
    pub model: rpi_ai::Model,
    pub prompt: String,
    pub extension_specs: Vec<McpServerSpec>,
    pub extra_subscriber: Option<SessionSharedSubscriber>,
    pub session_store: SessionStore,
    /// 运行期开关(main 从 settings 解析;测试用 Default)
    pub settings: SessionSettings,
    /// CLI 显式会话模式(--session-mode/--plan/--yolo;resume 时优先于 entry)
    pub session_mode_override: Option<SessionMode>,
}

pub async fn run_session(request: SessionRequest) -> Result<RunStop, String> {
    let built = build_session(BuildOptions {
        provider: request.provider,
        model: request.model,
        ui: Arc::new(NoopUi),
        extension_specs: request.extension_specs,
        spawn_hook: None,
        session_store: request.session_store,
        context_snapshot: Some(request.settings.context_snapshot),
        active_tools: request.settings.active_tools,
        search_ignore: request.settings.search_ignore,
        tool_result_max_chars: request.settings.tool_result_max_chars,
        block_images: request.settings.block_images,
        compaction: request.settings.compaction,
        session_mode: request.session_mode_override,
        default_session_mode: request.settings.session_mode,
        sandbox: request.settings.sandbox,
        approval: request.settings.approval,
        subagent_async_approval: request.settings.subagent_async_approval,
        approval_ui: Some(Arc::new(HeadlessApprovalUi {
            policy: request.settings.headless_approval,
        })),
        rpc_approval: None,
    })
    .await?;

    if let Some(subscriber) = request.extra_subscriber {
        built.session.subscribe(subscriber);
    }
    built
        .session
        .subscribe(Arc::new(PrintSubscriber::default()));

    let outcome = built
        .session
        .prompt(request.prompt)
        .await
        .map_err(|e| e.to_string())?;
    built.session.wait_idle().await;
    // print 模式跑完即退出:停掉仍在运行的后台 subagent(进程生命周期同父)
    if let Some(registry) = &built.subagent_registry {
        registry.abort_all();
    }
    Ok(outcome.stop())
}

/// rpi-core 的 `SessionSink` 适配器:把可选组件 rpi-session 注入业务核。
/// 拆卸 rpi-session 时删除本结构体即可,core 与其余 crate 不受影响。
/// transcript 统一:消息 / usage / 模型与思考级别变更全部落盘。
/// manager 经 holder 读取:/new 切换后写入跟随新会话。
struct SessionManagerSink(SessionManagerHolder);
#[async_trait]
impl SessionSink for SessionManagerSink {
    async fn append(&self, message: &rpi_agent::AgentMessage) -> Result<(), String> {
        let Some(manager) = self.0.get() else {
            return Ok(());
        };
        manager
            .append_message(message.clone())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn append_model_change(&self, provider: &str, model_id: &str) -> Result<(), String> {
        let Some(manager) = self.0.get() else {
            return Ok(());
        };
        manager
            .append_model_change(provider, model_id)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn append_thinking_level_change(&self, level: &str) -> Result<(), String> {
        let Some(manager) = self.0.get() else {
            return Ok(());
        };
        manager
            .append_thinking_level_change(level)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn append_tool_set_change(&self, tools: &[String]) -> Result<(), String> {
        let Some(manager) = self.0.get() else {
            return Ok(());
        };
        manager
            .append_tool_set_change(tools)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn append_mode_change(&self, mode: &str) -> Result<(), String> {
        let Some(manager) = self.0.get() else {
            return Ok(());
        };
        manager
            .append_mode_change(mode)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn append_usage(
        &self,
        kind: &str,
        provider: &str,
        model: &str,
        usage: rpi_ai::Usage,
    ) -> Result<(), String> {
        let Some(manager) = self.0.get() else {
            return Ok(());
        };
        manager
            .append_usage(kind, provider, model, usage, None)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// 统一 compaction(06 文档):provider 支撑的 Summarizer + SessionCompactor
// ---------------------------------------------------------------------------

/// 用当前会话的 provider/model 发摘要请求(SUMMARIZATION_PROMPT 管线)。
struct ProviderSummarizer {
    provider: Arc<dyn rpi_ai::Provider>,
    model: rpi_ai::Model,
}

#[async_trait]
impl rpi_session::Summarizer for ProviderSummarizer {
    async fn summarize(
        &self,
        request: &rpi_session::SummarizationRequest,
    ) -> Result<rpi_session::SummarizationResponse, String> {
        // 占位 user 承载序列化对话,摘要指令拼接其后
        let mut messages = request.messages.clone();
        match messages.last_mut() {
            Some(rpi_agent::AgentMessage::User { content, .. }) => {
                content.push_str("\n\n");
                content.push_str(&request.instruction);
            }
            _ => messages.push(rpi_agent::AgentMessage::user(&request.instruction)),
        }
        use rpi_agent::LoopHooks as _;
        let llm_messages = rpi_agent::PassthroughHooks.convert_to_llm(&messages);
        // 摘要请求挂专用 system 提示词(一次性异构请求,不参与正常对话的缓存前缀);
        // 无工具定义,防止摘要模型续写对话或调用工具
        let transcript = rpi_ai::normalize_context(rpi_ai::Context {
            system_prompt: Some(rpi_session::SUMMARIZATION_SYSTEM_PROMPT.to_string()),
            messages: llm_messages,
            tools: Vec::new(),
        });
        let mut stream = self
            .provider
            .stream(&self.model, transcript, Default::default())
            .await;
        use futures::StreamExt;
        let mut result: Option<rpi_session::SummarizationResponse> = None;
        while let Some(event) = stream.next().await {
            match event {
                rpi_ai::AssistantMessageEvent::Done(assistant) => {
                    result = Some(rpi_session::SummarizationResponse {
                        summary: assistant.text_content(),
                        stop_reason: assistant.stop_reason,
                        error_message: assistant.error_message.clone(),
                        usage: Some(assistant.usage),
                    });
                }
                rpi_ai::AssistantMessageEvent::Error(error) => {
                    return Err(error
                        .error_message
                        .unwrap_or_else(|| "summarizer error".into()));
                }
                _ => {}
            }
        }
        result.ok_or_else(|| "summarizer stream ended without a message".to_string())
    }
}

/// overflow 恢复与 manual /compact 共用的压缩实现:
/// 移除触发溢出的错误 assistant(ContextEdit)→ run_compaction →
/// Compaction entry 落盘(原始历史保留)→ Session projection 回填上下文。
/// manager 经 holder 读取:/new 切换后压缩跟随新会话。
struct SessionCompactor {
    manager_holder: SessionManagerHolder,
    provider: Arc<dyn rpi_ai::Provider>,
    settings: rpi_session::CompactionSettings,
}

#[async_trait]
impl rpi_core::ContextCompactor for SessionCompactor {
    async fn compact(&self, model: &rpi_ai::Model) -> Result<Vec<rpi_agent::AgentMessage>, String> {
        let manager = self
            .manager_holder
            .get()
            .ok_or_else(|| "无会话存储,无法压缩".to_string())?;
        // 末尾是 overflow 错误 assistant 时剔除出上下文(append-only ContextEdit;
        // 同时满足 continue_run"最后一条非 assistant"的前置条件)
        let entries = manager.branch_entries();
        if let Some(rpi_session::Entry::Message {
            id,
            message: rpi_agent::AgentMessage::Assistant(assistant),
            ..
        }) = entries.last()
        {
            if matches!(assistant.stop_reason, rpi_ai::StopReason::Error) {
                manager
                    .append_context_edit(id, None)
                    .map_err(|e| e.to_string())?;
            }
        }

        let entries = manager.branch_entries();
        let summarizer = ProviderSummarizer {
            provider: self.provider.clone(),
            model: model.clone(),
        };
        if let Some(outcome) =
            rpi_session::run_compaction(&entries, &self.settings, &summarizer).await?
        {
            manager
                .append_compaction(
                    outcome.summary,
                    outcome.first_kept_entry_id,
                    outcome.tokens_before,
                    Some(outcome.details),
                    outcome.usage,
                    false,
                )
                .map_err(|e| e.to_string())?;
        }
        // 压缩后上下文一律从 Session projection 重建(source of truth)
        Ok(manager.projection().messages)
    }

    /// 自动压缩阈值判定(06 文档 §3.1):Session projection 的 token 估算
    /// + should_compact(contextWindow - reserveTokens)。
    fn should_auto_compact(
        &self,
        model: &rpi_ai::Model,
        _messages: &[rpi_agent::AgentMessage],
    ) -> bool {
        let Some(manager) = self.manager_holder.get() else {
            return false;
        };
        let entries = manager.branch_entries();
        let projection = rpi_session::build_session_projection(&entries, None);
        let estimate =
            rpi_session::estimate_projected_context_tokens(&projection, &entries);
        rpi_session::should_compact(estimate.tokens, model.context_window, &self.settings)
    }
}

/// mode 侧订阅者(接缝 #5):流式增量直接打印,工具调用打印一行状态。
#[derive(Default)]
pub struct PrintSubscriber {
    streamed: Mutex<String>,
}

#[async_trait]
impl SessionSubscriber for PrintSubscriber {
    async fn on_session_event(&self, event: &AgentSessionEvent) {
        match event {
            AgentSessionEvent::Agent(agent_event) => match agent_event {
                AgentEvent::MessageDelta {
                    delta: rpi_agent::MessageDeltaPayload::Text { delta },
                } => {
                    // T2:print 模式只上文本增量(thinking/参数增量不上 stdout)
                    use std::io::Write;
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                    self.streamed.lock().unwrap().push_str(delta);
                }
                AgentEvent::MessageEnd { message } => {
                    if matches!(**message, rpi_agent::AgentMessage::Assistant(_)) {
                        println!();
                    }
                }
                AgentEvent::ToolExecutionStart { tool_name, .. } => {
                    println!("[tool] {tool_name} …");
                }
                _ => {}
            },
            AgentSessionEvent::AgentSettled => {}
            AgentSessionEvent::QueueUpdate { .. } => {}
            AgentSessionEvent::AutoRetryStart {
                attempt,
                delay_ms,
                reason,
            } => {
                eprintln!("[retry #{attempt} in {delay_ms}ms] {reason}");
            }
            AgentSessionEvent::AutoRetryEnd { .. } => {}
            AgentSessionEvent::ApprovalRequested { request } => {
                eprintln!(
                    "[approval] {} {}({})",
                    request.tool_name,
                    request.detail,
                    request.reason.message(rpi_core::SessionMode::Confirm)
                );
            }
            AgentSessionEvent::ApprovalResolved { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rpi_prefix_test_{}_{}",
                tag,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
        fn write_settings(&self, text: &str) {
            std::fs::create_dir_all(self.0.join(".rpi")).unwrap();
            std::fs::write(self.0.join(".rpi/settings.json"), text).unwrap();
        }
        fn write_file(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ---- contextSnapshot 开关(默认关) ----

    #[test]
    fn context_snapshot_unset_yields_none_and_configured_wins() {
        let project = TempDir::new("ctx_unset");
        assert_eq!(
            context_snapshot_enabled_from(Some(&project.0), None),
            None,
            "未配置 = None(默认关)"
        );
        project.write_settings(r#"{"contextSnapshot": true}"#);
        assert_eq!(context_snapshot_enabled_from(Some(&project.0), None), Some(true));
        project.write_settings(r#"{"contextSnapshot": false}"#);
        assert_eq!(
            context_snapshot_enabled_from(Some(&project.0), None),
            Some(false),
            "显式 false 也生效"
        );
    }

    #[test]
    fn context_snapshot_project_wins_over_global_and_bad_json_skipped() {
        let project = TempDir::new("ctx_prio");
        let global = TempDir::new("ctx_prio_global");
        global.write_settings(r#"{"contextSnapshot": true}"#);
        assert_eq!(
            context_snapshot_enabled_from(Some(&project.0), Some(&global.0)),
            Some(true),
            "项目未配置时回退全局"
        );
        project.write_settings("{not json");
        assert_eq!(
            context_snapshot_enabled_from(Some(&project.0), Some(&global.0)),
            Some(true),
            "项目坏 JSON 跳过,继续看全局"
        );
        project.write_settings(r#"{"contextSnapshot": false}"#);
        assert_eq!(
            context_snapshot_enabled_from(Some(&project.0), Some(&global.0)),
            Some(false),
            "项目显式 false 优先于全局 true"
        );
    }

    // ---- 系统提示词外置文件(.rpi/system-prompt.md) ----

    #[test]
    fn system_prompt_override_unset_yields_none() {
        let project = TempDir::new("sp_unset");
        assert_eq!(
            system_prompt_override_from(Some(&project.0), None),
            None,
            "未配置 = None(用内置默认提示词)"
        );
    }

    #[test]
    fn system_prompt_override_project_wins_global_fallback_blank_skipped() {
        let project = TempDir::new("sp_prio");
        let global = TempDir::new("sp_prio_global");
        global.write_file(".rpi/system-prompt.md", "global prompt");
        assert_eq!(
            system_prompt_override_from(Some(&project.0), Some(&global.0)),
            Some("global prompt".into()),
            "项目无文件时回退全局"
        );
        // 空白文件 = 未配置,继续看全局
        project.write_file(".rpi/system-prompt.md", "   \n\t");
        assert_eq!(
            system_prompt_override_from(Some(&project.0), Some(&global.0)),
            Some("global prompt".into()),
            "项目空白文件跳过"
        );
        project.write_file(".rpi/system-prompt.md", "project prompt\n");
        assert_eq!(
            system_prompt_override_from(Some(&project.0), Some(&global.0)),
            Some("project prompt".into()),
            "项目文件优先且去除首尾空白"
        );
    }

    #[test]
    fn command_prefix_from_project_settings() {
        let project = TempDir::new("project");
        project.write_settings(r#"{"commandPrefix": "timeout 300"}"#);
        assert_eq!(
            shell_command_prefix_from(Some(&project.0), None),
            Some("timeout 300".into()),
            "T10:settings commandPrefix 应进入装配通道"
        );
    }

    // ---- shell 运行时限 settings(bashTimeoutSecs / backgroundAfterSecs) ----

    #[test]
    fn shell_timeout_policy_unset_yields_defaults() {
        let project = TempDir::new("to_unset");
        assert_eq!(
            shell_timeout_policy_from(Some(&project.0), None),
            rpi_tools::ShellTimeoutPolicy {
                default_timeout_secs: 120,
                background_after_secs: 60
            },
            "未配置 = 默认 120s 超时 / 60s 转后台"
        );
    }

    #[test]
    fn shell_timeout_policy_project_overrides_global() {
        let project = TempDir::new("to_prio");
        let global = TempDir::new("to_prio_global");
        project.write_settings(r#"{"bashTimeoutSecs": 60}"#);
        let policy = shell_timeout_policy_from(Some(&project.0), Some(&global.0));
        assert_eq!(policy.default_timeout_secs, 60, "单键配置生效,其余保持默认");
        assert_eq!(policy.background_after_secs, 60);
        global.write_settings(r#"{"bashTimeoutSecs": 30, "backgroundAfterSecs": 45}"#);
        assert_eq!(
            shell_timeout_policy_from(Some(&project.0), Some(&global.0)),
            rpi_tools::ShellTimeoutPolicy {
                default_timeout_secs: 60,
                background_after_secs: 45
            },
            "首个含配置的 settings 生效:项目 bash 超时 + 全局后台阈值"
        );
        project.write_settings(r#"{"backgroundAfterSecs": 300}"#);
        let policy = shell_timeout_policy_from(Some(&project.0), Some(&global.0));
        assert_eq!(policy.default_timeout_secs, 30, "项目未含 bashTimeoutSecs 时回退全局");
        assert_eq!(policy.background_after_secs, 300, "项目 backgroundAfterSecs 覆盖全局");
    }

    #[test]
    fn project_settings_win_over_global_and_blank_is_skipped() {
        let project = TempDir::new("prio");
        let global = TempDir::new("prio_global");
        global.write_settings(r#"{"commandPrefix": "global-prefix"}"#);
        assert_eq!(
            shell_command_prefix_from(Some(&project.0), Some(&global.0)),
            Some("global-prefix".into()),
            "项目无 settings 时回退全局"
        );
        project.write_settings(r#"{"commandPrefix": "   "}"#);
        assert_eq!(
            shell_command_prefix_from(Some(&project.0), Some(&global.0)),
            Some("global-prefix".into()),
            "项目空白前缀跳过,继续看全局"
        );
        project.write_settings(r#"{"commandPrefix": "project-prefix"}"#);
        assert_eq!(
            shell_command_prefix_from(Some(&project.0), Some(&global.0)),
            Some("project-prefix".into()),
            "项目非空前缀优先于全局"
        );
    }

    #[test]
    fn missing_settings_yields_none() {
        let project = TempDir::new("none");
        assert_eq!(shell_command_prefix_from(Some(&project.0), None), None);
    }

    // ---- 激活工具集(settings `tools`) ----

    #[test]
    fn active_tools_unset_yields_none_and_project_wins() {
        let project = TempDir::new("tools_unset");
        assert_eq!(
            active_tool_names_from(Some(&project.0), None),
            None,
            "键未配置 = None(全部激活)"
        );
        // 配置了但清空后为空 = 显式不激活任何工具
        project.write_settings(r#"{"tools": []}"#);
        assert_eq!(
            active_tool_names_from(Some(&project.0), None),
            Some(Vec::<String>::new()),
            "空数组 = 不激活任何工具"
        );
        project.write_settings(r#"{"tools": ["", "  "]}"#);
        assert_eq!(
            active_tool_names_from(Some(&project.0), None),
            Some(Vec::<String>::new()),
            "全空白条目 = 显式不激活"
        );
        project.write_settings(r#"{"tools": [" bash "]}"#);
        assert_eq!(
            active_tool_names_from(Some(&project.0), None),
            Some(vec!["bash".to_string()]),
            "条目去首尾空白"
        );
    }

    #[test]
    fn active_tools_project_wins_over_global() {
        let project = TempDir::new("tools_prio");
        let global = TempDir::new("tools_prio_global");
        global.write_settings(r#"{"tools": ["read", "write"]}"#);
        assert_eq!(
            active_tool_names_from(Some(&project.0), Some(&global.0)),
            Some(vec!["read".to_string(), "write".to_string()]),
            "项目未配置时回退全局"
        );
        project.write_settings(r#"{"tools": ["bash"]}"#);
        assert_eq!(
            active_tool_names_from(Some(&project.0), Some(&global.0)),
            Some(vec!["bash".to_string()]),
            "项目配置优先于全局"
        );
    }

    #[test]
    fn resolve_active_tools_validates_names() {
        struct DummyTool(&'static str);
        #[async_trait]
        impl rpi_agent::Tool for DummyTool {
            fn name(&self) -> &str {
                self.0
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(
                &self,
                _call: rpi_agent::ToolCall,
                _cancel: tokio_util::sync::CancellationToken,
                _updater: &dyn rpi_agent::ToolUpdater,
            ) -> Result<rpi_agent::ToolOutput, rpi_agent::ToolError> {
                unimplemented!()
            }
        }
        let tools: Vec<Arc<dyn rpi_agent::Tool>> = vec![
            Arc::new(DummyTool("read")),
            Arc::new(DummyTool("bash")),
            Arc::new(DummyTool("edit")),
            Arc::new(DummyTool("write")),
        ];
        assert_eq!(
            resolve_active_tools(&["bash".to_string()], &tools).unwrap(),
            vec!["bash".to_string()]
        );
        let error = resolve_active_tools(&["bask".to_string()], &tools).unwrap_err();
        assert!(error.contains("未知工具 `bask`"), "{error}");
        assert!(error.contains("read, bash, edit, write"), "{error}");
    }

    // ---- transcript 统一:sink 扩展 entry + 统一 compaction ----

    #[tokio::test]
    async fn sink_persists_model_thinking_usage_entries() {
        let manager: Arc<rpi_session::SessionManager> =
            rpi_session::create_session(None::<String>).unwrap().into();
        let sink = SessionManagerSink(SessionManagerHolder::new(Some(manager.clone())));
        sink.append(&rpi_agent::AgentMessage::user("hi"))
            .await
            .unwrap();
        sink.append_model_change("openai", "gpt-5").await.unwrap();
        sink.append_thinking_level_change("high").await.unwrap();
        sink.append_usage("message", "openai", "gpt-5", rpi_ai::Usage::default())
            .await
            .unwrap();
        let kinds: Vec<String> = manager
            .entries()
            .iter()
            .map(|entry| match entry {
                rpi_session::Entry::Message { .. } => "message".into(),
                rpi_session::Entry::ModelChange { .. } => "model_change".into(),
                rpi_session::Entry::ThinkingLevelChange { .. } => "thinking_level".into(),
                rpi_session::Entry::Usage { .. } => "usage".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["message", "model_change", "thinking_level", "usage"]
        );
    }

    #[tokio::test]
    async fn session_compactor_appends_compaction_and_rebuilds_projection() {
        use rpi_agent::AgentMessage;

        let manager: Arc<rpi_session::SessionManager> =
            rpi_session::create_session(None::<String>).unwrap().into();
        // transcript:user → assistant(正常)→ assistant(overflow 错误)
        manager
            .append_message(AgentMessage::user("第一轮问题"))
            .unwrap();
        let m = rpi_ai::Model::minimal("m", "mock", "mock");
        let mut ok = rpi_ai::AssistantMessage::pending(&m);
        ok.content = vec![rpi_ai::ContentBlock::text("回答")];
        ok.stop_reason = rpi_ai::StopReason::Stop;
        ok.usage.total_tokens = 100;
        manager
            .append_message(AgentMessage::Assistant(Box::new(ok)))
            .unwrap();
        let mut error = rpi_ai::AssistantMessage::pending(&m);
        error.stop_reason = rpi_ai::StopReason::Error;
        error.error_message = Some("prompt is too long: 2000 tokens > 100 maximum".into());
        manager
            .append_message(AgentMessage::Assistant(Box::new(error)))
            .unwrap();

        let compactor = SessionCompactor {
            manager_holder: SessionManagerHolder::new(Some(manager.clone())),
            provider: rpi_ai::create_mock_provider("## Goal\n摘要内容"),
            settings: rpi_session::CompactionSettings {
                enabled: true,
                reserve_tokens: 0.0,
                keep_recent_tokens: 0,
            },
        };
        use rpi_core::ContextCompactor as _;
        let messages = compactor.compact(&m).await.unwrap();

        // 原始历史保留(append-only):3 条消息 entry 都在
        let entries = manager.entries();
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(e, rpi_session::Entry::Message { .. }))
                .count(),
            3,
            "原始消息不删除"
        );
        // Compaction + ContextEdit(剔除错误 assistant)entry 已落盘
        assert!(entries
            .iter()
            .any(|e| matches!(e, rpi_session::Entry::Compaction { .. })));
        assert!(entries
            .iter()
            .any(|e| matches!(e, rpi_session::Entry::ContextEdit { .. })));
        // 压缩后上下文来自 projection:摘要 + 保留的近期回复;
        // 错误 assistant(ContextEdit 剔除)与被摘要的 user 不再出现
        assert!(messages
            .iter()
            .any(|m| matches!(m, AgentMessage::CompactionSummary { .. })));
        assert!(!messages
            .iter()
            .any(|m| matches!(m, AgentMessage::User { .. })));
        assert!(
            !messages.iter().any(|m| matches!(m,
                AgentMessage::Assistant(a) if matches!(a.stop_reason, rpi_ai::StopReason::Error))),
            "触发溢出的错误 assistant 应被 ContextEdit 剔除"
        );
    }

    // ---- 权限系统 settings 新键(13 文档 §12) ----

    #[test]
    fn permission_settings_keys_parse_with_project_priority() {
        let project = TempDir::new("perm_unset");
        let files = read_settings_files(Some(&project.0), None);
        assert!(files.is_empty());

        // 新键整体反序列化
        project.write_settings(
            r#"{"sessionMode": "confirm", "headlessApproval": "auto-approve",
                "sandbox": {"writableRoots": ["../shared"], "networkAccess": true},
                "approval": {"allowCommands": ["make test"], "denyCommands": ["git push"]}}"#,
        );
        let files = read_settings_files(Some(&project.0), None);
        assert_eq!(files.len(), 1);
        assert_eq!(parse_session_mode(files[0].session_mode.as_ref()), SessionMode::Confirm);
        assert_eq!(
            parse_headless_approval(files[0].headless_approval.as_ref()),
            HeadlessApproval::AutoApprove
        );
        let sandbox = files[0].sandbox.clone().unwrap();
        assert_eq!(sandbox.writable_roots, vec!["../shared".to_string()]);
        assert!(sandbox.network_access);
        let approval = files[0].approval.clone().unwrap();
        assert_eq!(approval.allow_commands.unwrap(), vec!["make test".to_string()]);
        assert_eq!(approval.deny_commands.unwrap(), vec!["git push".to_string()]);

        // 未知模式回退 Plan;未配置 headless 回退 Deny
        project.write_settings(r#"{"sessionMode": "yolo-mode"}"#);
        let files = read_settings_files(Some(&project.0), None);
        assert_eq!(parse_session_mode(files[0].session_mode.as_ref()), SessionMode::Plan);
        assert_eq!(
            parse_headless_approval(files[0].headless_approval.as_ref()),
            HeadlessApproval::Deny
        );

        // 项目优先:项目未配置时回退全局
        let project2 = TempDir::new("perm_prio");
        let global = TempDir::new("perm_prio_global");
        global.write_settings(r#"{"sessionMode": "full-access"}"#);
        let files = read_settings_files(Some(&project2.0), Some(&global.0));
        assert_eq!(parse_session_mode(files[0].session_mode.as_ref()), SessionMode::FullAccess);
        project2.write_settings(r#"{"sessionMode": "plan"}"#);
        let files = read_settings_files(Some(&project2.0), Some(&global.0));
        assert_eq!(parse_session_mode(files[0].session_mode.as_ref()), SessionMode::Plan);
    }

    // ---- 检索忽略列表(settings `searchIgnore`) ----

    #[test]
    fn search_ignore_unset_yields_builtin_and_project_wins() {
        let project = TempDir::new("si_unset");
        // 未配置 = 内置默认表(含 node_modules)
        let ignore = search_ignore_from(Some(&project.0), None);
        assert!(
            ignore.patterns().iter().any(|p| p == "node_modules"),
            "未配置 = 内置默认表"
        );
        // 空数组 = 显式关闭过滤
        project.write_settings(r#"{"searchIgnore": []}"#);
        assert!(search_ignore_from(Some(&project.0), None).is_empty());
        // 自定义列表整体覆盖默认表;尾 `/` 剥离
        project.write_settings(r#"{"searchIgnore": ["generated", "node_modules/"]}"#);
        let ignore = search_ignore_from(Some(&project.0), None);
        assert_eq!(
            ignore.patterns().iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["generated", "node_modules"]
        );
        // 项目优先:项目配置生效,未配置时回退全局
        let global = TempDir::new("si_global");
        global.write_settings(r#"{"searchIgnore": ["vendor"]}"#);
        let ignore = search_ignore_from(Some(&project.0), Some(&global.0));
        assert_eq!(ignore.patterns().first().map(String::as_str), Some("generated"));
        let ignore = search_ignore_from(Some(&global.0), None);
        assert_eq!(ignore.patterns().iter().map(String::as_str).collect::<Vec<_>>(), vec!["vendor"]);
    }

    #[test]
    fn search_ignore_invalid_pattern_falls_back_to_builtin() {
        let project = TempDir::new("si_bad");
        project.write_settings(r#"{"searchIgnore": ["a[.ts"]}"#);
        // 非法 glob 打诊断回退内置默认(配置错误显式暴露,不阻断会话)
        let ignore = search_ignore_from(Some(&project.0), None);
        assert!(ignore.patterns().iter().any(|p| p == "node_modules"));
    }

    #[test]
    fn search_ignore_rule_merges_after_user_rules() {
        // 空列表 = 关闭过滤 = 无规则
        let empty = rpi_tools::SearchIgnore::from_patterns(Vec::<String>::new()).unwrap();
        assert_eq!(search_ignore_rule(&empty), None);
        // 有列表 = 一条规则,条目回显其中
        let ignore = rpi_tools::SearchIgnore::from_patterns(["node_modules", "dist"]).unwrap();
        let rule = search_ignore_rule(&ignore).unwrap();
        assert!(rule.contains("node_modules, dist"), "{rule}");
        // 用户外置规则在前,装配级规则在后
        let merged = merge_rules(Some("user rule".into()), Some(rule));
        assert!(merged.unwrap().starts_with("user rule\nWhen searching"));
    }

    // ---- <rules> 自定义规则(system-prompt.md 的 <rules> 标记块) ----

    #[test]
    fn system_prompt_override_splits_rules_block() {
        let project = TempDir::new("sp_rules");
        // 无标记块:全文是身份句(向后兼容)
        project.write_file(".rpi/system-prompt.md", "You are my agent.\n");
        let text = system_prompt_override_from(Some(&project.0), None).unwrap();
        let (prompt, rules) = rpi_core::split_prompt_and_rules(&text);
        assert_eq!(prompt.as_deref(), Some("You are my agent."));
        assert_eq!(rules, None);
        // <rules> 标记块:块外身份句 + 块内追加规则
        project.write_file(
            ".rpi/system-prompt.md",
            "You are my agent.\n\n<rules>\nAlways run cargo clippy before commit.\n</rules>\n",
        );
        let text = system_prompt_override_from(Some(&project.0), None).unwrap();
        let (prompt, rules) = rpi_core::split_prompt_and_rules(&text);
        assert_eq!(prompt.as_deref(), Some("You are my agent."));
        assert_eq!(rules.as_deref(), Some("Always run cargo clippy before commit."));
        // 项目文件整体优先于全局(与身份句同一 precedence:文件级)
        let global = TempDir::new("sp_rules_global");
        global.write_file(".rpi/system-prompt.md", "global prompt <rules>G</rules>");
        let text = system_prompt_override_from(Some(&project.0), Some(&global.0)).unwrap();
        let (prompt, rules) = rpi_core::split_prompt_and_rules(&text);
        assert_eq!(prompt.as_deref(), Some("You are my agent."));
        assert_eq!(rules.as_deref(), Some("Always run cargo clippy before commit."));
    }

    // ---- ProviderSummarizer:摘要请求挂专用 system 提示词 ----

    struct RecordingProvider {
        captured: std::sync::Mutex<Vec<Vec<rpi_ai::Message>>>,
    }

    #[async_trait::async_trait]
    impl rpi_ai::Provider for RecordingProvider {
        async fn stream(
            &self,
            model: &rpi_ai::Model,
            ctx: rpi_ai::TranscriptContext,
            _opts: rpi_ai::StreamOptions,
        ) -> rpi_ai::AssistantMessageEventStream {
            self.captured.lock().unwrap().push(ctx.messages);
            let message = rpi_ai::assistant_message(
                model,
                vec![rpi_ai::ContentBlock::text("summary text")],
                rpi_ai::StopReason::Stop,
            );
            Box::pin(futures::stream::iter(vec![
                rpi_ai::AssistantMessageEvent::Start,
                rpi_ai::AssistantMessageEvent::Done(Box::new(message)),
            ]))
        }
    }

    #[tokio::test]
    async fn provider_summarizer_prepends_summarization_system_prompt() {
        use rpi_session::Summarizer as _;

        let provider = std::sync::Arc::new(RecordingProvider {
            captured: std::sync::Mutex::new(Vec::new()),
        });
        let summarizer = ProviderSummarizer {
            provider: provider.clone(),
            model: rpi_ai::Model::minimal("m1", "mock", "mock"),
        };
        let response = summarizer
            .summarize(&rpi_session::SummarizationRequest {
                messages: vec![rpi_agent::AgentMessage::user("conversation body")],
                instruction: "summarize it".into(),
            })
            .await
            .unwrap();
        assert_eq!(response.summary, "summary text");

        let captured = provider.captured.lock().unwrap();
        let messages = &captured[0];
        assert_eq!(messages.len(), 2, "system + 单条 user");
        match &messages[0] {
            rpi_ai::Message::System { content, .. } => assert!(
                content.contains("context summarization assistant"),
                "首条应为摘要 system 提示词: {content}"
            ),
            other => panic!("首条消息应为 System: {other:?}"),
        }
        match &messages[1] {
            rpi_ai::Message::User {
                content: rpi_ai::UserContent::Text(text),
                ..
            } => assert_eq!(text, "conversation body\n\nsummarize it"),
            other => panic!("第二条应为承载对话+指令的 user 消息: {other:?}"),
        }
    }
}
