//! Linux bubblewrap 后端(13 文档 §7.4):文件系统限制交给 bwrap,参数序列
//! 断言可测(13 文档 §15.7:真沙箱行为由开发机手测)。

use std::path::Path;

use crate::{shell_quote, Sandbox, SandboxPolicy};

pub struct BwrapSandbox {
    policy: SandboxPolicy,
}

impl BwrapSandbox {
    pub fn new(policy: SandboxPolicy) -> Self {
        BwrapSandbox { policy }
    }
}

/// bwrap 真实执行探测:先查 PATH,再用与 [`BwrapSandbox::wrap_command`]
/// 相同的参数骨架跑 `/bin/sh -c true`。存在但不可用(内核禁用非特权
/// user namespace 且未 setuid、发行版裁剪)→ false,由 detect_availability
/// 落到 Landlock 备选。进程启动后 PATH 不变,首次访问结果缓存。
#[cfg(target_os = "linux")]
pub fn bwrap_probe_executes() -> bool {
    static USABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *USABLE.get_or_init(|| {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("bwrap").is_file())
        }) && std::process::Command::new("bwrap")
            .args(bwrap_probe_args())
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

/// 探测参数(与 wrap_command 骨架一致,减去可写根绑定);独立纯函数可测。
#[cfg(target_os = "linux")]
fn bwrap_probe_args() -> Vec<&'static str> {
    vec![
        "--unshare-all",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--",
        "/bin/sh",
        "-c",
        "true",
    ]
}

impl Sandbox for BwrapSandbox {
    fn wrap_command(&self, command: &str, cwd: &Path) -> Result<String, String> {
        let roots = crate::writable_roots_for(&self.policy, cwd);
        let network = crate::policy_network_access(&self.policy);
        let mut argv: Vec<String> = vec![
            "bwrap".into(),
            "--unshare-all".into(),
        ];
        if network {
            argv.push("--share-net".into());
        }
        // 全盘只读视图,可写根随后放开(后绑定覆盖前绑定)
        argv.push("--ro-bind".into());
        argv.push("/".into());
        argv.push("/".into());
        for root in &roots {
            let quoted_root = crate::shell_quote(&root.display().to_string());
            argv.push("--bind".into());
            argv.push(quoted_root.clone());
            argv.push(quoted_root);
            // 版本控制/会话元数据/环境密钥永远只读:ro-bind 后置覆盖 rw-bind
            for always_readonly in [".git", ".latent", ".env"] {
                let subpath = root.join(always_readonly);
                if subpath.exists() {
                    let quoted = crate::shell_quote(&subpath.display().to_string());
                    argv.push("--ro-bind".into());
                    argv.push(quoted.clone());
                    argv.push(quoted);
                }
            }
        }
        argv.extend([
            "--dev".into(),
            "/dev".into(),
            "--proc".into(),
            "/proc".into(),
            "--die-with-parent".into(),
            "--new-session".into(),
            "--".into(),
            "/bin/sh".into(),
            "-c".into(),
            shell_quote(command),
        ]);
        Ok(argv.join(" "))
    }

    fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 有可写根时路径必须过 shell 引号(含空格路径否则炸命令)。
    #[test]
    fn writable_root_paths_are_shell_quoted() {
        let dir = std::env::temp_dir().join("latent bwrap quoting test");
        let _ = std::fs::create_dir_all(&dir);
        let canonical = dir.canonicalize().unwrap();
        let wrapped = BwrapSandbox::new(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![dir.display().to_string()],
            network_access: false,
        })
        .wrap_command("ls", Path::new("/tmp"))
        .unwrap();
        let expected = shell_quote(&canonical.display().to_string());
        assert!(
            wrapped.contains(&format!("--bind {expected} {expected}")),
            "{wrapped}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实执行探测与存在性一致:bwrap 在 PATH 上就必须探测通过
    /// (失败意味着"存在但不可用",应落到 Landlock 备选而非产出必败包装)。
    #[cfg(target_os = "linux")]
    #[test]
    fn probe_succeeds_when_bwrap_present_in_path() {
        let present = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("bwrap").is_file())
        });
        if !present {
            return;
        }
        assert!(
            bwrap_probe_executes(),
            "bwrap 在 PATH 上但真实执行探测失败(检查非特权 user namespace / setuid)"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_args_match_wrap_skeleton() {
        let args = bwrap_probe_args();
        assert_eq!(args[0], "--unshare-all");
        assert!(args.contains(&"--ro-bind"));
        assert!(args.ends_with(&["/bin/sh", "-c", "true"]));
    }

    #[test]
    fn readonly_wraps_with_ro_bind_and_no_network() {
        let wrapped = BwrapSandbox::new(SandboxPolicy::ReadOnly {
            network_access: false,
        })
        .wrap_command("ls", Path::new("/tmp"))
        .unwrap();
        assert!(wrapped.starts_with("bwrap --unshare-all"));
        assert!(!wrapped.contains("--share-net"));
        assert!(wrapped.contains("--ro-bind / /"));
        assert!(wrapped.ends_with("-c 'ls'"));
        // 联网查询放行的只读策略(Plan 模式):share-net
        let networked = BwrapSandbox::new(SandboxPolicy::ReadOnly {
            network_access: true,
        })
        .wrap_command("curl https://example.com", Path::new("/tmp"))
        .unwrap();
        assert!(networked.contains("--share-net"));
    }

    #[test]
    fn workspace_write_binds_roots_and_share_net_flag() {
        let cwd = std::env::temp_dir();
        let canonical = cwd.canonicalize().unwrap();
        let wrapped = BwrapSandbox::new(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: true,
        })
        .wrap_command("ls", &cwd)
        .unwrap();
        assert!(wrapped.contains("--share-net"));
        let quoted = shell_quote(&canonical.display().to_string());
        assert!(wrapped.contains(&format!("--bind {quoted} {quoted}")));
    }
}
