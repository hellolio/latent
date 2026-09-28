//! macOS Seatbelt(SBPL)后端(13 文档 §7.2):`sandbox-exec -p <profile>`
//! 包装,profile 由模板拼装。只做 profile/命令串的纯字符串生成(可测);
//! 真沙箱行为由开发机手测(13 文档 §15.7)。

use std::path::Path;

use crate::{shell_quote, Sandbox, SandboxPolicy};

/// 固定绝对路径(防 PATH 注入,对齐 Codex seatbelt.rs)。
pub const SEATBELT_EXEC: &str = "/usr/bin/sandbox-exec";

/// SBPL profile 模板:全盘可读 + 可写根白名单 + 网络(按策略)。
/// ReadOnly 与 WorkspaceWrite 的差异**只有**可写根列表与网络两条。
fn build_profile(writable_roots: &[String], network_access: bool) -> String {
    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         ; 全盘可读\n\
         (allow file-read*)\n",
    );
    // 写权限:仅可写根
    if writable_roots.is_empty() {
        profile.push_str("; 无可写根(只读策略)\n");
    } else {
        profile.push_str("(allow file-write*");
        for root in writable_roots {
            profile.push_str(&format!(" (subpath \"{}\")", sbpl_escape(root)));
        }
        profile.push_str(")\n");
    }
    // 进程与基础系统操作(平台必需集)
    profile.push_str(
        "(allow process-exec* (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") \
         (subpath \"/opt/homebrew\"))\n\
         (allow process-fork)\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow file-read* (subpath \"/private/var/db/dyld\"))\n",
    );
    if network_access {
        profile.push_str("(allow network*)\n");
    } else {
        profile.push_str("(deny network*)\n");
    }
    profile
}

/// SBPL 字符串转义(反斜杠与双引号)。
fn sbpl_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

pub struct SeatbeltSandbox {
    policy: SandboxPolicy,
}

impl SeatbeltSandbox {
    pub fn new(policy: SandboxPolicy) -> Self {
        SeatbeltSandbox { policy }
    }
}

impl Sandbox for SeatbeltSandbox {
    fn wrap_command(&self, command: &str, cwd: &Path) -> Result<String, String> {
        let roots = crate::writable_roots_for(&self.policy, cwd);
        // 可写根内的版本控制/会话元数据/环境密钥永远只读(SBPL 后规则覆盖前规则)
        let mut profile = build_profile(
            &roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>(),
            matches!(&self.policy, SandboxPolicy::WorkspaceWrite { network_access: true, .. }),
        );
        for root in &roots {
            for always_readonly in [".git", ".rpi", ".env"] {
                let subpath = root.join(always_readonly);
                if subpath.exists() {
                    profile.push_str(&format!(
                        "(deny file-write* (subpath \"{}\"))\n",
                        sbpl_escape(&subpath.display().to_string())
                    ));
                }
            }
        }
        Ok(format!(
            "{} -p {} /bin/sh -c {}",
            SEATBELT_EXEC,
            shell_quote(&profile),
            shell_quote(command)
        ))
    }

    fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(policy: SandboxPolicy) -> SeatbeltSandbox {
        SeatbeltSandbox::new(policy)
    }

    #[test]
    fn readonly_profile_denies_network_and_has_no_write_roots() {
        let wrapped = sandbox(SandboxPolicy::ReadOnly)
            .wrap_command("ls", Path::new("/tmp"))
            .unwrap();
        assert!(wrapped.starts_with(SEATBELT_EXEC));
        assert!(wrapped.contains("(deny default)"));
        assert!(wrapped.contains("(allow file-read*)"));
        assert!(wrapped.contains("(deny network*)"));
        assert!(!wrapped.contains("file-write* (subpath"));
    }

    #[test]
    fn workspace_write_profile_lists_roots_and_network_flag() {
        let cwd = std::env::temp_dir();
        let wrapped = sandbox(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
        })
        .wrap_command("ls", &cwd)
        .unwrap();
        assert!(wrapped.contains(&format!(
            "(subpath \"{}\")",
            cwd.canonicalize().unwrap().display()
        )));
        assert!(wrapped.contains("(deny network*)"));
        let networked = sandbox(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: true,
        })
        .wrap_command("ls", &cwd)
        .unwrap();
        assert!(networked.contains("(allow network*)"));
    }

    #[test]
    fn command_is_single_quote_escaped() {
        let wrapped = sandbox(SandboxPolicy::ReadOnly)
            .wrap_command("echo 'hi'", Path::new("/tmp"))
            .unwrap();
        assert!(wrapped.contains("'echo '\\''hi'\\'''"), "{wrapped}");
    }

    #[test]
    fn git_dir_gets_explicit_deny() {
        let cwd = std::env::temp_dir().join("rpi_seatbelt_git_test");
        let _ = std::fs::create_dir_all(cwd.join(".git"));
        let wrapped = sandbox(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
        })
        .wrap_command("ls", &cwd)
        .unwrap();
        assert!(wrapped.contains(".git"), "{wrapped}");
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
