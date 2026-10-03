//! 权限判定引擎(13 文档 §6.3):纯函数 + 会话级批准缓存 + 当前模式。
//! 判定发生在**工具执行前**(问不问/拒不拒);沙箱包装(spawn 前)由
//! ShellSpawnHook 负责,职责不重叠。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

use rpi_agent::ToolCallCtx;

use crate::permission::shell::{classify_shell_command, is_readonly_command_with_rules, ShellSafety};
use crate::permission::shell_detail;
use crate::permission::types::{
    policy_for_mode, ApprovalKey, ApprovalReason, ApprovalRequest, SandboxConfig, SandboxPolicy,
    SessionMode, ToolRiskClass, Verdict,
};

/// 审批规则(settings `approval` 节;Confirm 模式下免审/必禁)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalRules {
    /// 前缀匹配,追加到内置只读表(免审)
    pub allow_commands: Vec<String>,
    /// 命中即 Deny,优先于 allow
    pub deny_commands: Vec<String>,
}

pub struct PermissionEngine {
    /// 当前模式(运行期可切换;与 set_mode 同锁)
    mode: RwLock<SessionMode>,
    /// Confirm 模式的 WorkspaceWrite 细节(Plan 固定 ReadOnly+联网,FullAccess 固定关)
    sandbox: SandboxConfig,
    rules: ApprovalRules,
    /// 会话级批准缓存(ApproveForSession 写入;set_mode 清空)
    cache: Mutex<HashSet<ApprovalKey>>,
    cwd: PathBuf,
    /// 平台沙箱可用性(装配期探测;false = Confirm 的命令审批升级为
    /// NoSandboxEscalation)
    sandbox_available: bool,
}

impl PermissionEngine {
    pub fn new(
        mode: SessionMode,
        sandbox: SandboxConfig,
        rules: ApprovalRules,
        cwd: PathBuf,
        sandbox_available: bool,
    ) -> Self {
        PermissionEngine {
            mode: RwLock::new(mode),
            sandbox,
            rules,
            cache: Mutex::new(HashSet::new()),
            cwd,
            sandbox_available,
        }
    }

    pub fn mode(&self) -> SessionMode {
        *self.mode.read().unwrap()
    }

    /// 切换模式并清空审批缓存(13 文档 §6.3)。
    pub fn set_mode(&self, mode: SessionMode) {
        *self.mode.write().unwrap() = mode;
        self.cache.lock().unwrap().clear();
    }

    /// 当前模式 → 沙箱策略(ShellSpawnHook 包装用)。
    pub fn policy(&self) -> SandboxPolicy {
        policy_for_mode(self.mode(), &self.sandbox)
    }

    /// 平台沙箱是否可用(装配期探测结果)。
    pub fn sandbox_available(&self) -> bool {
        self.sandbox_available
    }

