//! `!` bash 透传(pi 语义):后台执行 shell 命令,输出回流 UI;
//! `!` 前缀(单感叹号)输出注入对话上下文,`!!` 仅执行不注入。
//!
//! 输出在执行器层净化(pi 的 bash-executor.ts:82:stripAnsi → 控制字符过滤 →
//! 去 `\r`)——这是 rpi 唯一做内容净化的路径;模型工具结果字节级保真。

/// 执行 shell 命令,返回 (合并输出, 是否失败)。
/// stdout/stderr 合并(交互式反馈优先完整性),净化后截断。
pub async fn run_command(command: &str) -> (String, bool) {
    let output = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .output()
        .await;
    match output {
        Ok(output) => {
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&output.stdout));
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            (truncate_output(&sanitize_text(&text)), !output.status.success())
        }
        Err(error) => (format!("bash: {error}"), true),
    }
}

/// 净化(pi bash-executor 的链):剥离 ANSI → 过滤控制字符 → 去 `\r`。
fn sanitize_text(text: &str) -> String {
    rpi_tools::sanitize_output(text).replace('\r', "")
}

/// 输出截断上限(字符):上下文注入与屏显都不吃超长输出。
const MAX_OUTPUT_CHARS: usize = 8000;

fn truncate_output(text: &str) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
    format!("{cut}\n… (output truncated)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_simple_command() {
        let (output, is_error) = run_command("echo hello").await;
        assert!(!is_error);
        assert_eq!(output.trim(), "hello");
    }

    #[tokio::test]
    async fn captures_stderr_and_exit_code() {
        let (output, is_error) = run_command("echo oops >&2; exit 3").await;
        assert!(is_error);
        assert!(output.contains("oops"), "{output}");
    }

    #[test]
    fn truncate_keeps_marker() {
        let long = "x".repeat(MAX_OUTPUT_CHARS + 100);
        let cut = truncate_output(&long);
        assert!(cut.contains("output truncated"));
    }

    // ---- 净化(pi bash-executor 语义):! 输出剥 ANSI/控制字符/`\r` ----
    #[test]
    fn sanitizes_ansi_and_control_chars() {
        let text = "\u{1b}[31mRED\u{1b}[0m\u{7}ok\r\nnext\r";
        assert_eq!(sanitize_text(text), "REDok\nnext");
    }

    #[tokio::test]
    async fn run_command_output_is_sanitized() {
        let (output, _) = run_command("printf '\\033[32mgreen\\033[0m done\\n'").await;
        assert!(!output.contains('\u{1b}'), "应剥离 ANSI 码: {output:?}");
        assert!(output.contains("green done"), "{output}");
    }
}
