//! 核心类型(13 文档 §3.2/§5):会话模式、工具风险分类、判定结论、审批
//! 载荷与决策。`SandboxPolicy` 定义在本 crate(而非 latent-sandbox):latent-core
//! 不依赖 latent-sandbox(可选组件判据),装配层负责映射到平台后端。

use serde::{Deserialize, Serialize};

/// 会话模式(用户可见的安全档位)。新会话默认 Plan。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    /// 只读 + 沙箱 + 列计划(默认)
    #[default]
    Plan,
    /// 全工具,变更前确认
    Confirm,
    /// 全自动,无审批无沙箱
    FullAccess,
}

impl SessionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionMode::Plan => "plan",
            SessionMode::Confirm => "confirm",
            SessionMode::FullAccess => "full-access",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "plan" => Some(SessionMode::Plan),
            "confirm" => Some(SessionMode::Confirm),
            "full-access" | "fullaccess" | "full_access" => Some(SessionMode::FullAccess),
            _ => None,
        }
    }

    /// 状态栏/展示标记。
    pub fn label(&self) -> &'static str {
        match self {
            SessionMode::Plan => "plan",
            SessionMode::Confirm => "confirm",
            SessionMode::FullAccess => "full-access",
        }
    }

    /// 循环切换(Shift+Tab):Plan → Confirm → FullAccess → Plan。
    pub fn next(self) -> Self {
        match self {
            SessionMode::Plan => SessionMode::Confirm,
            SessionMode::Confirm => SessionMode::FullAccess,
            SessionMode::FullAccess => SessionMode::Plan,
        }
    }
}

/// 工具风险分类。内置工具静态映射;MCP 扩展工具一律 External。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRiskClass {
    ReadOnly,
    FileWrite,
    Shell,
    External,
}

/// 内置工具名 → 风险类;未知名字(MCP 扩展)一律 External。
pub fn classify_tool(name: &str) -> ToolRiskClass {
    match name {
        "read" | "grep" | "find" | "ls" => ToolRiskClass::ReadOnly,
        // subagent 引擎封壳:派发本身无副作用,且子 agent 的每次工具调用仍过
        // 同一权限引擎(Plan 模式下子调用照常被只读约束),故按只读类放行 ——
        // 否则 Plan 模式会把它当 External 工具整体拒绝
        "subagent" => ToolRiskClass::ReadOnly,
        // web 工具(latent-web):搜索/抓取/检索无本地写副作用(缓存写专属
        // 目录 ~/.latent/web-search-cache/),出网能力由沙箱 network_access 与
        // web-search.json 的用户配置约束;fetch/source_check 的目标 URL 过
        // SSRF 校验(私网封锁),故整体按只读类放行(取舍:确认于 web 设计)
        "web_search" | "get_search_content" | "fetch_content" | "source_check" => {
            ToolRiskClass::ReadOnly
        }
        // skills(latent-core):按名读 SKILL.md 全文进上下文,纯读无写副作用
        "load_skill" => ToolRiskClass::ReadOnly,
        "edit" | "write" => ToolRiskClass::FileWrite,
        "bash" | "powershell" => ToolRiskClass::Shell,
        _ => ToolRiskClass::External,
    }
}

/// 单次工具调用的判定结论。
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// 放行(hook 返回 None)
    Allow,
    /// 需人工审批(载荷展示给用户)
    Ask(ApprovalRequest),
    /// 直接拒绝(hook 返回 ToolBlock{block:true, reason})
    Deny(String),
}

/// 审批请求载荷(UI 渲染依据)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub risk: ToolRiskClass,
    /// 触发原因(决定 UI 文案与可用选项)
    pub reason: ApprovalReason,
    /// 展示文本:shell 命令原文 / 将写入的文件路径 / 工具参数摘要
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReason {
    /// 文件写入(Confirm,可写根内)
    FileWrite,
    /// 文件写入越出可写根(Confirm)
    OutsideWorkspace,
    /// 需审批的 shell 命令(Confirm;规则未放行)
    ShellCommand,
    /// MCP 扩展工具(Confirm)
    ExternalTool,
    /// 沙箱不可用,升级为无沙箱执行需批准(Confirm)
    NoSandboxEscalation,
}

impl ApprovalReason {
    /// 审批弹窗的原因文案。
    pub fn message(&self, mode: SessionMode) -> String {
        let mode_label = mode.label();
        match self {
            ApprovalReason::FileWrite => {
                format!("文件写入需要审批({mode_label} 模式)")
            }
            ApprovalReason::OutsideWorkspace => format!(
                "文件写入越出可写根({mode_label} 模式;可用 --sandbox-write 追加可写根)"
            ),
            ApprovalReason::ShellCommand => format!("需要审批的 shell 命令({mode_label} 模式)"),
            ApprovalReason::ExternalTool => format!("MCP 扩展工具({mode_label} 模式)"),
            ApprovalReason::NoSandboxEscalation => {
                format!("无法沙箱隔离,将以无沙箱方式执行({mode_label} 模式)")
            }
        }
    }
}

