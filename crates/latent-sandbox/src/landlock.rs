//! Linux Landlock 后端(13 文档 §7.4,bwrap 未安装时的备选):Landlock 是
//! 进程内 syscall(exec 后保持),而 latent 的沙箱接缝只做命令串包装 —— 因此
//! wrap 产物以**本可执行文件**为 helper:helper 进程先落 Landlock(文件系统)
//! + seccomp(socket(2) 过滤,网络限制)再 exec 真命令。
//!
//! helper 入口 [`run_helper`] 由 latent CLI main 在自检到 `--latent-landlock-helper`
//! 参数时调用(仅在 Linux 编译;其他平台返回错误)。

use std::path::{Path, PathBuf};

use crate::{shell_quote, Sandbox, SandboxPolicy};

/// helper 自识别参数(理论上与用户命令冲突的概率由显式 `--` 终结符消除)。
pub const HELPER_FLAG: &str = "--latent-landlock-helper";

pub struct LandlockSandbox {
    policy: SandboxPolicy,
    helper_exe: PathBuf,
}

impl LandlockSandbox {
    pub fn new(policy: SandboxPolicy, helper_exe: PathBuf) -> Self {
        LandlockSandbox { policy, helper_exe }
    }
}

/// 内核 Landlock 支持(无 unsafe 探测:/sys/kernel/security/lsm 含 landlock)。
pub fn kernel_support_detected() -> bool {
    std::fs::read_to_string("/sys/kernel/security/lsm")
        .map(|lsm| lsm.contains("landlock"))
        .unwrap_or(false)
}

impl Sandbox for LandlockSandbox {
    fn wrap_command(&self, command: &str, cwd: &Path) -> Result<String, String> {
        let roots = crate::writable_roots_for(&self.policy, cwd);
        let network = crate::policy_network_access(&self.policy);
        let mut argv: Vec<String> = vec![
            shell_quote(&self.helper_exe.display().to_string()),
            HELPER_FLAG.into(),
        ];
        for root in &roots {
            argv.push("--rw-root".into());
            argv.push(shell_quote(&root.display().to_string()));
        }
        if network {
            argv.push("--allow-net".into());
        }
        argv.push("--".into());
        argv.push("/bin/sh".into());
        argv.push("-c".into());
        argv.push(shell_quote(command));
        Ok(argv.join(" "))
    }

    fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }
}

/// helper 执行体(main 检测到 HELPER_FLAG 后调用):解析参数 → Landlock 限制
/// → seccomp 网络过滤 → exec 余下参数。**不返回**(exec 成功)或返回 Err。
///
/// # Arguments
/// * `args` — HELPER_FLAG 之后的参数(即 main 里跳过 helper 标志前的部分)。
///
/// 非 Linux 平台:helper 不存在(探测已保证不会被路由到此处)。
#[cfg(not(target_os = "linux"))]
pub fn run_helper(_args: &[String]) -> Result<(), String> {
    Err("landlock sandbox supports Linux only".to_string())
}

#[cfg(target_os = "linux")]
pub fn run_helper(args: &[String]) -> Result<(), String> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut allow_net = false;
    let mut rest: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--rw-root" => {
                let root = iter
                    .next()
                    .ok_or_else(|| "--rw-root is missing its path argument".to_string())?;
                roots.push(PathBuf::from(root));
            }
            "--allow-net" => allow_net = true,
            "--" => {
                rest = iter.cloned().collect();
                break;
            }
            other => return Err(format!("landlock helper: unknown argument: {other}")),
        }
    }
    if rest.is_empty() {
        return Err("landlock helper: missing `-- <command>` section".to_string());
    }

    restrict_filesystem(&roots)?;
    if !allow_net {
        restrict_network()?;
    }

    use std::os::unix::process::CommandExt as _;
    let error = std::process::Command::new(&rest[0]).args(&rest[1..]).exec();
    Err(format!("landlock helper exec failed: {error}"))
}

