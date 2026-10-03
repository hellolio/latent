//! shell 命令只读判定(13 文档 §6.2,纯函数):保守优先,宁可误判为非只读。
//! 这只是第一道筛 —— Plan 模式所有 bash(包括判定通过的)都在 ReadOnly OS
//! 沙箱内执行(13 文档 §15.1 双保险)。
//!
//! 判定是**词法级**的:先用 mini shell lexer 把命令切成词与操作符(引号、
//! 反斜杠转义、重定向、`$()`/反引号命令替换都在此层处理),再按顺序操作符
//! 分段逐段校验。对 awk/sed/find 这类"参数内容决定安全性"的命令,进一步
//! 解析脚本/谓词本身(interp 模块与 `find_words_are_safe`),只放行可证明
//! 无副作用的子集;拿不准一律拒绝。

use crate::permission::interp::{awk_program_is_safe, sed_script_is_safe};
use crate::permission::types::normalize_command;

/// 内置只读前缀表(初版;settings `approval.allowCommands` 可追加)。
/// find/fd/awk/sed 不在表内,由专用解析器处理。
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
    &["command", "-v"],
    // cargo check 会编译并执行依赖的 build.rs(第三方任意代码),不进只读表;
    // tree/metadata 只解析依赖图,不执行构建脚本
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
    &["df"],
    &["du"],
    &["ps"],
    &["sort"],
    &["uniq"],
    &["cut"],
    &["tr"],
    &["nl"],
    &["od"],
    &["xxd"],
    &["hexdump"],
    &["base64"],
    &["strings"],
    &["md5"],
    &["md5sum"],
    &["sha1sum"],
    &["sha256sum"],
    &["shasum"],
    &["cksum"],
    &["diff"],
    &["cmp"],
    &["comm"],
    &["join"],
    &["paste"],
    &["tac"],
    &["rev"],
    &["fold"],
    &["fmt"],
    &["expand"],
    &["column"],
    &["tree"],
    &["id"],
    &["groups"],
    &["basename"],
    &["dirname"],
    &["lsof"],
    &["uptime"],
    &["who"],
    &["sysctl"],
    // 系统配置/状态查询(写形态由前缀或专用参数校验挡住)
    &["defaults", "read"],
    &["pmset", "-g"],
    &["launchctl", "list"],
    &["security", "list-keychains"],
    &["netstat"],
    &["scutil", "--dns"],
    &["git", "stash", "list"],
    &["git", "clean", "-n"],
    &["git", "clean", "--dry-run"],
    &["mdutil", "-s"],
    &["csrutil", "status"],
    &["fdesetup", "status"],
    &["crontab", "-l"],
    &["kill", "-l"],
    // 零副作用命令。cd 自身无副作用,放行前提是分段校验仍要求其余所有段
    // 只读——cd 只影响后续只读命令看哪个目录;deny 规则否定优先,仍可禁掉。
    &["cd"],
    &["cal"],
    &["factor"],
    &["hostinfo"],
    &["arch"],
    &["otool"],
    &["sw_vers"],
];

/// `$()`/`-exec` 嵌套递归上限(防御病态嵌套)。
const MAX_SUBSTITUTION_DEPTH: usize = 4;

/// shell 命令安全判定结果(比只读二值判定更细,供 Plan 模式放行策略用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSafety {
    /// 可证明只读(内置只读表 / 专用解析器 / allow 规则)
    ReadOnly,
    /// 联网查询(curl/ping/dig 等,无本地写形态;Plan 模式放行)
    NetworkQuery,
    /// 明确写操作(写前缀表 / 输出重定向落盘 / 表内命令写形态 / deny 规则)
    Write,
    /// 无法判定(未知命令、展开命令名、解析失败)。Plan+沙箱放行交沙箱
    /// 裁决,无沙箱拒绝;Confirm 不放行、照常走审批。
    Unknown,
}

/// 明确写操作前缀表:命中即"明确知道是写操作"——Plan 模式即使有沙箱
/// 也不放行。只收主形态明确的命令;python/make/xargs 等执行体不确定的
/// 命令不进表(归 Unknown 交沙箱裁决)。只读表优先于本表,`kill -l`、
/// `git stash list`、`git clean -n` 等只读形态不受影响。
const WRITE_PREFIXES: &[&[&str]] = &[
    // 文件系统变更
    &["rm"],
    &["rmdir"],
    &["mv"],
    &["cp"],
    &["ln"],
    &["mkdir"],
    &["touch"],
    &["chmod"],
    &["chown"],
    &["chgrp"],
    &["dd"],
    &["tee"],
    &["shred"],
    &["truncate"],
    &["install"],
    &["mkfs"],
    // 进程控制(kill -l 只读形态由只读表先命中)
    &["kill"],
    &["pkill"],
    &["killall"],
    // git 写子命令(status/log/diff/stash list/clean -n 等只读形态由只读表先命中)
    &["git", "add"],
    &["git", "commit"],
    &["git", "push"],
    &["git", "pull"],
    &["git", "fetch"],
    &["git", "merge"],
    &["git", "rebase"],
    &["git", "reset"],
    &["git", "restore"],
    &["git", "checkout"],
    &["git", "switch"],
    &["git", "clean"],
    &["git", "stash"],
    &["git", "cherry-pick"],
    &["git", "revert"],
    &["git", "apply"],
    &["git", "am"],
    &["git", "rm"],
    &["git", "mv"],
    &["git", "init"],
    &["git", "clone"],
    &["git", "gc"],
    &["git", "prune"],
    &["git", "repack"],
    &["git", "worktree"],
    &["git", "submodule"],
    &["git", "bisect"],
    // 包管理写操作
    &["npm", "install"],
    &["npm", "uninstall"],
    &["npm", "remove"],
    &["npm", "ci"],
    &["npm", "update"],
    &["npm", "publish"],
    &["npm", "link"],
    &["npm", "unlink"],
    &["pip", "install"],
    &["pip", "uninstall"],
    &["pip3", "install"],
    &["pip3", "uninstall"],
    &["cargo", "install"],
    &["cargo", "uninstall"],
    &["cargo", "add"],
    &["cargo", "remove"],
    &["cargo", "new"],
    &["cargo", "init"],
    &["cargo", "clean"],
    &["brew", "install"],
    &["brew", "uninstall"],
    &["brew", "upgrade"],
    &["brew", "reinstall"],
    &["brew", "link"],
    &["brew", "unlink"],
    &["apt", "install"],
    &["apt", "remove"],
    &["apt", "upgrade"],
    &["apt", "autoremove"],
    &["apt-get", "install"],
    &["apt-get", "remove"],
    &["apt-get", "upgrade"],
    &["apt-get", "update"],
];

/// 联网查询前缀表(Plan 模式放行;Confirm 照常审批)。curl 的落盘/上传
/// 形态(`-o`/`-O`/`--output`/`-T`/`--upload-file`)是明确本地写,不算查询;
/// wget 默认下载落盘,不进表(归 Unknown 交沙箱裁决)。
const NETWORK_QUERY_PREFIXES: &[&[&str]] = &[
    &["curl"],
    &["ping"],
    &["ping6"],
    &["dig"],
    &["nslookup"],
    &["host"],
    &["whois"],
    &["traceroute"],
    &["tracepath"],
];

/// 三态判定一条 shell 命令(Plan 模式放行策略的依据)。
pub fn classify_shell_command(command: &str, allow: &[String], deny: &[String]) -> ShellSafety {
    check_command_class(command, allow, deny, 0)
}