/// 用户决策(对标 Codex ReviewDecision 的 latent 子集)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// 批准这一次
    Approve,
    /// 本会话内同类请求(同 ApprovalKey)自动批准
    ApproveForSession,
    /// 拒绝本次,会话继续(拒绝原因作为错误 tool result 给模型)
    Deny,
    /// 拒绝并终止本次 run(block + terminate)
    Abort,
}

/// 审批缓存键(对标 Codex ApprovalCacheKey):决策可复用的最小单元。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ApprovalKey {
    /// 规范化后的整条命令(空白折叠)
    ShellCommand(String),
    /// canonical 文件路径(edit/write 各自独立键)
    FilePath(std::path::PathBuf),
    /// 扩展工具名
    ExternalTool(String),
}

impl ApprovalRequest {
    /// 本请求对应的缓存键(无键 = 不可缓存,如路径解析失败的写入)。
    pub fn cache_key(&self) -> Option<ApprovalKey> {
        match self.risk {
            ToolRiskClass::Shell => Some(ApprovalKey::ShellCommand(normalize_command(&self.detail))),
            ToolRiskClass::FileWrite => self
                .args
                .get("path")
                .and_then(|path| path.as_str())
                .map(|path| ApprovalKey::FilePath(std::path::PathBuf::from(path))),
            ToolRiskClass::External => Some(ApprovalKey::ExternalTool(self.tool_name.clone())),
            ToolRiskClass::ReadOnly => None,
        }
    }
}

/// 命令规范化:折叠连续空白(判定与缓存共用)。
pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 沙箱配置(settings `sandbox` 节;Confirm 模式的 WorkspaceWrite 细节)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SandboxConfig {
    /// 额外可写根
    pub writable_roots: Vec<String>,
    /// Confirm 模式放行沙箱内网络访问
    pub network_access: bool,
}

/// 会话模式 → 沙箱策略(13 文档 §2.1 收敛映射)。Plan 的 ReadOnly 沙箱
/// 放开网络:Plan 模式允许联网查询命令(curl/ping/dns 查询),只读约束
/// 只针对文件系统。
pub fn policy_for_mode(mode: SessionMode, sandbox: &SandboxConfig) -> SandboxPolicy {
    match mode {
        SessionMode::Plan => SandboxPolicy::ReadOnly {
            network_access: true,
        },
        SessionMode::Confirm => SandboxPolicy::WorkspaceWrite {
            writable_roots: sandbox.writable_roots.clone(),
            network_access: sandbox.network_access,
        },
        SessionMode::FullAccess => SandboxPolicy::DangerFullAccess,
    }
}

/// 沙箱策略(latent-core 自有类型;装配层映射到 latent-sandbox 的平台后端)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SandboxPolicy {
    /// 全盘只读;network_access 控制网络(Plan 模式联网查询放行 = true)
    ReadOnly {
        #[serde(default)]
        network_access: bool,
    },
    WorkspaceWrite {
        #[serde(default)]
        writable_roots: Vec<String>,
        #[serde(default)]
        network_access: bool,
    },
    DangerFullAccess,
}

/// 三模式的基线激活集(13 文档 §5)。无沙箱平台 Plan 的 bash 仍在激活集:
/// 没有 OS 层兜底时,只读判定就是最后保证 —— 判定通过即执行,不通过拒绝
/// (降级矩阵 §7.5)。
pub fn mode_baseline_tools(mode: SessionMode, _sandbox_available: bool) -> Vec<String> {
    match mode {
        SessionMode::Plan => vec![
            "read".into(),
            "grep".into(),
            "find".into(),
            "ls".into(),
            "bash".into(),
        ],
        SessionMode::Confirm | SessionMode::FullAccess => vec![
            "read".into(),
            "bash".into(),
            "powershell".into(),
            "edit".into(),
            "write".into(),
            "grep".into(),
            "find".into(),
            "ls".into(),
        ],
    }
}

/// 模式 → 持久模式节文本(`ModeSection` 消息,模式切换时 append 进转录,
/// 位置永久固定;不并入系统提示词)。
/// Plan 进/出各一句:进 Plan 提示只读+产出计划;切出后的 Confirm/FullAccess
/// 沿用退出句(后半句"可写"恒为真,提醒模型可以动手)。
pub const PLAN_MODE_ENTER_SECTION: &str = "You are entering Plan mode: files cannot be created, modified, or deleted, and commands that clearly write files or system state are blocked; read-only inspection and network queries (curl/ping/DNS lookups) are allowed. Inspect the codebase and produce an implementation plan. Read the project's entry points and README first, then read only what the plan needs; batch independent lookups into single turns. If something the user referenced (a file, a doc) cannot be found, ask rather than searching at length. A clarifying question is a valid intermediate deliverable: if the request admits two plausible readings, present them with your recommendation and wait instead of building the plan on a guess. State the final plan as concrete file-level changes.";

