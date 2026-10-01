//! shell 输出净化(pi 的 utils/ansi.ts + utils/shell.ts):剥离 ANSI 转义序列、
//! 过滤控制字符。
//!
//! **只用于 `!` 裸命令路径**(rpi-cli 的 run_command,对应 pi 的
//! bash-executor.ts:82)——模型调用的内置工具结果字节级保真、不做净化
//! (对齐 pi:截断管体积,不管内容)。

use std::iter::Peekable;
use std::str::Chars;

/// 剥离 ANSI 转义序列(颜色、粗体、光标控制、OSC 标题/超链接等)。
/// 手写状态机:CSI(`ESC [` 参数…final byte)、OSC(`ESC ]` 终止于 BEL 或
/// ST `ESC \`)、单字节 C1 CSI(`U+009B`)与两字符转义(如 `ESC ( B`)。
/// 文本不含 ESC 时零开销直返(pi 的 strip-ansi)。
pub fn strip_ansi(text: &str) -> String {
    if !text.contains('\u{1b}') && !text.contains('\u{9b}') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{9b}' => skip_csi(&mut chars),
            '\u{1b}' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    skip_csi(&mut chars);
                }
                Some(']') => {
                    chars.next();
                    skip_osc(&mut chars);
                }
                // 字符集选择 ESC ( B / ESC ) 0 等:引导字符 + 最终字符
                Some('(') | Some(')') | Some('*') | Some('+') => {
                    chars.next();
                    chars.next();
                }
                // 其余两字符转义(ESC \ ESC 7 ESC 8 等):消耗后续字符
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            _ => out.push(c),
        }
    }
    out
}

/// 消费 CSI 序列剩余部分:参数(0x30-0x3F)与中间字节(0x20-0x2F)之后,
/// 遇 final byte(0x40-0x7E)结束。
fn skip_csi(chars: &mut Peekable<Chars<'_>>) {
    for c in chars.by_ref() {
        if ('@'..='~').contains(&c) {
            break;
        }
    }
}

/// 消费 OSC 序列剩余部分:终止于 BEL(`\x07`)或 ST(`ESC \`)。
fn skip_osc(chars: &mut Peekable<Chars<'_>>) {
    while let Some(c) = chars.next() {
        match c {
            '\u{7}' => break,
            '\u{1b}' if chars.peek() == Some(&'\\') => {
                chars.next();
                break;
            }
            _ => {}
        }
    }
}

/// 过滤控制字符(白名单):保留 `\t` `\n` `\r` 与可打印字符,删除其余
/// C0/C1 控制字符及 `U+FFF9-FFFB`(interlinear annotation,某些程序用它
/// 隐藏文字,且会让按显示宽度计算的下游崩溃;pi 的 INVALID_SHELL_OUTPUT)。
/// Rust `String` 恒为合法 UTF-8,不存在孤立代理对,无需处理。
pub fn sanitize_control_chars(text: &str) -> String {
    let needs_filter = text.chars().any(|c| {
        (c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
            || (0xFFF9..=0xFFFB).contains(&(c as u32))
    });
    if !needs_filter {
        return text.to_string();
    }
    text.chars()
        .filter(|c| {
            !c.is_control()
                || matches!(c, '\t' | '\n' | '\r')
        })
        .filter(|c| !(0xFFF9..=0xFFFB).contains(&(*c as u32)))
        .collect()
}

/// 组合净化:先剥 ANSI 序列,再过滤控制字符。工具输出进结果的统一入口。
pub fn sanitize_output(text: &str) -> String {
    sanitize_control_chars(&strip_ansi(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_color_codes() {
        assert_eq!(strip_ansi("\x1b[32m✓\x1b[0m done"), "✓ done");
        assert_eq!(strip_ansi("\x1b[1;31merror\x1b[0m: x"), "error: x");
    }

    #[test]
    fn strips_csi_and_osc_sequences() {
        // CSI 光标控制
        assert_eq!(strip_ansi("a\x1b[2Kb"), "ab");
        // OSC 终止于 BEL
        assert_eq!(strip_ansi("\x1b]0;title\x07body"), "body");
        // OSC 终止于 ST(ESC \)
        assert_eq!(strip_ansi("\x1b]8;;url\x1b\\link"), "link");
    }

    #[test]
    fn without_esc_returns_fast() {
        assert_eq!(strip_ansi("plain 中文 text"), "plain 中文 text");
    }

    #[test]
    fn control_char_filter_keeps_whitespace_and_printable() {
        assert_eq!(sanitize_control_chars("a\tb\nc\rd"), "a\tb\nc\rd");
        assert_eq!(sanitize_control_chars("普通中文 emoji 🎉"), "普通中文 emoji 🎉");
    }

    #[test]
    fn control_char_filter_removes_non_whitespace_controls() {
        assert_eq!(sanitize_control_chars("a\u{7}b\u{8}c\u{b}d"), "abcd");
        // C1 控制字符同样删除
        assert_eq!(sanitize_control_chars("a\u{9b}b"), "ab");
        // U+FFF9-FFFB(interlinear annotation)删除
        assert_eq!(sanitize_control_chars("a\u{fff9}b\u{fffb}c"), "abc");
    }

    #[test]
    fn sanitize_output_combines_both() {
        assert_eq!(sanitize_output("\x1b[31merr\x1b[0m\u{7}"), "err");
    }
}
