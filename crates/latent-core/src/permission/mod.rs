//! 权限系统与计划模式(13 文档 §3/§6):会话模式、风险分类、判定引擎、
//! 审批钩子与审批 UI 接缝。全部为纯业务概念,挂接在现有接缝上:
//! `before_tool_call` 唯一拦截点 + `ShellSpawnHook` 沙箱包装点 +
//! `set_active_tools_by_name` 工具收紧点。

pub mod engine;
pub mod hooks;
pub mod interp;
pub mod shell;
pub mod types;

pub use engine::{ApprovalRules, PermissionEngine};
pub use hooks::{ApprovalHooks, ApprovalUi, HeadlessApproval, HeadlessApprovalUi};
pub use types::{mode_section,
    classify_tool, mode_baseline_tools, normalize_command, policy_for_mode, ApprovalDecision,
    ApprovalKey, ApprovalReason, ApprovalRequest, SessionMode, SandboxConfig, SandboxPolicy,
    ToolRiskClass, Verdict, PLAN_MODE_ENTER_SECTION, PLAN_MODE_EXIT_SECTION,
};

use std::path::PathBuf;

/// shell 命令的展示摘要(审批 UI / 拒绝原因共用)。
pub(crate) fn shell_detail(command: &str) -> String {
    command.trim().to_string()
}/// 可写根判定(13 文档 §6.1):相对路径基于 cwd 解析并 canonicalize;
/// 解析失败(参数缺失/不存在)按"越界"保守处理。
pub(crate) fn resolve_path_in_roots(raw: &str, cwd: &std::path::Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let path = std::path::Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let canonical = absolute.canonicalize().ok()?;
    Some(canonical).filter(|canonical| roots.iter().any(|root| canonical.starts_with(root)))
}