    /// 当前生效的可写根(Confirm:cwd + 系统临时目录 + 配置;其余模式为空)。
    pub fn writable_roots(&self) -> Vec<PathBuf> {
        match self.policy() {
            SandboxPolicy::WorkspaceWrite { .. } => {
                let mut roots = vec![self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone())];
                if let Some(tmp) = std::env::var_os("TMPDIR") {
                    let path = PathBuf::from(tmp);
                    if path.is_dir() {
                        roots.push(path);
                    }
                }
                let fallback = PathBuf::from("/tmp");
                if fallback.is_dir() {
                    roots.push(fallback);
                }
                for root in &self.sandbox.writable_roots {
                    let path = PathBuf::from(root);
                    roots.push(path.canonicalize().unwrap_or(path));
                }
                roots
            }
            _ => Vec::new(),
        }
    }

    /// 会话级批准(ApproveForSession):键入缓存。
    pub fn approve_for_session(&self, request: &ApprovalRequest) {
        if let Some(key) = request.cache_key() {
            self.cache.lock().unwrap().insert(key);
        }
    }

    /// 主判定(13 文档 §6.3,每步短路)。
    pub fn evaluate(&self, ctx: &ToolCallCtx, risk: ToolRiskClass) -> Verdict {
        let mode = self.mode();
        if mode == SessionMode::FullAccess {
            return Verdict::Allow;
        }
        if self.cache_hit(ctx, risk) {
            return Verdict::Allow;
        }
        match risk {
            ToolRiskClass::ReadOnly => Verdict::Allow,
            ToolRiskClass::FileWrite => match mode {
                SessionMode::Plan => Verdict::Deny(
                    "Plan mode is read-only. Finish the plan and exit Plan mode before modifying files"
                        .into(),
                ),
                SessionMode::Confirm => {
                    let path = ctx.args.get("path").and_then(|path| path.as_str());
                    let inside = path
                        .map(|raw| {
                            crate::permission::resolve_path_in_roots(
                                raw,
                                &self.cwd,
                                &self.writable_roots(),
                            )
                            .is_some()
                        })
                        // 路径解析失败按越界保守处理
                        .unwrap_or(false);
                    if inside {
                        Verdict::Ask(self.request(ctx, risk, ApprovalReason::FileWrite))
                    } else {
                        Verdict::Ask(self.request(ctx, risk, ApprovalReason::OutsideWorkspace))
                    }
                }
                SessionMode::FullAccess => Verdict::Allow,
            },
            ToolRiskClass::Shell => {
                let command = ctx
                    .args
                    .get("command")
                    .and_then(|command| command.as_str())
                    .unwrap_or("");
                match mode {
                    SessionMode::Plan => {
                        // 三态判定:只读与联网查询直接放行(只读另有 OS 层
                        // ReadOnly 沙箱兜底);明确写(写前缀/落盘重定向)
                        // 即使有沙箱也不放行;未知命令在沙箱可用时放行、由
                        // 沙箱裁决 —— 无沙箱平台没有 OS 层兜底,保守拒绝。
                        match classify_shell_command(
                            command,
                            &self.rules.allow_commands,
                            &self.rules.deny_commands,
                        ) {
                            ShellSafety::ReadOnly | ShellSafety::NetworkQuery => Verdict::Allow,
                            ShellSafety::Write => Verdict::Deny(format!(
                                "Plan mode blocks commands that modify files or system state: {}",
                                shell_detail(command)
                            )),
                            ShellSafety::Unknown if self.sandbox_available => Verdict::Allow,
                            ShellSafety::Unknown => Verdict::Deny(format!(
                                "Plan mode allows read-only commands only: {}",
                                shell_detail(command)
                            )),
                        }
                    }
                    SessionMode::Confirm => {
                        if is_readonly_command_with_rules(
                            command,
                            &self.rules.allow_commands,
                            &self.rules.deny_commands,
                        ) {
                            return Verdict::Allow;
                        }
                        if self
                            .rules
                            .deny_commands
                            .iter()
                            .any(|rule| {
                                crate::permission::shell::prefix_matches(
                                    &crate::permission::types::normalize_command(command),
                                    &crate::permission::types::normalize_command(rule),
                                )
                            })
                        {
                            return Verdict::Deny(format!(
                                "Command matched a deny rule: {}",
                                shell_detail(command)
                            ));
                        }
                        let reason = if self.sandbox_available {
                            ApprovalReason::ShellCommand
                        } else {
                            ApprovalReason::NoSandboxEscalation
                        };
                        Verdict::Ask(self.request(ctx, risk, reason))
                    }
                    SessionMode::FullAccess => Verdict::Allow,
                }
            }
            ToolRiskClass::External => match mode {
                SessionMode::Plan => {
                    Verdict::Deny("Extension tools are not allowed in Plan mode".into())
                }
                SessionMode::Confirm => {
                    Verdict::Ask(self.request(ctx, risk, ApprovalReason::ExternalTool))
                }
                SessionMode::FullAccess => Verdict::Allow,
            },
        }
    }

    /// 缓存键命中判定(仅 Confirm 会产生 Ask,缓存只在 Confirm 生效)。
    fn cache_hit(&self, ctx: &ToolCallCtx, risk: ToolRiskClass) -> bool {
        if self.mode() != SessionMode::Confirm {
            return false;
        }
        let key = match risk {
            ToolRiskClass::Shell => ctx
                .args
                .get("command")
                .and_then(|command| command.as_str())
                .map(|command| {
                    ApprovalKey::ShellCommand(crate::permission::types::normalize_command(command))
                }),
            ToolRiskClass::FileWrite => ctx
                .args
                .get("path")
                .and_then(|path| path.as_str())
                .map(|path| ApprovalKey::FilePath(PathBuf::from(path))),
            ToolRiskClass::External => Some(ApprovalKey::ExternalTool(ctx.name.clone())),
            ToolRiskClass::ReadOnly => None,
        };
        match key {
            Some(key) => self.cache.lock().unwrap().contains(&key),
            None => false,
        }
    }

    /// 构造审批请求载荷。
    fn request(
        &self,
        ctx: &ToolCallCtx,
        risk: ToolRiskClass,
        reason: ApprovalReason,
    ) -> ApprovalRequest {
        let detail = match risk {
            ToolRiskClass::Shell => ctx
                .args
                .get("command")
                .and_then(|command| command.as_str())
                .map(shell_detail)
                .unwrap_or_default(),
            ToolRiskClass::FileWrite => ctx
                .args
                .get("path")
                .and_then(|path| path.as_str())
                .unwrap_or("(missing path)")
                .to_string(),
            _ => serde_json::to_string(&ctx.args).unwrap_or_default(),
        };
        ApprovalRequest {
            tool_call_id: ctx.tool_call_id.clone(),
            tool_name: ctx.name.clone(),
            args: ctx.args.clone(),
            risk,
            reason,
            detail,
        }
    }
}