/// 判定一条 shell 命令是否只读(可安全免审)。
///
/// 规则(13 文档 §6.2):
/// 1. 词法层一票否决:输出重定向到非 /dev/null 目标、`<(` 进程替换、
///    heredoc/here-string、换行、单个 `&`(后台)、未闭合引号、命令替换
///    内层不通过递归校验;
/// 2. 按顺序分隔符 `|`/`||`/`&&`/`;` 分段,每段都通过 3/4 才算只读;
/// 3. `env VAR=x cmd`、`sudo -n cmd` 剥离、`git` 全局 flag 剥离后命中内置
///    只读前缀表 → 只读;awk/sed/find 走专用解析器;
/// 4. deny 规则(`approval.denyCommands`)命中一律非只读(否定优先)。
pub fn is_readonly_command_with_rules(
    command: &str,
    allow: &[String],
    deny: &[String],
) -> bool {
    matches!(
        classify_shell_command(command, allow, deny),
        ShellSafety::ReadOnly
    )
}

/// 无规则便捷形态(测试与内置使用)。
pub fn is_readonly_command(command: &str) -> bool {
    is_readonly_command_with_rules(command, &[], &[])
}

/// 词法失败时区分"明确写"(输出重定向落盘)与"无法判定"。
fn check_command_class(command: &str, allow: &[String], deny: &[String], depth: usize) -> ShellSafety {
    if depth > MAX_SUBSTITUTION_DEPTH {
        return ShellSafety::Unknown;
    }
    let mut redirect_write = false;
    let Some(tokens) = tokenize(command, allow, deny, depth, &mut redirect_write) else {
        return if redirect_write {
            ShellSafety::Write
        } else {
            ShellSafety::Unknown
        };
    };
    let mut merged = ShellSafety::ReadOnly;
    let mut segment: Vec<ShellWord> = Vec::new();
    for token in tokens {
        match token {
            Tok::Word(word) => segment.push(word),
            Tok::Seq | Tok::Pipe | Tok::Or | Tok::And => {
                merged = merge_safety(merged, segment_class(&segment, allow, deny, depth));
                segment.clear();
            }
        }
    }
    merge_safety(merged, segment_class(&segment, allow, deny, depth))
}

/// 布尔形态便捷包装(命令替换内层递归校验用:内层非只读 = 外层无法放行)。
fn check_command(command: &str, allow: &[String], deny: &[String], depth: usize) -> bool {
    matches!(
        check_command_class(command, allow, deny, depth),
        ShellSafety::ReadOnly
    )
}

/// 段级安全合并:Write 否定优先,Unknown 次之(无法判定不能洗白整条命令),
/// ReadOnly 可被 NetworkQuery 升级。
fn merge_safety(a: ShellSafety, b: ShellSafety) -> ShellSafety {
    match (a, b) {
        (ShellSafety::Write, _) | (_, ShellSafety::Write) => ShellSafety::Write,
        (ShellSafety::ReadOnly, rest) | (rest, ShellSafety::ReadOnly) => rest,
        (ShellSafety::Unknown, _) | (_, ShellSafety::Unknown) => ShellSafety::Unknown,
        _ => ShellSafety::NetworkQuery,
    }
}

// ---- mini shell lexer ----

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(ShellWord),
    /// `;` 或换行
    Seq,
    Pipe,
    Or,
    And,
}

#[derive(Debug, Clone, PartialEq)]
struct ShellWord {
    /// 字面文本(引号与反斜杠已处理;已递归验证的命令替换占位为 X)
    text: String,
    /// 含变量/参数展开(`$x`):值运行期才确定;命令名位置出现 → 拒绝
    has_expansion: bool,
}

impl ShellWord {
    fn new() -> Self {
        ShellWord {
            text: String::new(),
            has_expansion: false,
        }
    }
}

fn tokenize(
    command: &str,
    allow: &[String],
    deny: &[String],
    depth: usize,
    redirect_write: &mut bool,
) -> Option<Vec<Tok>> {
    let mut tokens = Vec::new();
    let mut word = ShellWord::new();
    let mut in_word = false;
    let mut chars = command.chars().peekable();

    macro_rules! flush {
        () => {
            if in_word {
                tokens.push(Tok::Word(word.clone()));
                word = ShellWord::new();
                in_word = false;
            }
        };
    }

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // 单引号:一切字面
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => word.text.push(ch),
                        None => return None,
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            // 双引号内仅 \" \$ \` \\ 是转义,其余 `\` 保留字面
                            Some(escaped @ ('$' | '`' | '"' | '\\')) => word.text.push(escaped),
                            Some(other) => {
                                word.text.push('\\');
                                word.text.push(other);
                            }
                            None => return None,
                        },
                        // 双引号内 ` 与 $( 仍会展开,按替换验证
                        Some('$') => {
                            if !take_substitution(&mut chars, &mut word, allow, deny, depth) {
                                return None;
                            }
                        }
                        Some('`') => {
                            if !take_backtick(&mut chars, &mut word, allow, deny, depth) {
                                return None;
                            }
                        }
                        Some(ch) => word.text.push(ch),
                        None => return None,
                    }
                }
            }
            '\\' => {
                // 反斜杠转义下一字符(字面,非操作符):`\;` `\|` `\(` 等
                in_word = true;
                word.text.push(chars.next()?);
            }
            '$' => {
                in_word = true;
                if !take_substitution(&mut chars, &mut word, allow, deny, depth) {
                    return None;
                }
            }
            '`' => {
                in_word = true;
                if !take_backtick(&mut chars, &mut word, allow, deny, depth) {
                    return None;
                }
            }
            '>' | '<' => {
                // fd 数字前缀(2>/2<):独立数字紧跟重定向符是 IO number,
                // 清词并复位 in_word,避免 flush 推入空词 token
                if in_word
                    && !word.text.is_empty()
                    && word.text.chars().all(|c| c.is_ascii_digit())
                {
                    word.text.clear();
                    in_word = false;
                }
                flush!();
                if c == '>' {
                    // 输出重定向:仅 /dev/null 与 fd 复制(>&N)无害
                    if !take_output_redirect(&mut chars) {
                        // 落盘重定向是明确写,供三态判定区分
                        *redirect_write = true;
                        return None;
                    }
                } else {
                    // 输入重定向只读;`<(` 进程替换、`<</<<<` 依赖执行体,否决
                    if matches!(chars.peek(), Some('(') | Some('<')) {
                        return None;
                    }
                }
            }
            '|' => {
                flush!();
                if chars.peek() == Some(&'|') {
                    chars.next();
                    tokens.push(Tok::Or);
                } else {
                    tokens.push(Tok::Pipe);
                }
            }
            ';' | '\n' | '\r' => {
                flush!();
                tokens.push(Tok::Seq);
            }
            '&' => {
                flush!();
                match chars.peek() {
                    Some('&') => {
                        chars.next();
                        tokens.push(Tok::And);
                    }
                    // 单个 & 后台执行(含 `&>`)一票否决
                    _ => return None,
                }
            }
            c if c.is_whitespace() => flush!(),
            c => {
                in_word = true;
                word.text.push(c);
            }
        }
    }
    // 末段入列:不再走 flush!(循环结束后复位 word/in_word 是无效赋值)
    if in_word {
        tokens.push(Tok::Word(word));
    }
    Some(tokens)
}

/// 消费 `$` 之后的替换:`$(...)` 递归验证内层命令后占位为 X;
/// `$((...))` 算术展开无命令执行语义,直接放行;其余 `$x` 标记展开。
fn take_substitution(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    word: &mut ShellWord,
    allow: &[String],
    deny: &[String],
    depth: usize,
) -> bool {
    if chars.peek() != Some(&'(') {
        word.has_expansion = true;
        word.text.push('$');
        return true;
    }
    chars.next();
    // `$((...))` 算术展开:结果恒为数值,无副作用(第一个 `(` 已消费)。
    // 但 bash 允许算术体内嵌命令替换 `$(( $(cmd) ))` 与反引号,内层必须
    // 拦截(第一个 `(` 已消费,剩余深度从 1 起)
    if chars.peek() == Some(&'(') {
        return skip_arithmetic_body(chars, 1);
    }
    let Some(inner) = take_balanced_substitution_body(chars) else {
        return false;
    };
    if check_command(&inner, allow, deny, depth + 1) {
        word.text.push('X');
        true
    } else {
        false
    }
}

