//! awk / sed 脚本内容分析(纯函数):判定一段脚本文本是否**可证明无副作用**。
//!
//! 这两个工具的危险能力全部藏在脚本字符串里(awk 的 `system()`/输出重定向、
//! sed 的 `w`/`e`/`-i`),命令名前缀表无法校验。这里对脚本文本做词法扫描,
//! 只放行安全子集;拿不准一律拒绝(保守优先,宁可误判)。

/// awk 程序是否无副作用。
///
/// 规则(词法扫描,字符串/正则字面量内外区分):
/// - 代码位置的 `|` 一律拒绝(print 管道、`|&` 协程、`cmd | getline`);
/// - `system` 函数与 `@load`(加载扩展 = 任意代码)拒绝;
/// - `>` 仅在 print/printf 语句内才是输出重定向(awk 语法:print 上下文
///   中的比较必须加括号),因此 `>` 出现在 print/printf 之后 → 拒绝;
///   其余位置的 `>` 是比较,放行;
/// - 正则字面量按表达式位置启发式识别(前一字符是操作符/括号/行首等)。
pub fn awk_program_is_safe(program: &str) -> bool {
    let b: Vec<char> = program.chars().collect();
    let n = b.len();
    let mut i = 0;
    let mut in_string = false;
    let mut in_regex = false;
    // 下一个 `/` 是正则开头还是除号:表达式位置(前一符号是操作符、
    // 括号、行首等)是正则;跟在标识符/数字/字符串/右括号后是除号
    let mut regex_can_start = true;
    // 当前语句是否处于 print/printf 的输出列表中(此时 > 是重定向)
    let mut in_print = false;

    while i < n {
        let c = b[i];
        if in_string {
            match c {
                '\\' => i += 2,
                '"' => {
                    in_string = false;
                    regex_can_start = false;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        if in_regex {
            match c {
                '\\' => i += 2,
                '/' => {
                    in_regex = false;
                    regex_can_start = false;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                i += 1;
            }
            '#' => {
                while i < n && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' => {
                if regex_can_start {
                    in_regex = true;
                }
                regex_can_start = true;
                i += 1;
            }
            // 代码位置的 | 一定是管道形态
            '|' => return false,
            '>' => {
                if in_print {
                    return false;
                }
                regex_can_start = true;
                i += 1;
            }
            '<' => {
                regex_can_start = true;
                i += 1;
            }
            ';' | '\n' | '{' | '}' => {
                in_print = false;
                regex_can_start = true;
                i += 1;
            }
            '(' | ',' | '~' | '?' | ':' => {
                regex_can_start = true;
                i += 1;
            }
            ')' | ']' => {
                regex_can_start = false;
                i += 1;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < n && (b[i].is_alphanumeric() || b[i] == '_') {
                    i += 1;
                }
                let ident: String = b[start..i].iter().collect();
                match ident.as_str() {
                    "print" | "printf" => in_print = true,
                    "system" | "load" => return false,
                    _ => {}
                }
                regex_can_start = false;
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                // 运算符/数字等:数字后跟 / 是除号
                if c.is_ascii_digit() {
                    let start = i;
                    while i < n && (b[i].is_ascii_digit() || b[i] == '.') {
                        i += 1;
                    }
                    regex_can_start = false;
                    let _ = start;
                } else {
                    regex_can_start = true;
                    i += 1;
                }
            }
        }
    }
    true
}

/// sed 脚本是否无副作用。
///
/// 逐命令解析:危险命令 `w`/`W`(写文件)、`r`/`R`(读入拼接)、`e`(执行
/// shell)、`s///e`(执行)、`s///w`(写文件)拒绝;未知命令拒绝。
/// 地址(数字、`$`、`/regex/`、`\XregexX`)与块 `{}` 跳过。
pub fn sed_script_is_safe(script: &str) -> bool {
    let b: Vec<char> = script.chars().collect();
    let n = b.len();
    let mut i = 0;

    loop {
        // 命令间分隔:空白与 `;`
        while i < n && (b[i].is_whitespace() || b[i] == ';') {
            i += 1;
        }
        if i == n {
            return true;
        }
        // 地址(可缺省,可为范围)
        i = match skip_sed_address(&b, i) {
            Some(next) => next,
            None => return false,
        };
        // 范围第二地址
        if i < n && b[i] == ',' {
            i = match skip_sed_address(&b, i + 1) {
                Some(next) => next,
                None => return false,
            };
        }
        // `!` 否定
        while i < n && b[i] == '!' {
            i += 1;
        }
        if i == n {
            return false;
        }
        let cmd = b[i];
        i += 1;
        match cmd {
            // 无参/标签类安全命令;`{`/`}` 是块
            'p' | 'P' | 'd' | 'D' | 'n' | 'N' | 'g' | 'G' | 'h' | 'H' | 'x' | '=' | 'l'
            | 'z' | 'F' | 'q' | 'Q' | 't' | 'T' | 'b' | 'v' | '{' | '}' => {}
            ':' => {
                while i < n && !b[i].is_whitespace() && b[i] != ';' && b[i] != '}' {
                    i += 1;
                }
            }
            // 文本参数(a/i/c):一行内其余都是文本,无 shell 语义
            'a' | 'i' | 'c' => {
                if i < n && b[i] == '\\' {
                    i += 1;
                }
                while i < n && b[i] != '\n' {
                    i += 1;
                }
            }
            's' | 'y' => {
                if i >= n || b[i] == '\\' || b[i] == '\n' {
                    return false;
                }
                let delim = b[i];
                i += 1;
                // 两个 span(模式与替换/映射),`\` 转义
                for _ in 0..2 {
                    let mut closed = false;
                    while i < n {
                        if b[i] == '\\' {
                            i += 2;
                            continue;
                        }
                        if b[i] == delim {
                            closed = true;
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    if !closed {
                        return false;
                    }
                }
                // flags:`w`(写文件)与 `e`(执行)拒绝
                while i < n && b[i].is_alphanumeric() {
                    match b[i] {
                        'g' | 'p' | 'i' | 'I' | 'm' | 'M' | 'o' | 'd' | '0'..='9' => i += 1,
                        _ => return false,
                    }
                }
            }
            'w' | 'W' | 'r' | 'R' | 'e' => return false,
            '#' => {
                while i < n && b[i] != '\n' {
                    i += 1;
                }
            }
            _ => return false,
        }
    }
}

/// 跳过一个 sed 地址:`$`、数字(`[step]~[step]`)、`/regex/`、`\XregexX`、
/// `+N`。无地址时原样返回。
fn skip_sed_address(b: &[char], mut i: usize) -> Option<usize> {
    let n = b.len();
    while i < n && b[i].is_whitespace() {
        i += 1;
    }
    if i >= n {
        return Some(i);
    }
    match b[i] {
        '$' => Some(i + 1),
        '+' => {
            // 范围相对偏移 ,+N
            let start = i;
            i += 1;
            while i < n && b[i].is_ascii_digit() {
                i += 1;
            }
            (i > start).then_some(i)
        }
        '0'..='9' => {
            while i < n && b[i].is_ascii_digit() {
                i += 1;
            }
            // first~step 形式
            if i < n && b[i] == '~' {
                i += 1;
                while i < n && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            Some(i)
        }
        '/' => {
            i += 1;
            while i < n {
                if b[i] == '\\' {
                    i += 2;
                    continue;
                }
                if b[i] == '/' {
                    return Some(i + 1);
                }
                i += 1;
            }
            None
        }
        '\\' => {
            i += 1;
            if i >= n {
                return None;
            }
            let delim = b[i];
            i += 1;
            while i < n {
                if b[i] == '\\' {
                    i += 2;
                    continue;
                }
                if b[i] == delim {
                    return Some(i + 1);
                }
                i += 1;
            }
            None
        }
        _ => Some(i),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awk_safe_subsets_pass() {
        assert!(awk_program_is_safe("NR<=3{print NR\": \"$0}"));
        assert!(awk_program_is_safe("NR>1{print}"));
        assert!(awk_program_is_safe("$1>2"));
        assert!(awk_program_is_safe("/foo|bar/{print $1}"));
        assert!(awk_program_is_safe(
            "NR>1{c[$11]+=$3} END{for(p in c) printf \"%6.2f%% cpu\", c[p], p}"
        ));
        assert!(awk_program_is_safe("{print $1, \"->\", $3}"));
        // print 上下文内的比较(需加括号的写法)误拒可接受,但比较本身
        // 在 print 之外必须放行
        assert!(awk_program_is_safe("BEGIN{x=a/b/c}"));
    }

    #[test]
    fn awk_side_effects_are_rejected() {
        assert!(!awk_program_is_safe("BEGIN{system(\"rm -rf x\")}"));
        assert!(!awk_program_is_safe("{system(\"sh -c x\")}"));
        assert!(!awk_program_is_safe("{print $1 > \"out.txt\"}"));
        assert!(!awk_program_is_safe("{print $1 >> \"out.txt\"}"));
        assert!(!awk_program_is_safe("{print | \"mail x\"}"));
        assert!(!awk_program_is_safe("\"sh\" | getline line"));
        assert!(!awk_program_is_safe("@load \"ext\""));
        // 字符串/正则里的危险字符无害
        assert!(awk_program_is_safe("{print \"system(|>)\"}"));
        assert!(awk_program_is_safe("/a|b|system/ {print}"));
    }

    #[test]
    fn sed_safe_subsets_pass() {
        assert!(sed_script_is_safe("4,6p"));
        assert!(sed_script_is_safe("1d"));
        assert!(sed_script_is_safe("s/a/b/g"));
        assert!(sed_script_is_safe("s|a|b|g"));
        assert!(sed_script_is_safe("y/abc/xyz/"));
        assert!(sed_script_is_safe("/^#/d"));
        assert!(sed_script_is_safe("1,3{=;p}"));
        assert!(sed_script_is_safe("$d"));
        assert!(sed_script_is_safe("s/\\//x/g"));
        assert!(sed_script_is_safe("10~2p"));
        assert!(sed_script_is_safe("2,+1p"));
    }

    #[test]
    fn sed_side_effects_are_rejected() {
        assert!(!sed_script_is_safe("s/a/b/w out.txt"));
        assert!(!sed_script_is_safe("s/a/b/e"));
        assert!(!sed_script_is_safe("w out.txt"));
        assert!(!sed_script_is_safe("W out.txt"));
        assert!(!sed_script_is_safe("r other.txt"));
        assert!(!sed_script_is_safe("2e date"));
        assert!(!sed_script_is_safe("/x/ s/a/b/w f"));
        assert!(!sed_script_is_safe("Qwq")); // 未知命令
    }
}