// classify_tool 经 types 重导出,这里引用测试
#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::types::ApprovalDecision;

    fn engine(mode: SessionMode) -> PermissionEngine {
        PermissionEngine::new(
            mode,
            SandboxConfig::default(),
            ApprovalRules::default(),
            std::env::temp_dir(),
            true,
        )
    }

    fn ctx(name: &str, args: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "t1".into(),
            name: name.into(),
            args,
        }
    }

    fn workspace_engine(mode: SessionMode, root: &std::path::Path) -> PermissionEngine {
        PermissionEngine::new(
            mode,
            SandboxConfig {
                writable_roots: vec![],
                network_access: false,
            },
            ApprovalRules::default(),
            root.to_path_buf(),
            true,
        )
    }

    #[test]
    fn full_access_short_circuits_everything() {
        let engine = engine(SessionMode::FullAccess);
        assert_eq!(engine.evaluate(&ctx("write", serde_json::json!({"path": "x"})), ToolRiskClass::FileWrite), Verdict::Allow);
        assert_eq!(engine.evaluate(&ctx("bash", serde_json::json!({"command": "rm -rf /"})), ToolRiskClass::Shell), Verdict::Allow);
        assert_eq!(engine.evaluate(&ctx("mcp__s__t", serde_json::json!({})), ToolRiskClass::External), Verdict::Allow);
    }

    #[test]
    fn readonly_tools_always_allow() {
        for mode in [SessionMode::Plan, SessionMode::Confirm, SessionMode::FullAccess] {
            let engine = engine(mode);
            assert_eq!(
                engine.evaluate(&ctx("read", serde_json::json!({"path": "x"})), ToolRiskClass::ReadOnly),
                Verdict::Allow
            );
        }
    }

    #[test]
    fn plan_mode_denies_writes_and_mutating_shell_without_asking() {
        let engine = engine(SessionMode::Plan);
        match engine.evaluate(&ctx("write", serde_json::json!({"path": "x"})), ToolRiskClass::FileWrite) {
            Verdict::Deny(reason) => assert!(reason.contains("Plan"), "{reason}"),
            other => panic!("期望 Deny,得到 {other:?}"),
        }
        // 明确写前缀:即使有沙箱也不放行
        assert_eq!(
            engine.evaluate(&ctx("bash", serde_json::json!({"command": "rm -rf build"})), ToolRiskClass::Shell),
            Verdict::Deny("Plan mode blocks commands that modify files or system state: rm -rf build".into())
        );
        // 只读命令放行(沙箱兜底)
        assert_eq!(
            engine.evaluate(&ctx("bash", serde_json::json!({"command": "git log"})), ToolRiskClass::Shell),
            Verdict::Allow
        );
        // 联网查询放行(Plan 模式调研用)
        assert_eq!(
            engine.evaluate(&ctx("bash", serde_json::json!({"command": "curl -s https://api.example.com"})), ToolRiskClass::Shell),
            Verdict::Allow
        );
    }

    #[test]
    fn plan_mode_unknown_command_follows_sandbox_availability() {
        let ctx = ctx("bash", serde_json::json!({"command": "make test"}));
        // 有沙箱:未知命令放行,由 ReadOnly 沙箱裁决实际副作用
        let sandboxed = PermissionEngine::new(
            SessionMode::Plan,
            SandboxConfig::default(),
            ApprovalRules::default(),
            std::env::temp_dir(),
            true,
        );
        assert_eq!(sandboxed.evaluate(&ctx, ToolRiskClass::Shell), Verdict::Allow);
        // 无沙箱:没有 OS 层兜底,保守拒绝
        let degraded = PermissionEngine::new(
            SessionMode::Plan,
            SandboxConfig::default(),
            ApprovalRules::default(),
            std::env::temp_dir(),
            false,
        );
        match degraded.evaluate(&ctx, ToolRiskClass::Shell) {
            Verdict::Deny(reason) => assert!(reason.contains("read-only"), "{reason}"),
            other => panic!("期望 Deny,得到 {other:?}"),
        }
    }

    #[test]
    fn confirm_mode_asks_for_writes_inside_and_outside_roots() {
        let root = std::env::temp_dir().join("rpi_engine_roots_test");
        let _ = std::fs::create_dir_all(&root);
        let file = root.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let engine = workspace_engine(SessionMode::Confirm, &root);
        match engine.evaluate(&ctx("write", serde_json::json!({"path": "a.txt"})), ToolRiskClass::FileWrite) {
            Verdict::Ask(request) => assert_eq!(request.reason, ApprovalReason::FileWrite),
            other => panic!("期望 Ask,得到 {other:?}"),
        }
        match engine.evaluate(&ctx("write", serde_json::json!({"path": "/etc/hosts"})), ToolRiskClass::FileWrite) {
            Verdict::Ask(request) => assert_eq!(request.reason, ApprovalReason::OutsideWorkspace),
            other => panic!("期望 Ask(越界),得到 {other:?}"),
        }
        // 路径缺失 = 保守按越界
        match engine.evaluate(&ctx("write", serde_json::json!({})), ToolRiskClass::FileWrite) {
            Verdict::Ask(request) => assert_eq!(request.reason, ApprovalReason::OutsideWorkspace),
            other => panic!("期望 Ask,得到 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_mode_shell_without_sandbox_relies_on_judge() {
        // 降级矩阵 §7.5:无沙箱平台没有 OS 层兜底,只读判定是最后保证 ——
        // 判定通过即放行,不通过拒绝
        let engine = PermissionEngine::new(
            SessionMode::Plan,
            SandboxConfig::default(),
            ApprovalRules::default(),
            std::env::temp_dir(),
            false,
        );
        assert_eq!(
            engine.evaluate(&ctx("bash", serde_json::json!({"command": "git log"})), ToolRiskClass::Shell),
            Verdict::Allow
        );
        match engine.evaluate(&ctx("bash", serde_json::json!({"command": "rm -rf build"})), ToolRiskClass::Shell) {
            Verdict::Deny(reason) => assert!(reason.contains("blocks commands"), "{reason}"),
            other => panic!("期望 Deny,得到 {other:?}"),
        }
    }

    #[test]
    fn confirm_mode_shell_classification() {
        let engine = engine(SessionMode::Confirm);
        // 只读免审
        assert_eq!(
            engine.evaluate(&ctx("bash", serde_json::json!({"command": "git diff"})), ToolRiskClass::Shell),
            Verdict::Allow
        );
        // 非只读 Ask(沙箱可用)
        match engine.evaluate(&ctx("bash", serde_json::json!({"command": "make test"})), ToolRiskClass::Shell) {
            Verdict::Ask(request) => assert_eq!(request.reason, ApprovalReason::ShellCommand),
            other => panic!("期望 Ask,得到 {other:?}"),
        }
    }

    #[test]
    fn confirm_mode_no_sandbox_escalates_reason() {
        let engine = PermissionEngine::new(
            SessionMode::Confirm,
            SandboxConfig::default(),
            ApprovalRules::default(),
            std::env::temp_dir(),
            false,
        );
        match engine.evaluate(&ctx("bash", serde_json::json!({"command": "make test"})), ToolRiskClass::Shell) {
            Verdict::Ask(request) => {
                assert_eq!(request.reason, ApprovalReason::NoSandboxEscalation)
            }
            other => panic!("期望 Ask,得到 {other:?}"),
        }
    }

    #[test]
    fn deny_rule_blocks_before_ask() {
        let engine = PermissionEngine::new(
            SessionMode::Confirm,
            SandboxConfig::default(),
            ApprovalRules {
                allow_commands: vec![],
                deny_commands: vec!["git push".into()],
            },
            std::env::temp_dir(),
            true,
        );
        match engine.evaluate(&ctx("bash", serde_json::json!({"command": "git push origin"})), ToolRiskClass::Shell) {
            Verdict::Deny(reason) => assert!(reason.contains("deny"), "{reason}"),
            other => panic!("期望 Deny,得到 {other:?}"),
        }
    }

    #[test]
    fn approve_for_session_caches_same_key_only() {
        let engine = engine(SessionMode::Confirm);
        let first = ctx("bash", serde_json::json!({"command": "make test"}));
        assert!(matches!(
            engine.evaluate(&first, ToolRiskClass::Shell),
            Verdict::Ask(_)
        ));
        engine.approve_for_session(&ApprovalRequest {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"command": "make test"}),
            risk: ToolRiskClass::Shell,
            reason: ApprovalReason::ShellCommand,
            detail: "make test".into(),
        });
        assert_eq!(engine.evaluate(&first, ToolRiskClass::Shell), Verdict::Allow);
        // 不同命令不同键
        assert!(matches!(
            engine.evaluate(&ctx("bash", serde_json::json!({"command": "make build"})), ToolRiskClass::Shell),
            Verdict::Ask(_)
        ));
        // Plan 模式不走缓存(不会 Ask,只 Deny)
        engine.set_mode(SessionMode::Plan);
        assert!(engine.cache.lock().unwrap().is_empty(), "set_mode 清空缓存");
        match engine.evaluate(&ctx("write", serde_json::json!({"path": "x"})), ToolRiskClass::FileWrite) {
            Verdict::Deny(_) => {}
            other => panic!("期望 Deny,得到 {other:?}"),
        }
    }

    #[test]
    fn confirm_asks_for_external_tools_and_plan_denies() {
        let confirm = engine(SessionMode::Confirm);
        match confirm.evaluate(&ctx("mcp__s__t", serde_json::json!({})), ToolRiskClass::External) {
            Verdict::Ask(request) => assert_eq!(request.reason, ApprovalReason::ExternalTool),
            other => panic!("期望 Ask,得到 {other:?}"),
        }
        let plan = engine(SessionMode::Plan);
        assert!(matches!(
            plan.evaluate(&ctx("mcp__s__t", serde_json::json!({})), ToolRiskClass::External),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn plan_mode_allows_subagent_dispatch() {
        // subagent 派发按只读类放行:子 agent 内部工具调用仍过同一引擎,
        // Plan 的只读约束不变(14 文档)
        let plan = engine(SessionMode::Plan);
        assert!(matches!(
            plan.evaluate(
                &ctx("subagent", serde_json::json!({"task": "review"})),
                ToolRiskClass::ReadOnly,
            ),
            Verdict::Allow
        ));
        // 对照:子 agent 内部的写调用在 Plan 下仍被拒
        assert!(matches!(
            plan.evaluate(
                &ctx("write", serde_json::json!({"path": "a", "content": "x"})),
                ToolRiskClass::FileWrite,
            ),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn cache_key_from_decision_roundtrip() {
        // ApproveForSession 决策路径使用的键与 cache_hit 一致
        let engine = engine(SessionMode::Confirm);
        let request = ApprovalRequest {
            tool_call_id: "t1".into(),
            tool_name: "write".into(),
            args: serde_json::json!({"path": "a.txt", "content": "x"}),
            risk: ToolRiskClass::FileWrite,
            reason: ApprovalReason::FileWrite,
            detail: "a.txt".into(),
        };
        engine.approve_for_session(&request);
        let _ = ApprovalDecision::ApproveForSession;
        assert!(engine.cache_hit(
            &ctx("write", serde_json::json!({"path": "a.txt"})),
            ToolRiskClass::FileWrite
        ));
    }
}