/// 反引号替换:取到配对反引号,内层递归验证(不支持嵌套反引号)。
fn take_backtick(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    word: &mut ShellWord,
    allow: &[String],
    deny: &[String],
    depth: usize,
) -> bool {
    let mut inner = String::new();
    loop {
        match chars.next() {
            Some('`') => break,
            Some('\\') => match chars.next() {
                Some(escaped) => inner.push(escaped),
                None => return false,
            },
            Some(c) => inner.push(c),
            None => return false,
        }
    }
    if inner.trim().is_empty() {
        return false;
    }
    if check_command(&inner, allow, deny, depth + 1) {
        word.text.push('X');
        true
    } else {
        false
    }
}

/// 取 `$(` 到配对 `)` 之间的原文(引号内的 `)` 不算)。
fn take_balanced_substitution_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    let mut inner = String::new();
    let mut paren_depth = 1usize;
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some('"') => match c {
                '"' => quote = None,
                '\\' => {
                    inner.push(c);
                    inner.push(chars.next()?);
                    continue;
                }
                _ => inner.push(c),
            },
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                }
                inner.push(c);
            }
            _ => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    inner.push(c);
                }
                '(' => {
                    paren_depth += 1;
                    inner.push(c);
                }
                ')' => {
                    paren_depth -= 1;
                    if paren_depth == 0 {
                        if inner.trim().is_empty() {
                            return None;
                        }
                        return Some(inner);
                    }
                    inner.push(c);
                }
                _ => inner.push(c),
            },
        }
    }
    None
}

/// 算术展开体消费:括号配对扫描,但反引号与 `$`(命令替换)一票否决
/// (bash 算术体内可嵌套命令执行;嵌套算术 `$(( $((1)) ))` 也被保守拒绝)。
fn skip_arithmetic_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, opened: usize) -> bool {
    let mut depth = opened;
    while let Some(c) = chars.next() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
            '`' => return false,
            '$' if chars.peek() == Some(&'(') => return false,
            _ => {}
        }
    }
    false
}

/// `>` 的目标只允许 `/dev/null` 与 fd 复制(`>&N`);其余可能写文件。
fn take_output_redirect(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
    match chars.peek().copied() {
        Some('/') => {
            let mut lookahead = chars.clone();
            let mut candidate = String::new();
            for _ in 0.."/dev/null".len() {
                match lookahead.next() {
                    Some(c) => candidate.push(c),
                    None => break,
                }
            }
            if candidate != "/dev/null" {
                return false;
            }
            for _ in 0..candidate.len() {
                chars.next();
            }
            // 边界:后随分隔符/空白/结尾(/dev/nullfoo 是写文件)
            match chars.peek() {
                None => true,
                Some(c) => c.is_whitespace() || matches!(c, '|' | '&' | ';' | '>' | '<'),
            }
        }
        Some('&') => {
            chars.next();
            matches!(chars.next(), Some(d) if d.is_ascii_digit())
        }
        _ => false,
    }
}

// ---- 分段校验 ----

/// 单段命令的三态判定:deny 否定优先 → allow 规则 → 包装剥离 + 内置表/
/// 专用解析器 → 写前缀表 → 联网查询表,落不进任何一类的归 Unknown。
fn segment_class(
    words: &[ShellWord],
    allow: &[String],
    deny: &[String],
    depth: usize,
) -> ShellSafety {
    if words.is_empty() {
        return ShellSafety::Unknown;
    }
    let normalized = words
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    // 否定优先于白名单;命中 deny 规则按明确写处理(判定层面即拒绝)
    if deny
        .iter()
        .any(|rule| prefix_matches(&normalized, &normalize_command(rule)))
    {
        return ShellSafety::Write;
    }
    let stripped = strip_wrappers(words);
    if stripped.is_empty() || stripped[0].has_expansion {
        // 命令名来自展开,值运行期才确定
        return ShellSafety::Unknown;
    }
    let texts: Vec<&str> = stripped.iter().map(|word| word.text.as_str()).collect();
    // 脚本型命令:安全性取决于参数内容,整体解析;解析不通过归 Unknown
    // (可能是明确写形态也可能是拿不准,由沙箱/审批兜底)
    match texts[0] {
        "awk" => return bool_safety(awk_words_are_safe(&stripped)),
        "sed" => return bool_safety(sed_words_are_safe(&stripped)),
        "find" | "fd" => {
            return bool_safety(find_words_are_safe(&stripped, allow, deny, depth))
        }
        "nvram" => return bool_safety(nvram_words_are_safe(&stripped)),
        "ifconfig" => return bool_safety(ifconfig_words_are_safe(&stripped)),
        "route" => return bool_safety(route_words_are_safe(&stripped)),
        "arp" => return bool_safety(arp_words_are_safe(&stripped)),
        "dscl" => return bool_safety(dscl_words_are_safe(&stripped)),
        _ => {}
    }
    // allow 规则:用户显式声明的免审命令按只读放行
    if allow
        .iter()
        .any(|rule| prefix_matches(&normalized, &normalize_command(rule)))
    {
        return ShellSafety::ReadOnly;
    }
    // 无参 mount 只打印挂载点(带参是真挂载,不进表)
    if texts.len() == 1 && texts[0] == "mount" {
        return ShellSafety::ReadOnly;
    }
    if READONLY_PREFIXES
        .iter()
        .any(|prefix| token_prefix_matches(&texts, prefix))
    {
        // 表内命令自带的写形态(`sort -o`/`sysctl -w`/`git tag v1`)是明确写
        return if has_write_flag(&texts) {
            ShellSafety::Write
        } else {
            ShellSafety::ReadOnly
        };
    }
    if WRITE_PREFIXES
        .iter()
        .any(|prefix| token_prefix_matches(&texts, prefix))
    {
        return ShellSafety::Write;
    }
    // curl 的落盘/上传形态是明确本地写,先于联网查询判定
    if texts[0] == "curl" && curl_has_write_form(&texts) {
        return ShellSafety::Write;
    }
    if is_network_query_segment(&texts) {
        return ShellSafety::NetworkQuery;
    }
    ShellSafety::Unknown
}

/// 专用解析器布尔结果 → 三态(不通过归 Unknown 而非 Write:解析失败
/// 不等于证明了写副作用)。
fn bool_safety(safe: bool) -> ShellSafety {
    if safe {
        ShellSafety::ReadOnly
    } else {
        ShellSafety::Unknown
    }
}

/// curl 的落盘/上传形态(`-o`/`-O`/`--output`/`-T`/`--upload-file` 等):
/// 明确写本地文件,不算联网查询。
fn curl_has_write_form(tokens: &[&str]) -> bool {
    tokens[1..].iter().any(|arg| {
        arg.starts_with("-o")
            || arg.starts_with("-O")
            || *arg == "-J"
            || arg.starts_with("-T")
            || arg.starts_with("--output")
            || arg.starts_with("--upload-file")
            || arg.starts_with("--remote-name")
    })
}

/// 单段命令是否联网查询(Plan 模式放行;curl 的落盘/上传形态除外)。
fn is_network_query_segment(tokens: &[&str]) -> bool {
    if !NETWORK_QUERY_PREFIXES
        .iter()
        .any(|prefix| token_prefix_matches(tokens, prefix))
    {
        return false;
    }
    tokens[0] != "curl" || !curl_has_write_form(tokens)
}

/// 单段命令的只读判定(deny 否定优先,再走包装剥离 + 内置表/专用解析器,
/// 最后查 allow 规则)。
fn segment_is_readonly(
    words: &[ShellWord],
    allow: &[String],
    deny: &[String],
    depth: usize,
) -> bool {
    matches!(
        segment_class(words, allow, deny, depth),
        ShellSafety::ReadOnly
    )
}

