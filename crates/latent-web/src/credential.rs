//! 凭据解析(credential-source.ts 的移植):明文 / `$ENV_VAR` / `!shell命令`
//! 三种来源,转义(`$$`/`$!`)与 1Password 风格动态取值,5s 超时 + 环境变量
//! 白名单 + 16KB 输出上限,错误消息中 redact key。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

const COMMAND_TIMEOUT_MS: u64 = 5_000;
const MAX_CREDENTIAL_BYTES: usize = 16_384;

/// `$NAME` / `${NAME}`(整串,与上游 ENV_SOURCE 一致)。
static ENV_SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([A-Za-z0-9_]*)\})$").unwrap());
/// 1Password service account 会话变量允许透传给凭据命令。
static OP_SESSION_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^OP_SESSION_[A-Za-z0-9_]+$").unwrap());

/// `!command` 执行时透传的环境变量白名单(防泄漏)。
const COMMAND_ENVIRONMENT_NAMES: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "OP_SERVICE_ACCOUNT_TOKEN",
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "WSL_DISTRO_NAME",
    "WSL_INTEROP",
];

/// 纯本地检查:配置串存在即算有来源(! / $ 命令在 resolve 时才真正执行),
/// 或环境变量已配置且非空。不发网络请求、零开销(分析文档 §2 原则)。
pub fn has_credential_source(
    configured_value: Option<&str>,
    environment_value: Option<&str>,
) -> bool {
    normalize(configured_value).is_some() || normalize(environment_value).is_some()
}

fn normalize(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn explicit_environment_name(source: &str) -> Option<String> {
    ENV_SOURCE
        .captures(source)
        .and_then(|c| c.get(1).or_else(|| c.get(2)))
        .map(|m| m.as_str().to_string())
}

/// 错误文本中去掉凭据本体。
pub fn redact_credential(text: &str, credential: Option<&str>) -> String {
    match credential {
        Some(credential) if !credential.is_empty() => text.replace(credential, "[redacted]"),
        _ => text.to_string(),
    }
}

/// 凭据解析结果:None = 完全未配置(调用方决定报错还是跳过)。
pub async fn resolve_credential(
    provider: &str,
    configured_value: Option<&str>,
    environment_value: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(source) = normalize(configured_value) else {
        return Ok(normalize(environment_value));
    };
    // 转义:$$secret / $!secret → 字面值
    if let Some(escaped) = source
        .strip_prefix("$$")
        .or_else(|| source.strip_prefix("$!"))
    {
        return Ok(Some(escaped.to_string()));
    }
    if let Some(command) = source.strip_prefix('!') {
        let command = command.trim();
        if command.is_empty() {
            return Err(format!("{provider} credential resolution failed: invalid-source"));
        }
        return run_credential_command(provider, command).await.map(Some);
    }
    if let Some(name) = explicit_environment_name(&source) {
        let value = std::env::var(&name).ok();
        let Some(value) = normalize(value.as_deref()) else {
            return Err(format!(
                "{provider} credential resolution failed: environment-empty"
            ));
        };
        return Ok(Some(value));
    }
    // `$` 开头但不是合法 ENV 引用 → 显式配置错误
    if source.starts_with('$') {
        return Err(format!(
            "{provider} credential resolution failed: invalid-source"
        ));
    }
    Ok(normalize(environment_value).or(Some(source)))
}

fn command_environment() -> HashMap<String, String> {
    let mut environment = HashMap::new();
    for name in COMMAND_ENVIRONMENT_NAMES {
        if let Ok(value) = std::env::var(name) {
            environment.insert((*name).to_string(), value);
        }
    }
    for (name, value) in std::env::vars() {
        if OP_SESSION_NAME.is_match(&name) {
            environment.insert(name, value);
        }
    }
    environment
}

fn shell_program() -> (&'static str, Vec<&'static str>) {
    if cfg!(windows) {
        ("cmd", vec!["/C"])
    } else {
        ("sh", vec!["-c"])
    }
}

async fn run_credential_command(provider: &str, command: &str) -> Result<String, String> {
    let (program, args) = shell_program();
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(command_environment())
        .spawn()
        .map_err(|error| {
            format!(
                "{provider} credential resolution failed: command-failed ({error})"
            )
        })?;
    let mut stdout = child.stdout.take().expect("stdout piped");

    let read_all = async {
        let mut buffer = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut buffer).await?;
        Ok::<Vec<u8>, std::io::Error>(buffer)
    };
    let output = match tokio::time::timeout(Duration::from_millis(COMMAND_TIMEOUT_MS), read_all).await
    {
        Ok(result) => result.map_err(|error| {
            format!(
                "{provider} credential resolution failed: command-failed ({error})"
            )
        })?,
        Err(_) => {
            let _ = child.start_kill();
            return Err(format!(
                "{provider} credential resolution failed: command-timeout"
            ));
        }
    };
    let status = child.wait().await.map_err(|error| {
        format!("{provider} credential resolution failed: command-failed ({error})")
    })?;
    if output.len() > MAX_CREDENTIAL_BYTES {
        return Err(format!(
            "{provider} credential resolution failed: command-output-too-large"
        ));
    }
    let stdout = String::from_utf8_lossy(&output).to_string();
    if !status.success() {
        return Err(format!(
            "{provider} credential resolution failed: command-failed (exit {status})"
        ));
    }
    let value = stdout.trim().to_string();
    if value.is_empty() {
        return Err(format!(
            "{provider} credential resolution failed: command-empty"
        ));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(format!(
            "{provider} credential resolution failed: command-invalid-output"
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_is_pure_local() {
        assert!(has_credential_source(Some("!op read x"), None));
        assert!(has_credential_source(Some("$MY_KEY"), None));
        assert!(has_credential_source(Some("plain"), None));
        assert!(has_credential_source(None, Some("env-value")));
        assert!(!has_credential_source(None, Some("")));
        assert!(!has_credential_source(None, None));
    }

    #[tokio::test]
    async fn resolves_escape_and_plain_and_env() {
        assert_eq!(
            resolve_credential("Brave", Some("$$literal"), None).await.unwrap(),
            Some("literal".into())
        );
        assert_eq!(
            resolve_credential("Brave", Some("plain-key"), None).await.unwrap(),
            Some("plain-key".into())
        );
        unsafe { std::env::set_var("LATENT_WEB_TEST_KEY", "from-env") };
        assert_eq!(
            resolve_credential("Brave", Some("$LATENT_WEB_TEST_KEY"), None)
                .await
                .unwrap(),
            Some("from-env".into())
        );
        assert!(resolve_credential("Brave", Some("$LATENT_WEB_MISSING_KEY"), None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn resolves_command() {
        let value = resolve_credential("Brave", Some("!echo hello-key"), None)
            .await
            .unwrap();
        assert_eq!(value, Some("hello-key".into()));
        assert!(resolve_credential("Brave", Some("!exit 3"), None)
            .await
            .is_err());
    }

    #[test]
    fn redaction() {
        assert_eq!(
            redact_credential("failed with key abc123", Some("abc123")),
            "failed with key [redacted]"
        );
        assert_eq!(redact_credential("no key", None), "no key");
    }
}