/// Landlock 文件系统限制:读全盘、写仅可写根(非 Linux 平台不支持)。
#[cfg(target_os = "linux")]
fn restrict_filesystem(roots: &[PathBuf]) -> Result<(), String> {
    use landlock::{
        AccessFs, ABI, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    };
    // 兼容模式:内核 ABI 低于所选版本时降级到可用访问集
    let abi = ABI::new_enforceable().map_err(|e| format!("landlock ABI probe failed: {e}"))?;
    let read_access = AccessFs::from_read(abi)
        .map_err(|e| format!("failed to build landlock read access set: {e}"))?;
    let all_access = AccessFs::from_all(abi).map_err(|e| format!("failed to build landlock full access set: {e}"))?;
    let ruleset = Ruleset::default()
        .set_compatibility(true)
        .handle_access(all_access)
        .and_then(|ruleset| ruleset.create())
        .map_err(|e| format!("failed to create landlock ruleset: {e}"))?;
    // 全盘可读(基线),可写根放开全部访问
    let ruleset = ruleset
        .add_rule(PathBeneath::new(read_access, PathBuf::from("/")))
        .map_err(|e| format!("failed to add landlock read rule: {e}"))?;
    for root in roots {
        let fd = PathFd::new(root).map_err(|e| format!("failed to open landlock writable root: {e}"))?;
        ruleset
            .add_rule(PathBeneath::new(all_access, fd))
            .map_err(|e| format!("failed to add landlock write rule: {e}"))?;
    }
    ruleset
        .restrict_self()
        .map_err(|e| format!("failed to apply landlock restriction: {e}"))
}

/// seccomp 网络限制:socket(2) 的非 AF_UNIX 调用一律 Errno(13 文档 §7.4)。
#[cfg(target_os = "linux")]
fn restrict_network() -> Result<(), String> {
    use seccompiler::{SeccompAction, SeccompFilter, SeccompRule};

    let arch = if cfg!(target_arch = "x86_64") {
        seccompiler::TargetArch::x86_64
    } else if cfg!(target_arch = "aarch64") {
        seccompiler::TargetArch::aarch64
    } else {
        return Err("landlock helper: unsupported CPU architecture (network restriction inactive)".to_string());
    };
    // AF_UNIX = 1;socket(domain != 1) → EPERM
    let socket_not_unix = SeccompRule::new(vec![])
        .and_condition(seccompiler::SeccompCondition::new(
            0,
            seccompiler::SeccompValueMask::Eq(1),
            false,
        ))
        .map_err(|e| format!("failed to build seccomp condition: {e}"))?;
    let filter = SeccompFilter::new(
        [(libc_consts::SYS_SOCKET, vec![socket_not_unix])]
            .into_iter()
            .collect(),
        SeccompAction::Errno,
        SeccompAction::Allow,
        arch,
    )
    .map_err(|e| format!("failed to build seccomp filter: {e}"))?;
    let program: seccompiler::BpfProgram =
        filter.try_into().map_err(|e| format!("failed to compile seccomp filter: {e}"))?;
    seccompiler::apply_filter(&program).map_err(|e| format!("failed to apply seccomp filter: {e}"))
}

/// socket(2) 系统调用号(避免引入 libc 依赖;目标架构在 restrict_network 里已限定)。
#[cfg(target_os = "linux")]
mod libc_consts {
    #[cfg(target_arch = "x86_64")]
    pub const SYS_SOCKET: u64 = 41;
    #[cfg(target_arch = "aarch64")]
    pub const SYS_SOCKET: u64 = 198;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_command_routes_through_helper_flag() {
        let cwd = std::env::temp_dir();
        let canonical = cwd.canonicalize().unwrap();
        let wrapped = LandlockSandbox::new(
            SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![],
                network_access: false,
            },
            PathBuf::from("/usr/local/bin/latent"),
        )
        .wrap_command("ls", &cwd)
        .unwrap();
        assert!(wrapped.contains(HELPER_FLAG), "{wrapped}");
        assert!(
            wrapped.contains(&format!("--rw-root '{}'", canonical.display())),
            "{wrapped}"
        );
        assert!(!wrapped.contains("--allow-net"));
        assert!(wrapped.contains("-- /bin/sh -c 'ls'"));
    }

    #[test]
    fn allow_net_flag_reflects_policy() {
        let wrapped = LandlockSandbox::new(
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
            PathBuf::from("/usr/bin/latent"),
        )
        .wrap_command("ls", Path::new("/tmp"))
        .unwrap();
        assert!(!wrapped.contains("--allow-net"));
        let networked = LandlockSandbox::new(
            SandboxPolicy::ReadOnly {
                network_access: true,
            },
            PathBuf::from("/usr/bin/latent"),
        )
        .wrap_command("curl https://example.com", Path::new("/tmp"))
        .unwrap();
        assert!(networked.contains("--allow-net"));
    }
}
