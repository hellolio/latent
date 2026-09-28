//! shell 命令只读判定(13 文档 §6.2,纯函数):保守优先,宁可误判为非只读。
//! 这只是第一道筛 —— Plan 模式所有 bash(包括判定通过的)都在 ReadOnly OS
//! 沙箱内执行(13 文档 §15.1 双保险)。

use crate::permission::types::normalize_command;

/// 内置只读前缀表(初版;settings `approval.allowCommands` 可追加)。
const READONLY_PREFIXES: &[&[&str]] = &[
    &["ls"],
    &["cat"],
    &["head"],
    &["tail"],
    &["wc"],
    &["file"],
    &["stat"],
    &["readlink"],
    &["realpath"],
    &["which"],
    &["whereis"],
    &["type"],
    &["grep"],
    &["rg"],
    &["find"],
    &["fd"],
    &["ls-tree"],
    &["git", "status"],
    &["git", "log"],
    &["git", "diff"],
    &["git", "show"],
    &["git", "blame"],
    &["git", "branch"],
    &["git", "tag"],
    &["git", "remote"],
    &["git", "rev-parse"],
    &["git", "describe"],
    &["git", "shortlog"],
    &["git", "ls-files"],
    &["cargo", "check"],
    &["cargo", "tree"],
    &["cargo", "metadata"],
    &["rustc", "--version"],
    &["echo"],
    &["printf"],
    &["pwd"],
    &["date"],
    &["whoami"],
    &["uname"],
    &["hostname"],
    &["env"],
    &["printenv"],
];

/// 判定一条 shell 命令是否只读(可安全免审)。
///
/// 规则(13 文档 §6.2):
/// 1. 元字符即非只读:`|` `&` `;` `>` `<` `` ` `` `$(` 换行;
/// 2. `env VAR=x cmd`、`sudo -n cmd` 剥离后命中内置只读前缀表 → 只读;
/// 3. deny 规则(`approval.denyCommands`)命中一律非只读(否定优先)。
pub fn is_readonly_command_with_rules(
    command: &str,
    allow: &[String],
    deny: &[String],
) -> bool {
    let normalized = normalize_command(command);
    if normalized.is_empty() {
        return false;
    }
    // 元字符检测在规范化之前(空白折叠会吃掉换行;首尾空格不属元字符)
    if contains_metacharacters(command) {
        return false;
    }
    // 否定优先于白名单
    if deny
        .iter()
        .any(|rule| prefix_matches(&normalized, &normalize_command(rule)))
    {
        return false;
    }
    let tokens: Vec<&str> = normalized.split(' ').collect();
    // deny 规则也可能只匹配元字符剥离后的形式;再查一次(前缀已保证元字符不存在)
    let stripped = strip_wrappers(&tokens);
    if READONLY_PREFIXES
        .iter()
        .any(|prefix| token_prefix_matches(stripped, prefix))
    {
        return true;
    }
    allow
        .iter()
        .any(|rule| prefix_matches(&normalized, &normalize_command(rule)))
}

/// 无规则便捷形态(测试与内置使用)。
pub fn is_readonly_command(command: &str) -> bool {
    is_readonly_command_with_rules(command, &[], &[])
}

/// 元字符检测(管道/顺序/重定向/命令替换/换行)。
fn contains_metacharacters(command: &str) -> bool {
    command.chars().any(|c| {
        matches!(
            c,
            '|' | '&'
                | ';'
                | '>'
                | '<'
                | '`'
                | '\n'
                | '\r'
        )
    }) || command.contains("$(")
}

/// 剥离 `env VAR=x` 与 `sudo -n` 包装(可嵌套,如 `sudo -n env FOO=1 ls`)。
fn strip_wrappers<'a>(tokens: &'a [&'a str]) -> &'a [&'a str] {
    let mut rest = tokens;
    loop {
        match rest {
            ["env", after @ ..] => {
                // 跳过 VAR=value 赋值与 env 自身 flag(-i/-u X/--unset=NAME);
                // 然后是实际命令
                let mut index = 0;
                while index < after.len() {
                    let token = after[index];
                    if token.contains('=') || token.starts_with("--") {
                        index += 1;
                    } else if token == "-u" && index + 1 < after.len() {
                        index += 2;
                    } else if !token.starts_with('-') {
                        break;
                    } else {
                        index += 1;
                    }
                }
                if index == after.len() {
                    return after;
                }
                rest = &after[index..];
            }
            ["sudo", "-n", after @ ..] => rest = after,
            _ => return rest,
        }
    }
}

/// tokens 是否以 prefix tokens 开头。
fn token_prefix_matches(tokens: &[&str], prefix: &[&str]) -> bool {
    tokens.len() >= prefix.len() && tokens[..prefix.len()] == *prefix
}

/// 字符串前缀匹配(词边界:prefix 后必须是结尾或空白)。
fn prefix_matches(command: &str, prefix: &str) -> bool {
    command == prefix
        || (command.starts_with(prefix)
            && command[prefix.len()..].starts_with(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_table_hits() {
        assert!(is_readonly_command("ls"));
        assert!(is_readonly_command("ls -la"));
        assert!(is_readonly_command("git log --oneline -5"));
        assert!(is_readonly_command("git status"));
        assert!(is_readonly_command("cargo check"));
        assert!(is_readonly_command("echo hello"));
        assert!(is_readonly_command("  pwd  "));
        assert!(is_readonly_command("wc -l foo.txt"));
    }

    #[test]
    fn multi_word_commands_miss_prefix() {
        // git 后跟不在表中的子命令
        assert!(!is_readonly_command("git push"));
        assert!(!is_readonly_command("git commit -m x"));
        assert!(!is_readonly_command("cargo build"));
        assert!(!is_readonly_command("rm -rf build"));
        assert!(!is_readonly_command("curl example.com"));
    }

    #[test]
    fn metacharacters_are_not_readonly() {
        assert!(!is_readonly_command("ls | wc -l"));
        assert!(!is_readonly_command("echo hi > out.txt"));
        assert!(!is_readonly_command("cat a && cat b"));
        assert!(!is_readonly_command("git log; git status"));
        assert!(!is_readonly_command("echo `date`"));
        assert!(!is_readonly_command("echo $(date)"));
        assert!(!is_readonly_command("ls\n"));
        assert!(!is_readonly_command(""));
    }

    #[test]
    fn env_and_sudo_wrappers_are_stripped() {
        assert!(is_readonly_command("env VAR=1 ls"));
        assert!(is_readonly_command("env -u X ls"));
        assert!(is_readonly_command("sudo -n git status"));
        assert!(is_readonly_command("sudo -n env FOO=1 ls"));
        // 无 -n 的 sudo 不剥离(可能要密码,交互命令)
        assert!(!is_readonly_command("sudo git push"));
        // 剥离后仍要命中前缀表
        assert!(!is_readonly_command("env VAR=1 rm x"));
    }

    #[test]
    fn deny_rules_win_over_allow_and_prefix() {
        let deny = vec!["git log".to_string()];
        assert!(!is_readonly_command_with_rules(
            "git log",
            &[],
            &deny
        ));
        let allow = vec!["make test".to_string()];
        assert!(is_readonly_command_with_rules("make test", &allow, &[]));
        assert!(!is_readonly_command_with_rules("make test-all", &allow, &[]));
        // deny 命中优先于 allow
        assert!(!is_readonly_command_with_rules(
            "make test",
            &allow,
            &["make".to_string()]
        ));
    }

    #[test]
    fn whitespace_is_normalized() {
        assert!(is_readonly_command("  ls   -la  "));
        assert!(is_readonly_command("git\tlog"));
    }
}