pub const PLAN_MODE_EXIT_SECTION: &str = "You are exiting Plan mode: you may now create, modify, and delete files; implement the agreed plan. Work from the plan agreed in Plan mode; re-inspect a file only right before modifying it.";

pub fn mode_section(mode: SessionMode) -> Option<String> {
    match mode {
        SessionMode::Plan => Some(PLAN_MODE_ENTER_SECTION.to_string()),
        SessionMode::Confirm | SessionMode::FullAccess => {
            Some(PLAN_MODE_EXIT_SECTION.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_mode_parse_and_str_roundtrip() {
        for name in ["plan", "confirm", "full-access"] {
            assert_eq!(SessionMode::parse(name).unwrap().as_str(), name);
        }
        assert_eq!(SessionMode::parse("FULL-ACCESS"), Some(SessionMode::FullAccess));
        assert_eq!(SessionMode::parse("yolo"), None);
        assert_eq!(SessionMode::default(), SessionMode::Plan);
    }

    #[test]
    fn mode_cycles_for_shift_tab() {
        assert_eq!(SessionMode::Plan.next(), SessionMode::Confirm);
        assert_eq!(SessionMode::Confirm.next(), SessionMode::FullAccess);
        assert_eq!(SessionMode::FullAccess.next(), SessionMode::Plan);
    }

    #[test]
    fn classify_tool_maps_builtin_names() {
        assert_eq!(classify_tool("read"), ToolRiskClass::ReadOnly);
        assert_eq!(classify_tool("ls"), ToolRiskClass::ReadOnly);
        assert_eq!(classify_tool("write"), ToolRiskClass::FileWrite);
        assert_eq!(classify_tool("edit"), ToolRiskClass::FileWrite);
        assert_eq!(classify_tool("bash"), ToolRiskClass::Shell);
        assert_eq!(classify_tool("powershell"), ToolRiskClass::Shell);
        assert_eq!(classify_tool("subagent"), ToolRiskClass::ReadOnly);
        assert_eq!(classify_tool("load_skill"), ToolRiskClass::ReadOnly);
        assert_eq!(classify_tool("mcp__x__y"), ToolRiskClass::External);
        assert_eq!(classify_tool("unknown"), ToolRiskClass::External);
    }

    #[test]
    fn baseline_tools_drop_write_tools_in_plan() {
        let plan = mode_baseline_tools(SessionMode::Plan, true);
        assert!(plan.contains(&"bash".to_string()));
        assert!(!plan.contains(&"edit".to_string()));
        assert!(!plan.contains(&"powershell".to_string()));
        // 无沙箱平台:bash 仍在基线,由只读判定兜底(降级矩阵 §7.5)
        let degraded = mode_baseline_tools(SessionMode::Plan, false);
        assert!(degraded.contains(&"bash".to_string()));
        let confirm = mode_baseline_tools(SessionMode::Confirm, true);
        assert!(confirm.contains(&"powershell".to_string()));
        assert!(confirm.contains(&"edit".to_string()));
    }

    #[test]
    fn policy_mapping_follows_mode() {
        let sandbox = SandboxConfig {
            writable_roots: vec!["../shared".into()],
            network_access: true,
        };
        assert_eq!(
            policy_for_mode(SessionMode::Plan, &sandbox),
            SandboxPolicy::ReadOnly {
                network_access: true
            }
        );
        assert_eq!(
            policy_for_mode(SessionMode::Confirm, &sandbox),
            SandboxPolicy::WorkspaceWrite {
                writable_roots: vec!["../shared".into()],
                network_access: true,
            }
        );
        assert_eq!(
            policy_for_mode(SessionMode::FullAccess, &sandbox),
            SandboxPolicy::DangerFullAccess
        );
    }

    #[test]
    fn cache_key_derivation() {
        let request = ApprovalRequest {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"command": "make test"}),
            risk: ToolRiskClass::Shell,
            reason: ApprovalReason::ShellCommand,
            detail: "make  test".into(),
        };
        assert_eq!(
            request.cache_key(),
            Some(ApprovalKey::ShellCommand("make test".into()))
        );
        let external = ApprovalRequest {
            tool_call_id: "t2".into(),
            tool_name: "mcp__s__tool".into(),
            args: serde_json::json!({}),
            risk: ToolRiskClass::External,
            reason: ApprovalReason::ExternalTool,
            detail: String::new(),
        };
        assert_eq!(
            external.cache_key(),
            Some(ApprovalKey::ExternalTool("mcp__s__tool".into()))
        );
    }
}