/// 表内命令自带的写文件 flag(`sort -o`)或写内核态(`sysctl -w`)使该段非只读;
/// git 家族另有子命令级形态守卫(见 [`git_words_have_write_flag`])。
fn has_write_flag(tokens: &[&str]) -> bool {
    match tokens {
        ["sort", args @ ..] => args.iter().any(|a| *a == "-o" || a.starts_with("--o")),
        ["sysctl", args @ ..] => args.contains(&"-w"),
        ["git", ..] => git_words_have_write_flag(tokens),
        _ => false,
    }
}

/// git 只读子命令的写形态守卫:
/// - `tag`/`branch`:非 flag 参数(创建/改名 ref)与写 flag(-d/-D/-m/-M/
///   -a/-s/-u/-c/-C/--delete/--force 等)拒绝;只放行已知只读 flag。
///   `git tag -l 'v*'` 的 pattern 形式被保守拒绝(可 `git tag | grep` 替代)。
/// - `remote`:只放行无参(列表)、`-v`/`--verbose`、`get-url`/`show` 子命令;
///   add/remove/update/prune 等写 `.git/config` 或联网改写拒绝。
/// - 全家族:`--output <path>`/`--output=<path>`(diff/show 输出落盘到
///   任意路径)拒绝;`--output-indicator-*` 不受影响。
fn git_words_have_write_flag(tokens: &[&str]) -> bool {
    const REF_READ_FLAGS: &[&str] = &[
        "-l", "--list", "-a", "--all", "-r", "--remotes", "-v", "-vv", "--verbose",
        "--show-current", "--contains", "--no-contains", "--merged", "--no-merged",
        "--points-at", "--column", "-i", "--ignore-case",
    ];
    let Some(sub) = tokens.get(1..) else {
        return false;
    };
    match sub {
        [] => false,
        ["tag", args @ ..] | ["branch", args @ ..] => !args.iter().copied().all(|a| {
            REF_READ_FLAGS.contains(&a)
                || a.starts_with("--sort=")
                || a.starts_with("--format=")
                || a.starts_with("--contains=")
                || a.starts_with("--no-contains=")
                || a.starts_with("--merged=")
                || a.starts_with("--no-merged=")
                || a.starts_with("--points-at=")
                || a == "-n"
                || (a.starts_with("-n")
                    && a.len() > 2
                    && a[2..].chars().all(|c| c.is_ascii_digit()))
        }),
        ["remote", args @ ..] => !args.is_empty()
            && !args.iter().all(|a| matches!(*a, "-v" | "--verbose"))
            && !(matches!(args[0], "get-url" | "show")
                && args[1..].iter().all(|a| !a.starts_with('-'))),
        _ => sub.iter().any(|a| *a == "--output" || a.starts_with("--output=")),
    }
}

/// awk 参数解析:只放行安全选项(`-F`/`-v`/`-e`/`--source`),拒绝
/// `-f`(脚本文件)与未知选项(gawk `-i inplace` 等);程序文本交给
/// [`awk_program_is_safe`]。
fn awk_words_are_safe(words: &[ShellWord]) -> bool {
    let mut i = 1;
    let mut program: Option<String> = None;
    while i < words.len() {
        let word = &words[i];
        let text = word.text.as_str();
        if text == "--" {
            i += 1;
            break;
        }
        if let Some(value) = text.strip_prefix("--source=") {
            if word.has_expansion {
                return false;
            }
            program = Some(value.to_string());
            i += 1;
            continue;
        }
        if text == "--source" {
            if i + 1 < words.len() && !words[i + 1].has_expansion {
                program = Some(words[i + 1].text.clone());
                i += 2;
                continue;
            }
            return false;
        }
        if text.starts_with("--") {
            return false;
        }
        if text.starts_with("-F") {
            i += if text.len() == 2 { 2 } else { 1 };
            continue;
        }
        if text.starts_with("-v") {
            i += if text.len() == 2 { 2 } else { 1 };
            continue;
        }
        if text == "-e" {
            if i + 1 < words.len() && !words[i + 1].has_expansion {
                program = Some(words[i + 1].text.clone());
                i += 2;
                continue;
            }
            return false;
        }
        if text.starts_with('-') && text.len() > 1 {
            // -f(脚本文件)、-i(inplace)、未知短选项
            return false;
        }
        // 首个非选项词 = 程序
        if word.has_expansion {
            return false;
        }
        program = Some(word.text.clone());
        i += 1;
        break;
    }
    let Some(program) = program else {
        return false;
    };
    if !awk_program_is_safe(&program) {
        return false;
    }
    // 其余是输入文件;再出现选项形态的词保守拒绝
    words[i..]
        .iter()
        .all(|word| !word.text.starts_with('-') || word.text == "-")
}

/// sed 参数解析:安全选项集合 `-n -E -r -s -z -u -b`;拒绝 `-i`/`-f`/
/// 未知选项;脚本文本(首参或 `-e` 值)交给 [`sed_script_is_safe`]。
fn sed_words_are_safe(words: &[ShellWord]) -> bool {
    let mut i = 1;
    let mut scripts: Vec<String> = Vec::new();
    let mut saw_expression = false;
    while i < words.len() {
        let word = &words[i];
        let text = word.text.as_str();
        if text == "--" {
            i += 1;
            break;
        }
        if let Some(value) = text.strip_prefix("--expression=") {
            if word.has_expansion {
                return false;
            }
            scripts.push(value.to_string());
            saw_expression = true;
            i += 1;
            continue;
        }
        if text == "--expression" {
            if i + 1 < words.len() && !words[i + 1].has_expansion {
                scripts.push(words[i + 1].text.clone());
                saw_expression = true;
                i += 2;
                continue;
            }
            return false;
        }
        if text.starts_with("--") {
            return false;
        }
        if let Some(flag) = text.strip_prefix('-') {
            if flag.is_empty() {
                break; // "-" 是 stdin 文件名
            }
            if flag.chars().all(|c| "nErSzub".contains(c)) {
                i += 1;
                continue;
            }
            if flag == "e" {
                if i + 1 < words.len() && !words[i + 1].has_expansion {
                    scripts.push(words[i + 1].text.clone());
                    saw_expression = true;
                    i += 2;
                    continue;
                }
                return false;
            }
            return false;
        }
        break;
    }
    // 无 -e 时首个非选项词是脚本
    if !saw_expression && i < words.len() {
        if words[i].has_expansion {
            return false;
        }
        scripts.push(words[i].text.clone());
        i += 1;
    }
    // 脚本之后是输入文件;GNU sed 允许脚本后置选项(`sed 's/a/b/' -i f`
    // 会就地写文件),选项形态一律保守拒绝("-" 是 stdin 文件名)
    words[i..]
        .iter()
        .all(|word| !word.text.starts_with('-') || word.text == "-")
        && !scripts.is_empty()
        && scripts.iter().all(|script| sed_script_is_safe(script))
}

