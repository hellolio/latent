//! latent-sandbox —— OS 级沙箱(13 文档 §7):macOS Seatbelt(SBPL)、Linux
//! bwrap、Linux Landlock+seccomp 三个后端。零内部依赖,整体可拆卸。
//!
//! 职责边界(13 文档 §6.4):只做**命令包装**(怎么跑),不做审批(问不问)
//! 与拒绝(拒不拒)—— 那是权限引擎的事。包装失败(无法构造沙箱)返回 Err,
//! 由调用方(ShellSpawnHook)转拒绝。
//!
//! Landlock 后端的限制必须由**目标进程自身**施加(landlock_restrict_self 是
//! 进程内 syscall,exec 后保持),因此 wrap 产物是「以本可执行文件为 helper
//! 的命令行」:helper 进程先落 Landlock/seccomp 再 exec 真命令。helper 入口
//! 见 [`landlock::run_helper`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub mod bwrap;
pub mod landlock;
pub mod seatbelt;

/// 沙箱策略(13 文档 §3.3,对标 Codex SandboxPolicy 的收敛子集)。
/// 与 latent-core 的权限策略类型解耦:装配层负责映射(core 不依赖本 crate)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum SandboxPolicy {
    /// 全盘只读;network_access 控制网络(Plan 模式联网查询放行 = true)
    ReadOnly {
        #[serde(default)]
        network_access: bool,
    },
    /// 全盘可读 + 可写根内可写
    WorkspaceWrite {
        /// 额外可写根;cwd 与系统临时目录自动并入([`writable_roots_for`])
        #[serde(default)]
        writable_roots: Vec<String>,
        #[serde(default)]
        network_access: bool,
    },
    /// 不设防(FullAccess 模式;等价于不包装)
    DangerFullAccess,
}

/// 平台沙箱可用性(启动时探测一次)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxAvailability {
    /// /usr/bin/sandbox-exec 存在
    MacosSeatbelt,
    /// bwrap 在 PATH
    LinuxBwrap,
    /// 内核支持 Landlock(备选,无外部依赖)
    LinuxLandlock,
    /// Windows / 探测失败
    None,
}

/// 平台探测(13 文档 §7.1):macOS 对 sandbox-exec 做**真实执行探测**(该
/// 二进制在部分 macOS 版本上存在但运行即 EPERM,存在性检查会假阳性);
/// Linux 先 bwrap 后 Landlock(lsm 列表探测,无 unsafe)。
pub fn detect_availability() -> SandboxAvailability {
    #[cfg(target_os = "macos")]
    {
        if Path::new(seatbelt::SEATBELT_EXEC).exists()
            && seatbelt_probe_executes()
        {
            return SandboxAvailability::MacosSeatbelt;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if bwrap::bwrap_in_path() {
            return SandboxAvailability::LinuxBwrap;
        }
        if landlock::kernel_support_detected() {
            return SandboxAvailability::LinuxLandlock;
        }
    }
    let _ = std::env::var_os("PATH");
    SandboxAvailability::None
}

/// seatbelt 真实执行探测:最小 profile 跑 /bin/true。
/// 只在装配期调用一次,失败 = 本机 sandbox-exec 不可用(如被系统策略禁用)。
#[cfg(target_os = "macos")]
fn seatbelt_probe_executes() -> bool {
    std::process::Command::new(seatbelt::SEATBELT_EXEC)
        .args(["-p", "(version 1)(deny default)(allow process-exec* (subpath \"/bin\"))"])
        .arg("/bin/true")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// 沙箱执行器 trait(10 文档 §2 规则 1/2:工厂出厂,上游只见 trait)。
pub trait Sandbox: Send + Sync {
    /// 把命令包装为沙箱内执行。返回 Err(String) = 无法构造沙箱。
    fn wrap_command(&self, command: &str, cwd: &Path) -> Result<String, String>;
    fn policy(&self) -> &SandboxPolicy;
}

/// 沙箱工厂:`None` = DangerFullAccess(无需沙箱);`Err` = 策略要求沙箱但
/// 平台不可用(装配层降级为审批,绝不静默裸跑)。`helper_exe` 是 Landlock
/// helper 使用的本可执行文件路径(仅 LinuxLandlock 后端需要)。
pub fn create_sandbox(
    policy: &SandboxPolicy,
    helper_exe: Option<&Path>,
) -> Result<Option<Arc<dyn Sandbox>>, String> {
    if matches!(policy, SandboxPolicy::DangerFullAccess) {
        return Ok(None);
    }
    match detect_availability() {
        SandboxAvailability::MacosSeatbelt => {
            Ok(Some(Arc::new(seatbelt::SeatbeltSandbox::new(policy.clone()))))
        }
        SandboxAvailability::LinuxBwrap => {
            Ok(Some(Arc::new(bwrap::BwrapSandbox::new(policy.clone()))))
        }
        SandboxAvailability::LinuxLandlock => {
            let exe = helper_exe.ok_or_else(|| {
                "Landlock 后端需要 helper 可执行文件路径".to_string()
            })?;
            Ok(Some(Arc::new(landlock::LandlockSandbox::new(
                policy.clone(),
                exe.to_path_buf(),
            ))))
        }
        SandboxAvailability::None => Err("No sandbox available on this platform".to_string()),
    }
}

/// 可写根计算(13 文档 §7.3):cwd + 系统临时目录 + 额外配置;只读策略为空。
/// 全部尽力 canonicalize(失败保留原路径)。
pub fn writable_roots_for(policy: &SandboxPolicy, cwd: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    match policy {
        SandboxPolicy::ReadOnly { .. } | SandboxPolicy::DangerFullAccess => {}
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access: _,
        } => {
            let mut push = |path: PathBuf| {
                let canonical = path.canonicalize().unwrap_or(path);
                if !roots.contains(&canonical) {
                    roots.push(canonical);
                }
            };
            push(cwd.to_path_buf());
            for tmp in system_tmp_dirs() {
                push(tmp);
            }
            for root in writable_roots {
                push(PathBuf::from(root));
            }
        }
    }
    roots
}

/// 系统临时目录(macOS 每用户 TMPDIR 优先,/tmp 兜底,取存在者)。
fn system_tmp_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(tmp) = std::env::var_os("TMPDIR") {
        let path = PathBuf::from(tmp);
        if path.is_dir() {
            dirs.push(path);
        }
    }
    let fallback = PathBuf::from("/tmp");
    if fallback.is_dir() {
        dirs.push(fallback);
    }
    dirs
}

