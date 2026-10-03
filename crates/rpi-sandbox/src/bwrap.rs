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

/// bwrap 是否在 PATH(探测用;进程启动后 PATH 不变,首次访问结果缓存)。
pub fn bwrap_in_path() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("bwrap").is_file())
        })
    })
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
            argv.push("--bind".into());
            argv.push(root.display().to_string());
            argv.push(root.display().to_string());
            // 版本控制/会话元数据/环境密钥永远只读:ro-bind 后置覆盖 rw-bind
            for always_readonly in [".git", ".rpi", ".env"] {
                let subpath = root.join(always_readonly);
                if subpath.exists() {
                    argv.push("--ro-bind".into());
                    argv.push(subpath.display().to_string());
                    argv.push(subpath.display().to_string());
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
        assert!(wrapped.contains(&format!("--bind {} {}", canonical.display(), canonical.display())));
    }
}
