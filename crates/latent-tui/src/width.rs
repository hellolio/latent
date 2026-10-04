//! 终端显示宽度计算(零依赖:内置常用宽字符区间表)。
//!
//! pi 用完整的 wcwidth 表;这里覆盖 ASCII + 常用 CJK/全角区间,足够聊天 UI
//! 的换行与光标定位。组合字符按 0 宽计,其余未知区间按 1 宽兜底(宁窄勿宽)。

/// 单个字符在终端中的显示列数(0/1/2)。
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    // 组合字符(常见区间):Mn 类,简化为常见组合附加符号区
    if (0x0300..=0x036F).contains(&cp) || (0x200B..=0x200F).contains(&cp) || cp == 0x00AD {
        return 0;
    }
    // 全角/CJK 常用区间(GBK 兼容区、CJK 统一表意、扩展 A、兼容表意、全角形式)
    if (0x1100..=0x115F).contains(&cp) // Hangul Jamo
        || (0x2E80..=0x303E).contains(&cp) // CJK 部首/符号
        || (0x3041..=0x33FF).contains(&cp) // 假名/注音/兼容
        || (0x3400..=0x4DBF).contains(&cp) // CJK 扩展 A
        || (0x4E00..=0x9FFF).contains(&cp) // CJK 统一表意
        || (0xA000..=0xA4CF).contains(&cp) // 彝文
        || (0xAC00..=0xD7A3).contains(&cp) // Hangul 音节
        || (0xF900..=0xFAFF).contains(&cp) // CJK 兼容表意
        || (0xFE30..=0xFE4F).contains(&cp) // CJK 兼容形式
        || (0xFF00..=0xFF60).contains(&cp) // 全角 ASCII/括号
        || (0xFFE0..=0xFFE6).contains(&cp)
    // 全角符号
    {
        return 2;
    }
    // 控制字符不占列(渲染前应剔除)
    if c.is_control() {
        return 0;
    }
    1
}

/// 字符串的终端显示列数(按字符累加;不做 grapheme 合并,组合字符按 0 宽处理)。
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 按显示宽度截断字符串,返回 `(截断结果, 实际占用宽度)`;不切断双宽字符。
pub fn truncate_to_width(s: &str, max_width: usize) -> (String, usize) {
    let mut out = String::new();
    let mut width = 0;
    for c in s.chars() {
        let w = char_width(c);
        if width + w > max_width {
            break;
        }
        width += w;
        out.push(c);
    }
    (out, width)
}

/// 按显示宽度折行(词边界优先,超长单词硬切);空输入返回单行空串。
pub fn wrap_to_width(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    for raw_line in s.split('\n') {
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0;
        for word in raw_line.split_whitespace() {
            let word_width = display_width(word);
            let extra = if current_width > 0 { 1 } else { 0 }; // 词间空格
            if current_width + extra + word_width > max_width && current_width > 0 {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            } else if current_width > 0 {
                current.push(' ');
                current_width += 1;
            }
            // 单词本身超宽:硬切
            let mut rest = word;
            while display_width(rest) > max_width.saturating_sub(current_width) {
                let limit = max_width - current_width;
                let (head, _w) = truncate_to_width(rest, limit);
                if head.is_empty() {
                    break;
                }
                lines.push(head.clone());
                current_width = 0;
                rest = &rest[head.len()..];
            }
            current_width += display_width(rest);
            current.push_str(rest);
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 按显示宽度硬折行(逐字符,保留全部空白;空输入返回单行空串)。
/// 与 `wrap_to_width` 的词边界折行不同,本函数不做任何空白归一化——
/// 输入框等需要"所见即所得"的编辑场景使用(行尾空格、连续空格都
/// 占位可见,光标列与实际字符一一对应)。
pub fn wrap_verbatim(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    for raw_line in s.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for c in raw_line.chars() {
            let w = char_width(c);
            if current_width + w > max_width {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(c);
            current_width += w;
        }
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_control_widths() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('\n'), 0);
        assert_eq!(display_width("hello"), 5);
    }

    #[test]
    fn cjk_is_double_width() {
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('Ａ'), 2); // 全角 A
        assert_eq!(display_width("中文abc"), 7);
    }

    #[test]
    fn combining_marks_are_zero_width() {
        assert_eq!(char_width('\u{0301}'), 0);
        assert_eq!(display_width("e\u{0301}"), 1);
    }

    #[test]
    fn truncate_respects_double_width_boundary() {
        // 剩 1 列放不下双宽字符:截断到它之前
        let (out, w) = truncate_to_width("a中b", 2);
        assert_eq!(out, "a");
        assert_eq!(w, 1);
        let (out2, w2) = truncate_to_width("a中b", 3);
        assert_eq!(out2, "a中");
        assert_eq!(w2, 3);
    }

    #[test]
    fn wrap_breaks_at_word_boundaries() {
        assert_eq!(wrap_to_width("hello world", 5), vec!["hello", "world"]);
        assert_eq!(wrap_to_width("a bb ccc", 6), vec!["a bb", "ccc"]);
    }

    #[test]
    fn wrap_hard_splits_overlong_words() {
        assert_eq!(wrap_to_width("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn wrap_keeps_explicit_newlines_and_empty_lines() {
        assert_eq!(wrap_to_width("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn wrap_zero_width_passthrough() {
        assert_eq!(wrap_to_width("abc", 0), vec!["abc"]);
    }

    #[test]
    fn wrap_cjk_line() {
        // 每行最多 4 列:两个汉字一行
        assert_eq!(wrap_to_width("中文中文", 4), vec!["中文", "中文"]);
    }

    #[test]
    fn wrap_verbatim_preserves_all_whitespace() {
        // 行尾/行首空格保留、连续空格不合并、超宽硬切
        assert_eq!(wrap_verbatim("a ", 10), vec!["a "]);
        assert_eq!(wrap_verbatim(" a", 10), vec![" a"]);
        assert_eq!(wrap_verbatim("a  b", 10), vec!["a  b"]);
        assert_eq!(wrap_verbatim("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap_verbatim("ab cdef", 3), vec!["ab ", "cde", "f"]);
        assert_eq!(wrap_verbatim("a\n\nb", 10), vec!["a", "", "b"]);
        // 光标前缀列数与实际字符位置一致(空格占位)
        let rows = wrap_verbatim("ab ", 10);
        assert_eq!(display_width(&rows[0]), 3);
    }
}