/// 策略是否放行网络(ReadOnly 与 WorkspaceWrite 各带 network_access;
/// DangerFullAccess 不设防,包装层不会用到)。
pub(crate) fn policy_network_access(policy: &SandboxPolicy) -> bool {
    match policy {
        SandboxPolicy::ReadOnly { network_access } => *network_access,
        SandboxPolicy::WorkspaceWrite { network_access, .. } => *network_access,
        SandboxPolicy::DangerFullAccess => true,
    }
}

/// sh 单引号转义(`'` → `'\''`)。
pub(crate) fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_serde_roundtrip_kebab_case() {
        for (json, policy) in [
            (
                r#"{"type":"read-only","networkAccess":false}"#,
                SandboxPolicy::ReadOnly {
                    network_access: false,
                },
            ),
            (
                r#"{"type":"read-only","networkAccess":true}"#,
                SandboxPolicy::ReadOnly {
                    network_access: true,
                },
            ),
            (
                r#"{"type":"workspace-write","writableRoots":[],"networkAccess":true}"#,
                SandboxPolicy::WorkspaceWrite {
                    writable_roots: vec![],
                    network_access: true,
                },
            ),
            (
                r#"{"type":"danger-full-access"}"#,
                SandboxPolicy::DangerFullAccess,
            ),
        ] {
            let parsed: SandboxPolicy = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, policy);
            assert_eq!(serde_json::to_string(&policy).unwrap(), json);
        }
    }

    #[test]
    fn writable_roots_include_cwd_and_tmp() {
        let cwd = std::env::temp_dir();
        let canonical_cwd = cwd.canonicalize().unwrap();
        let roots = writable_roots_for(
            &SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![cwd.display().to_string()],
                network_access: false,
            },
            &cwd,
        );
        assert!(roots.contains(&canonical_cwd));
        // /tmp 在 macOS 上 canonicalize 为 /private/tmp,按前缀匹配
        assert!(roots.iter().any(|root| root.starts_with("/private/tmp") || root.starts_with("/tmp") || root == &canonical_cwd));
        assert!(writable_roots_for(
            &SandboxPolicy::ReadOnly {
                network_access: false
            },
            &cwd
        )
        .is_empty());
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("ls"), "'ls'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