/// find/fd 参数解析:写谓词拒绝;`-exec` 族(exec/execdir/ok/okdir、
/// fd 的 -x/--exec/--exec-batch)的命令体递归走同一段校验。
fn find_words_are_safe(
    words: &[ShellWord],
    allow: &[String],
    deny: &[String],
    depth: usize,
) -> bool {
    const WRITE_PREDICATES: &[&str] =
        &["-delete", "-fls", "-fprint", "-fprint0", "-fprintf"];
    const EXEC_PREDICATES: &[&str] = &[
        "-exec", "-execdir", "-ok", "-okdir", "-x", "--exec", "--exec-batch",
    ];
    let mut i = 1;
    while i < words.len() {
        let text = words[i].text.as_str();
        if WRITE_PREDICATES.contains(&text) {
            return false;
        }
        if EXEC_PREDICATES.contains(&text) {
            // 命令体到终止符 `;`(源自 `\;`)或 `+`
            let mut j = i + 1;
            while j < words.len() && words[j].text != ";" && words[j].text != "+" {
                j += 1;
            }
            if j >= words.len() || j == i + 1 {
                return false;
            }
            if !segment_is_readonly(&words[i + 1..j], allow, deny, depth + 1) {
                return false;
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    true
}

/// nvram:只放行打印形态(`-p`/`-x`/裸变量名);`-d`/`-c`(删除)与
/// `name=value`(写入)拒绝。
fn nvram_words_are_safe(words: &[ShellWord]) -> bool {
    words[1..].iter().all(|word| {
        let text = word.text.as_str();
        text == "-p"
            || text == "-x"
            || (!text.starts_with('-') && !text.contains('='))
    })
}

/// ifconfig:无参 = 列出全部;参数只能是接口名(小写字母开头、纯字母
/// 数字、必含数字)。动作词(up/down/inet/alias/mtu…)与 IP 地址都不
/// 满足该特征,`en0 down`/`en0 1.2.3.4` 由此拒绝。
fn ifconfig_words_are_safe(words: &[ShellWord]) -> bool {
    words[1..].iter().all(|word| {
        let text = word.text.as_str();
        text == "-l"
            || (text.starts_with(|c: char| c.is_ascii_lowercase())
                && text.chars().all(|c| c.is_ascii_alphanumeric())
                && text.contains(|c: char| c.is_ascii_digit()))
    })
}

/// route:只放行查询形态 `route [-n|-q|-v] get [目标...]`;add/change/
/// delete/flush 等拒绝。
fn route_words_are_safe(words: &[ShellWord]) -> bool {
    let mut seen_get = false;
    for word in &words[1..] {
        match word.text.as_str() {
            "-n" | "-q" | "-v" if !seen_get => {}
            "get" => seen_get = true,
            // get 之后全是查询目标(读)
            _ if seen_get => {}
            _ => return false,
        }
    }
    seen_get
}

/// arp:只放行报表 flag(`-a`/`-n`);`-s`(写表项)/`-d`(删表项)/`-f` 拒绝。
fn arp_words_are_safe(words: &[ShellWord]) -> bool {
    words[1..]
        .iter()
        .all(|word| matches!(word.text.as_str(), "-a" | "-n"))
}

/// dscl:只放行读操作(`-read`/`-readall`/`-list`/`-search`);
/// -create/-delete/-append/-merge/-change/-passwd 等写操作拒绝。
fn dscl_words_are_safe(words: &[ShellWord]) -> bool {
    let mut has_read = false;
    for word in &words[1..] {
        let text = word.text.as_str();
        if text.starts_with('-') {
            if matches!(text, "-read" | "-readall" | "-list" | "-search") {
                has_read = true;
            } else {
                return false;
            }
        }
    }
    has_read
}

/// 剥离 `env VAR=x` 与 `sudo -n` 包装(可嵌套,如 `sudo -n env FOO=1 ls`),
/// 以及 `git` 全局 flag(`-C path`、`--git-dir=...`;`-c k=v` 不剥离,见上)。
/// env 变量名命中 [`env_var_spawns_programs`] 时停止剥离(整段保守拒绝)。
fn strip_wrappers(words: &[ShellWord]) -> Vec<ShellWord> {
    let mut current: Vec<ShellWord> = words.to_vec();
    loop {
        let Some(head) = current.first() else {
            return current;
        };
        match head.text.as_str() {
            "env" => {
                let after = &current[1..];
                let mut index = 0;
                while index < after.len() {
                    let text = after[index].text.as_str();
                    if text.contains('=')
                        && env_var_spawns_programs(text.split('=').next().unwrap_or(""))
                    {
                        // 危险变量(GIT_PAGER/LD_PRELOAD 等可注入可执行程序):
                        // 停止剥离,残留 token 不命中前缀表 → 非只读
                        break;
                    } else if text.contains('=') || text.starts_with("--") {
                        index += 1;
                    } else if text == "-u" && index + 1 < after.len() {
                        index += 2;
                    } else if !text.starts_with('-') {
                        break;
                    } else {
                        index += 1;
                    }
                }
                if index == after.len() {
                    // 裸 env 保留自身(只打印环境变量);仅 flag 的 env 不继续
                    return if after.is_empty() {
                        current
                    } else {
                        current[1 + index..].to_vec()
                    };
                }
                current = current[1 + index..].to_vec();
            }
            "sudo" if current.get(1).is_some_and(|word| word.text == "-n") => {
                current = current[2..].to_vec();
            }
            "git" if current.get(1).is_some_and(|word| is_git_global_flag(&word.text)) => {
                let mut index = 1;
                while index < current.len() {
                    let text = current[index].text.as_str();
                    // 注意:`-c k=v` 不剥离 —— git 配置项可注入可执行程序
                    // (core.pager/diff.external/core.fsmonitor 等),保守整体
                    // 拒绝;后续可演进为可执行配置键黑名单
                    if text == "-C" && index + 1 < current.len() {
                        index += 2;
                    } else if text.starts_with("--") && text.contains('=') {
                        index += 1;
                    } else if matches!(
                        text,
                        "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix"
                    ) && index + 1 < current.len()
                    {
                        index += 2;
                    } else {
                        break;
                    }
                }
                current = current[..1]
                    .iter()
                    .chain(current[index..].iter())
                    .cloned()
                    .collect();
            }
            "arch" if current.len() > 1 => {
                // arch 是包装器:`arch [-arch 类型|-32|-64|...] program args`
                // 以指定架构执行 program。剥掉自身与 flag 后对内层命令
                // 重新走完整校验;剥完为空 = 裸打印形态
                let mut index = 1;
                while index < current.len() {
                    let text = current[index].text.as_str();
                    if text == "-arch" && index + 1 < current.len() {
                        index += 2;
                    } else if text.starts_with('-') {
                        index += 1;
                    } else {
                        break;
                    }
                }
                current = current[index..].to_vec();
            }
            _ => return current,
        }
    }
}

fn is_git_global_flag(text: &str) -> bool {
    text == "-C"
        || text.starts_with("--git-dir")
        || text.starts_with("--work-tree")
        || text.starts_with("--namespace")
        || text.starts_with("--super-prefix")
}

/// env 变量名会否被下游命令用来 spawn 外部程序:`GIT_PAGER`/`GIT_EDITOR`/
/// `GIT_EXTERNAL_DIFF` 等 GIT 家族、通用 `PAGER`/`EDITOR`/`VISUAL`,以及
/// 动态链接器注入(`LD_PRELOAD`/`DYLD_*`)与 shell 启动文件(`BASH_ENV`/`ENV`)。
fn env_var_spawns_programs(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("GIT_")
        || matches!(
            upper.as_str(),
            "PAGER" | "EDITOR" | "VISUAL" | "BASH_ENV" | "ENV" | "LD_PRELOAD"
        )
        || upper.starts_with("DYLD_")
}

/// tokens 是否以 prefix tokens 开头。
fn token_prefix_matches(tokens: &[&str], prefix: &[&str]) -> bool {
    tokens.len() >= prefix.len() && tokens[..prefix.len()] == *prefix
}

/// 字符串前缀匹配(词边界:prefix 后必须是结尾或空白)。
pub(crate) fn prefix_matches(command: &str, prefix: &str) -> bool {
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
        assert!(is_readonly_command("cargo tree"));
        assert!(is_readonly_command("echo hello"));
        assert!(is_readonly_command("  pwd  "));
        assert!(is_readonly_command("wc -l foo.txt"));
        // cargo check 会执行依赖的 build.rs,不进只读表
        assert!(!is_readonly_command("cargo check"));
    }

    #[test]
    fn classify_tri_state() {
        use ShellSafety::{NetworkQuery, ReadOnly, Unknown, Write};
        // 只读
        assert_eq!(classify_shell_command("git log", &[], &[]), ReadOnly);
        // 联网查询:Plan 模式放行
        assert_eq!(
            classify_shell_command("curl -s https://api.example.com/v1 | head -5", &[], &[]),
            NetworkQuery
        );
        assert_eq!(classify_shell_command("ping -c 2 example.com", &[], &[]), NetworkQuery);
        assert_eq!(classify_shell_command("dig +short example.com", &[], &[]), NetworkQuery);
        // curl 落盘/上传形态是明确写
        assert_eq!(
            classify_shell_command("curl -o out.bin https://example.com", &[], &[]),
            Write
        );
        assert_eq!(
            classify_shell_command("curl -T file https://example.com/up", &[], &[]),
            Write
        );
        // 落盘重定向是明确写(词法层识别)
        assert_eq!(classify_shell_command("curl https://example.com > out.txt", &[], &[]), Write);
        // 明确写前缀
        assert_eq!(classify_shell_command("rm -rf build", &[], &[]), Write);
        assert_eq!(classify_shell_command("git push origin main", &[], &[]), Write);
        assert_eq!(classify_shell_command("npm install", &[], &[]), Write);
        // 只读形态不被写前缀误伤(只读表先命中)
        assert_eq!(classify_shell_command("kill -l", &[], &[]), ReadOnly);
        assert_eq!(classify_shell_command("git stash list", &[], &[]), ReadOnly);
        assert_eq!(classify_shell_command("git clean -n", &[], &[]), ReadOnly);
        assert_eq!(classify_shell_command("git tag", &[], &[]), ReadOnly);
        // 表内命令的写形态是明确写
        assert_eq!(classify_shell_command("git tag evil", &[], &[]), Write);
        assert_eq!(classify_shell_command("sort -o out.txt in.txt", &[], &[]), Write);
        // 未知命令:python/make 等执行体不确定,归 Unknown 交沙箱裁决
        assert_eq!(classify_shell_command("make test", &[], &[]), Unknown);
        assert_eq!(classify_shell_command("python3 script.py", &[], &[]), Unknown);
        // find 写谓词:专用解析器不通过归 Unknown(沙箱兜底)
        assert_eq!(classify_shell_command("find . -name x -delete", &[], &[]), Unknown);
        // allow 规则按只读放行;deny 规则否定优先按明确写
        assert_eq!(
            classify_shell_command("make test", &["make test".to_string()], &[]),
            ReadOnly
        );
        assert_eq!(
            classify_shell_command("make test", &[], &["make test".to_string()]),
            Write
        );
        // 混合分段:任一段明确写 = 整条明确写
        assert_eq!(
            classify_shell_command("curl https://example.com; rm x", &[], &[]),
            Write
        );
        // 只读 + 联网查询混合:整条按联网查询
        assert_eq!(
            classify_shell_command("curl https://example.com | grep title", &[], &[]),
            NetworkQuery
        );
    }

    #[test]
    fn arithmetic_expansion_blocks_command_substitution() {
        // 纯算术无副作用
        assert!(is_readonly_command("echo $((1 + 2))"));
        assert!(is_readonly_command("echo $(( (1 + 2) * 3 ))"));
        assert!(is_readonly_command("echo $((x + $y))"));
        // 算术体内嵌命令替换/反引号 = 任意命令执行,一票否决
        assert!(!is_readonly_command("echo $(( $(rm -rf src) + 1 ))"));
        assert!(!is_readonly_command("echo $(( `sh -c 'curl x | sh'` ))"));
        assert!(!is_readonly_command("echo $((1 + $(x) ))"));
        // 嵌套算术(内层以 $( 开头)也被保守拒绝
        assert!(!is_readonly_command("echo $(( $((1 + 2)) + 1 ))"));
    }

    #[test]
    fn sed_tail_options_are_denied() {
        // GNU sed 允许脚本后置选项,尾部选项形态一律拒绝
        assert!(!is_readonly_command("sed 's/a/b/' -i f"));
        assert!(!is_readonly_command("sed 's/a/b/' --in-place f"));
        assert!(!is_readonly_command("sed -n '4,6p' f -i"));
        // 尾部是文件名/stdin 标记的只读形式不受影响
        assert!(is_readonly_command("sed 's/a/b/' f"));
        assert!(is_readonly_command("sed 's/a/b/' -"));
        assert!(is_readonly_command("sed -e 's/a/b/' f"));
    }

    #[test]
    fn git_ref_write_forms_are_denied() {
        // tag/branch:非 flag 参数(创建 ref)与写 flag
        assert!(!is_readonly_command("git tag evil"));
        assert!(!is_readonly_command("git tag -d v1"));
        assert!(!is_readonly_command("git tag -m msg v1"));
        assert!(!is_readonly_command("git branch -D x"));
        assert!(!is_readonly_command("git branch --move a b"));
        // remote:写 .git/config 的子命令
        assert!(!is_readonly_command("git remote add o url"));
        assert!(!is_readonly_command("git remote remove origin"));
        assert!(!is_readonly_command("git remote update"));
        // diff/show:输出落盘任意路径
        assert!(!is_readonly_command("git diff --output=foo.txt"));
        assert!(!is_readonly_command("git show --output foo.txt"));
    }

    #[test]
    fn git_ref_readonly_forms_still_pass() {
        assert!(is_readonly_command("git tag"));
        assert!(is_readonly_command("git tag -l"));
        assert!(is_readonly_command("git tag -n5"));
        assert!(is_readonly_command("git branch"));
        assert!(is_readonly_command("git branch -a"));
        assert!(is_readonly_command("git branch --show-current"));
        assert!(is_readonly_command("git branch --sort=-committerdate -v"));
        assert!(is_readonly_command("git remote"));
        assert!(is_readonly_command("git remote -v"));
        assert!(is_readonly_command("git remote get-url origin"));
        assert!(is_readonly_command("git remote show origin"));
        // --output-indicator-* 不是 --output 落盘 flag
        assert!(is_readonly_command("git diff --output-indicator-new='#'"));
    }

    #[test]
    fn git_c_and_dangerous_env_are_denied() {
        // -c 配置可注入可执行程序(core.pager/diff.external/core.fsmonitor…)
        assert!(!is_readonly_command("git -c core.pager='touch /tmp/pwned' log"));
        assert!(!is_readonly_command("git -c diff.external='sh -c x' diff"));
        // 混在 -C 剥离序列里也不放行
        assert!(!is_readonly_command("git -C /tmp -c core.fsmonitor=evil.sh status"));
        // env 危险变量(GIT_* / PAGER / LD_PRELOAD…)注入
        assert!(!is_readonly_command("env GIT_PAGER=evil git log"));
        assert!(!is_readonly_command("env LD_PRELOAD=x ls"));
        assert!(!is_readonly_command("env PGPORT=5432 GIT_EDITOR=evil git log"));
        // 普通变量赋值仍剥离放行
        assert!(is_readonly_command("env FOO=1 git log"));
    }

    #[test]
    fn io_number_prefix_leaves_no_empty_token() {
        // IO number(2>)不再残留空词 token,首词判定不被污染
        assert!(is_readonly_command("2>/dev/null ls"));
        assert!(is_readonly_command("ls 2>/dev/null"));
        assert!(is_readonly_command("2>/dev/null git status"));
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
    fn hard_denied_forms_stay_readonly_false() {
        // 输出重定向到真实文件
        assert!(!is_readonly_command("echo hi > out.txt"));
        assert!(!is_readonly_command("echo hi >> out.txt"));
        assert!(!is_readonly_command("echo hi > /dev/nullx"));
        assert!(!is_readonly_command("cat a > in.txt 2>&1"));
        // 进程替换/heredoc 可执行任意内容或依赖换行体
        assert!(!is_readonly_command("diff <(echo a) <(echo b)"));
        assert!(!is_readonly_command("cat << EOF"));
        assert!(!is_readonly_command("cat <<< hi"));
        // 单个 & 后台执行、换行、未闭合引号、空命令
        assert!(!is_readonly_command("ls &"));
        assert!(!is_readonly_command("ls\n"));
        assert!(!is_readonly_command("echo \"unterminated"));
        assert!(!is_readonly_command("echo 'unterminated"));
        assert!(!is_readonly_command(""));
    }

    #[test]
    fn pipelines_of_readonly_segments_are_readonly() {
        assert!(is_readonly_command("ls | wc -l"));
        assert!(is_readonly_command("find . -type f | wc -l"));
        assert!(is_readonly_command("find . -type f | wc -l && echo \"---\""));
        assert!(is_readonly_command("cat a && cat b"));
        assert!(is_readonly_command("git log; git status"));
        assert!(is_readonly_command("ls || git status"));
        // 引号内的 | 是字面字符,不拆分
        assert!(is_readonly_command("echo \"a | b\""));
        assert!(is_readonly_command("echo 'a && b'"));
        // 引号内的 > 也是字面
        assert!(is_readonly_command("echo \"a > b\""));
        // 输入重定向只读
        assert!(is_readonly_command("ls < in.txt"));
        assert!(is_readonly_command("tr a-z A-Z < cache_test.py | head"));
        assert!(is_readonly_command("hexdump -C rpi | head"));
        // 丢弃型输出重定向无害
        assert!(is_readonly_command("echo hi > /dev/null"));
        assert!(is_readonly_command("sysctl -n hw.ncpu 2>/dev/null"));
        assert!(is_readonly_command("ls > /dev/null 2>&1"));
        assert!(is_readonly_command("ls 2>&1 | head -3"));
        // 新增只读命令
        assert!(is_readonly_command("uptime; who; date -u"));
        assert!(is_readonly_command("env | sort | grep -i '^PATH=' | head -15"));
    }

    #[test]
    fn pipelines_with_non_readonly_or_dangling_segment_fail() {
        // 任一段不在白名单 → 整条拒绝
        assert!(!is_readonly_command("ls | rm x"));
        assert!(!is_readonly_command("cargo build | wc -l"));
        // 悬空分隔符(空段)→ 拒绝
        assert!(!is_readonly_command("ls |"));
        assert!(!is_readonly_command("| ls"));
        assert!(!is_readonly_command("ls &&"));
        // 组合:一段合法一段重定向 → 拒绝
        assert!(!is_readonly_command("ls && echo hi > out.txt"));
    }

    #[test]
    fn sysctl_write_mode_is_denied() {
        assert!(!is_readonly_command("sysctl -w kern.maxfiles=10000"));
    }

    #[test]
    fn cd_segments_pass_when_rest_is_readonly() {
        // cd 自身无副作用,复合命令其余段全部只读时放行
        assert!(is_readonly_command(
            "cd /Users/kin/Documents/10source/test && git status && git log --oneline -5 && file rpi"
        ));
        assert!(is_readonly_command("cd /tmp"));
        assert!(is_readonly_command("cd .. && ls"));
        assert!(is_readonly_command("cd /tmp; git status | head -3"));
    }

    #[test]
    fn cd_does_not_rescue_non_readonly_segments() {
        assert!(!is_readonly_command("cd /tmp && rm -rf x"));
        assert!(!is_readonly_command("cd /tmp && echo hi > out.txt"));
    }

    #[test]
    fn deny_rule_overrides_cd() {
        let deny = vec!["cd".to_string()];
        assert!(!is_readonly_command_with_rules(
            "cd /tmp && git status",
            &[],
            &deny
        ));
    }

    #[test]
    fn awk_safe_forms_pass() {
        assert!(is_readonly_command("awk 'NR<=3{print NR\": \"$0}' cache_test.py"));
        assert!(is_readonly_command("awk -F, '{print $2}' f.csv"));
        assert!(is_readonly_command("awk -v OFS=, '{print $1, $2}' f"));
        assert!(is_readonly_command("awk '$1>2{print}' f"));
        assert!(is_readonly_command(
            "ps aux | awk 'NR>1{c[$11]+=$3; m[$11]+=$4} END{for(p in c) printf \"%6.2f%% cpu\", c[p], m[p], p}' | sort -rn | head -12"
        ));
        // print 上下文之外的比较 >
        assert!(is_readonly_command("awk 'END{for(p in c) print c[p]}' f"));
    }

    #[test]
    fn awk_side_effects_fail() {
        assert!(!is_readonly_command("awk 'BEGIN{system(\"rm -rf x\")}' f"));
        assert!(!is_readonly_command("awk '{print > \"out.txt\"}' f"));
        assert!(!is_readonly_command("awk '{print | \"mail x\"}' f"));
        assert!(!is_readonly_command("awk '\"sh\" | getline line' f"));
        // -f 脚本文件 / -i inplace / 未知选项
        assert!(!is_readonly_command("awk -f prog.awk f"));
        assert!(!is_readonly_command("awk -i inplace '{print}' f"));
        assert!(!is_readonly_command("awk --sandbox '{print}' f"));
        // 程序来自变量展开,内容不可判定
        assert!(!is_readonly_command("awk $PROG f"));
        assert!(!is_readonly_command("awk \"$PROG\" f"));
    }

    #[test]
    fn sed_safe_forms_pass() {
        assert!(is_readonly_command("sed -n '4,6p' cache_test.py"));
        assert!(is_readonly_command("df -h | sed 1d | sort -k5 -rh | head -8"));
        assert!(is_readonly_command("sed 's/a/b/g' f"));
        assert!(is_readonly_command("sed '/^#/d' /etc/hosts"));
        assert!(is_readonly_command("sed 'y/abc/xyz/' f"));
        assert!(is_readonly_command("sed -n '1,3{=;p}' f"));
        assert!(is_readonly_command("mount | awk '{print $1, \"->\", $3}' | head -10"));
    }

    #[test]
    fn sed_side_effects_fail() {
        assert!(!is_readonly_command("sed -i 's/a/b/' f"));
        assert!(!is_readonly_command("sed 's/a/b/w out.txt' f"));
        assert!(!is_readonly_command("sed 's/a/b/e' f"));
        assert!(!is_readonly_command("sed '2e date' f"));
        assert!(!is_readonly_command("sed 'r other.txt' f"));
        assert!(!is_readonly_command("sed -f script.sed f"));
    }

    #[test]
    fn command_substitution_is_recursively_validated() {
        // 内层只读 → 放行
        assert!(is_readonly_command("file $(command -v git)"));
        assert!(is_readonly_command("echo $(date)"));
        assert!(is_readonly_command("echo `date`"));
        assert!(is_readonly_command("echo \"$(date)\""));
        assert!(is_readonly_command(
            "file $(command -v python3 git node curl 2>/dev/null) 2>/dev/null | head -10"
        ));
        // 内层非只读 → 拒绝
        assert!(!is_readonly_command("file $(rm -rf x)"));
        assert!(!is_readonly_command("echo `rm x`"));
        assert!(!is_readonly_command("echo \"`rm x`\""));
        assert!(!is_readonly_command("echo $(cat x > y)"));
        // 未闭合/空替换
        assert!(!is_readonly_command("echo $(date"));
        assert!(!is_readonly_command("echo $( )"));
        // 算术展开无数值外副作用
        assert!(is_readonly_command("echo $((1 + 2))"));
    }

    #[test]
    fn find_exec_body_is_recursively_validated() {
        assert!(is_readonly_command("find . -name \"*.sample\" -exec ls -l {} \\;"));
        assert!(is_readonly_command("find . -exec ls {} +"));
        assert!(is_readonly_command(
            "find /usr/local -maxdepth 3 \\( -name \"*.conf\" -o -name \"*.plist\" \\) 2>/dev/null | head -20"
        ));
        // exec 体非只读 / 缺终止符
        assert!(!is_readonly_command("find . -exec rm {} \\;"));
        assert!(!is_readonly_command("find . -exec sh -c 'x' \\;"));
        assert!(!is_readonly_command("find . -exec ls {}"));
        // find 写谓词
        assert!(!is_readonly_command("find . -name x -delete"));
        assert!(!is_readonly_command("find . -name x -fprint out.txt"));
        assert!(!is_readonly_command("find . -name x -ok rm {} \\;"));
    }

    #[test]
    fn real_world_plan_mode_probes() {
        assert!(is_readonly_command("uname -a && whoami && date"));
        assert!(is_readonly_command("ps aux | head -20"));
        assert!(is_readonly_command("df -h /"));
        assert!(is_readonly_command("env | sort | head -30"));
        assert!(is_readonly_command("du -ah /tmp/x | sort -rn | head -10"));
        assert!(is_readonly_command("head -c 4096 /tmp/x | strings | head -3"));
        assert!(is_readonly_command("md5 /tmp/x | head -c 64"));
        assert!(is_readonly_command(
            "git -C /Users/kin/Documents/10source/test log --oneline -10 | cat"
        ));
        assert!(is_readonly_command("tail -5 /tmp/x.py | wc -c"));
        assert!(is_readonly_command("sort /tmp/x.py | uniq -c | sort -rn | head -5"));
        // 裸 env / 裸 mount 只打印信息
        assert!(is_readonly_command("mount"));
        assert!(is_readonly_command("sudo -n env"));
        assert!(is_readonly_command("mount | awk '{print $1, \"->\", $3}' | head -10"));
    }

    #[test]
    fn commands_with_write_side_effects_stay_denied() {
        // mount 带参数是真挂载
        assert!(!is_readonly_command("df -h / && mount /dev/disk1 /mnt"));
        // sort -o 写文件
        assert!(!is_readonly_command("sort -o out.txt in.txt"));
        assert!(!is_readonly_command("sort --output=out.txt in.txt"));
        // git -C 剥离后仍要命中子命令表
        assert!(!is_readonly_command("git -C /tmp/x push"));
        // 命令名来自展开
        assert!(!is_readonly_command("$CMD x"));
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
        // deny 命中任何一段 → 整条拒绝
        assert!(!is_readonly_command_with_rules(
            "ls | make test",
            &allow,
            &["make".to_string()]
        ));
        assert!(!is_readonly_command_with_rules(
            "git status && git log",
            &[],
            &["git log".to_string()]
        ));
        // deny 也拦截替换内层
        assert!(!is_readonly_command_with_rules(
            "echo $(git log)",
            &[],
            &["git log".to_string()]
        ));
    }

    #[test]
    fn whitespace_is_normalized() {
        assert!(is_readonly_command("  ls   -la  "));
        assert!(is_readonly_command("git\tlog"));
    }

    // ---- 系统配置/状态查询(17 条,追加;写形态对应拒绝) ----

    #[test]
    fn macos_system_queries_are_readonly() {
        // 第1批:系统配置查询
        assert!(is_readonly_command("defaults read com.apple.dock | head -5"));
        assert!(is_readonly_command("pmset -g"));
        assert!(is_readonly_command("nvram -p"));
        assert!(is_readonly_command("launchctl list | head -5"));
        assert!(is_readonly_command("security list-keychains"));
        // 第3批:网络家族·本地零发包状态查询
        assert!(is_readonly_command("netstat -an | head -5"));
        assert!(is_readonly_command("ifconfig lo0"));
        assert!(is_readonly_command("route -n get default"));
        assert!(is_readonly_command("arp -a | head -3"));
        assert!(is_readonly_command("scutil --dns | head -10"));
        // 第4批:git 只读子命令
        assert!(is_readonly_command("git stash list"));
        assert!(is_readonly_command("git clean -n"));
        assert!(is_readonly_command("mdutil -s /"));
        // 第5批:状态查询
        assert!(is_readonly_command(
            "dscl . -read /Groups/admin GroupMembership"
        ));
        assert!(is_readonly_command("csrutil status"));
        assert!(is_readonly_command("fdesetup status"));
        assert!(is_readonly_command("crontab -l"));
        assert!(is_readonly_command("kill -l | head -5"));
    }

    #[test]
    fn macos_state_writers_stay_denied() {
        // defaults:写/删
        assert!(!is_readonly_command(
            "defaults write com.apple.dock orientation -int left"
        ));
        assert!(!is_readonly_command("defaults delete com.apple.dock x"));
        // pmset:设置
        assert!(!is_readonly_command("pmset -a displaysleep 10"));
        // nvram:写/删
        assert!(!is_readonly_command("nvram boot-args=x"));
        assert!(!is_readonly_command("nvram -d boot-args"));
        assert!(!is_readonly_command("nvram -c"));
        // launchctl:加载/启停
        assert!(!is_readonly_command("launchctl load /Library/LaunchAgents/x.plist"));
        assert!(!is_readonly_command("launchctl enable system/com.x"));
        // security:删钥匙串
        assert!(!is_readonly_command("security delete-keychain login.keychain"));
        // ifconfig:动作词/IP 不满足接口名特征
        assert!(!is_readonly_command("ifconfig en0 down"));
        assert!(!is_readonly_command("ifconfig en0 inet 192.168.1.5"));
        assert!(!is_readonly_command("ifconfig en0 alias 192.168.1.5"));
        // route:改路由/刷表
        assert!(!is_readonly_command("route add -host 10.0.0.1 10.0.0.2"));
        assert!(!is_readonly_command("route -n flush"));
        // arp:写/删表项
        assert!(!is_readonly_command("arp -s en0 192.168.1.5"));
        assert!(!is_readonly_command("arp -d en0"));
        // scutil:设置
        assert!(!is_readonly_command("scutil --set ComputerName x"));
        // git:非只读子命令
        assert!(!is_readonly_command("git stash drop"));
        assert!(!is_readonly_command("git stash pop"));
        assert!(!is_readonly_command("git clean -fd"));
        assert!(!is_readonly_command("git clean -f"));
        // mdutil:开关索引
        assert!(!is_readonly_command("mdutil -i on /"));
        // dscl:写操作
        assert!(!is_readonly_command("dscl . -create /Users/x RealName y"));
        assert!(!is_readonly_command(
            "dscl . -append /Groups/admin GroupMembership user"
        ));
        // csrutil:开关 SIP
        assert!(!is_readonly_command("csrutil enable"));
        // crontab:删/编
        assert!(!is_readonly_command("crontab -r"));
        // kill:发信号
        assert!(!is_readonly_command("kill -9 1"));
        assert!(!is_readonly_command("kill 123"));
    }

    #[test]
    fn zero_side_effect_commands_are_readonly() {
        assert!(is_readonly_command("cal"));
        assert!(is_readonly_command("cal 2026"));
        assert!(is_readonly_command("factor 12345"));
        assert!(is_readonly_command("hostinfo"));
        assert!(is_readonly_command("arch"));
        assert!(is_readonly_command("arch -x86_64 ls"));
        assert!(is_readonly_command("arch -arch arm64 git status"));
        assert!(is_readonly_command("otool -L /bin/ls | head -5"));
        assert!(is_readonly_command("otool -l /bin/ls"));
        // sysctl(-w 守卫)与 uptime 此前已在表中,一并确认
        assert!(is_readonly_command("sysctl -n hw.ncpu"));
        assert!(is_readonly_command("uptime"));
        assert!(is_readonly_command("sw_vers"));
    }

    #[test]
    fn arch_wrapper_revalidates_inner_command() {
        // arch 是包装器:内层命令必须单独通过只读校验
        assert!(!is_readonly_command("arch -x86_64 rm -rf x"));
        assert!(!is_readonly_command("arch -arch x86_64 curl example.com"));
        assert!(!is_readonly_command("arch -x86_64 python3 -c 'print(1)'"));
    }
}
