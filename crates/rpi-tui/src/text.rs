//! Span 感知的行处理工具:折行、截断、取纯文本。宽度计算复用 `width`
//! 模块(CJK 双宽),样式(span)在折行/截断后原样保留。

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::width::{char_width, display_width};

/// 取一行的纯文本(spans 顺序拼接)。
pub fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// 取一行的显示宽度。
pub fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

/// 按显示宽度折行(词边界优先、超长单词硬切),样式逐字符保留。
/// 与 `width::wrap_to_width` 的词边界规则语义一致,但作用在 styled 流上。
pub fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line_owned(line)];
    }
    let styled: Vec<(Style, char)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (span.style, c)))
        .collect();

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<(Style, String)> = Vec::new();
    let mut current_width = 0usize;

    // 结束当前行
    let flush = |current: &mut Vec<(Style, String)>, out: &mut Vec<Line<'static>>| {
        let spans = merge_runs(std::mem::take(current));
        out.push(Line::from(spans));
    };
    // 追加一段同样式文本
    let push_text = |current: &mut Vec<(Style, String)>, style: Style, text: &str| {
        if current.last().map(|(s, _)| *s) == Some(style) {
            current.last_mut().unwrap().1.push_str(text);
        } else {
            current.push((style, text.to_string()));
        }
    };

    let mut i = 0usize;
    while i < styled.len() {
        let (style, c) = styled[i];
        if c == ' ' || c == '\t' {
            // 空白:放不下就换行并折叠后续空白
            if current_width + 1 > width {
                flush(&mut current, &mut out);
                current_width = 0;
                while i < styled.len() && (styled[i].1 == ' ' || styled[i].1 == '\t') {
                    i += 1;
                }
                continue;
            }
            push_text(&mut current, style, " ");
            current_width += 1;
            i += 1;
            continue;
        }
        // 收集一个词(非空白连续段)
        let word_start = i;
        let mut word_width = 0usize;
        while i < styled.len() && styled[i].1 != ' ' && styled[i].1 != '\t' {
            word_width += char_width(styled[i].1);
            i += 1;
        }
        // 当前行放不下整个词:先换行(行首有词时不丢前缀)
        if current_width > 0 && current_width + word_width > width {
            flush(&mut current, &mut out);
            current_width = 0;
        }
        // 逐字符放入;单字符放不下即换行(超长词硬切、双宽字符不切断语义由
        // "放不下就换行"保证)
        for &(style, c) in &styled[word_start..i] {
            let w = char_width(c);
            if current_width + w > width {
                flush(&mut current, &mut out);
                current_width = 0;
            }
            push_text(&mut current, style, &c.to_string());
            current_width += w;
        }
    }
    flush(&mut current, &mut out);
    out
}

/// 按显示宽度截断(不切断双宽字符),样式保留。
pub fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    if line_width(&line) <= width {
        return line;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        if used >= width {
            break;
        }
        let (text, w) = crate::width::truncate_to_width(span.content.as_ref(), width - used);
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
        used += w;
    }
    Line::from(spans)
}

/// 右对齐辅助:左行 + 填充 + 右行,总宽不超过 width(放不下时只留左行)。
pub fn join_right(left: Line<'static>, right: Line<'static>, width: usize) -> Line<'static> {
    let left_width = line_width(&left);
    let right_width = line_width(&right);
    if left_width + right_width + 1 > width {
        return left;
    }
    let pad = width - left_width - right_width;
    let mut spans = left.spans;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right.spans);
    Line::from(spans)
}

fn line_owned(line: &Line<'_>) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|span| Span::styled(span.content.as_ref().to_string(), span.style))
            .collect::<Vec<_>>(),
    )
}

/// 把 (Style, text) 段归并成 spans。
fn merge_runs(runs: Vec<(Style, String)>) -> Vec<Span<'static>> {
    runs.into_iter()
        .map(|(style, text)| Span::styled(text, style))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(line: &Line<'static>) -> String {
        line_text(line)
    }

    #[test]
    fn wrap_plain_line_word_boundary() {
        let line = Line::raw("hello world");
        let out = wrap_line(&line, 5);
        assert_eq!(out.len(), 2);
        assert_eq!(text_of(&out[0]), "hello");
        assert_eq!(text_of(&out[1]), "world");
    }

    #[test]
    fn wrap_preserves_styles_across_break() {
        let error = Style::new().fg(crate::theme::Theme::dark_ansi().error);
        let line = Line::from(vec![Span::styled("aaa", error), Span::raw(" bbb")]);
        let out = wrap_line(&line, 3);
        assert_eq!(out.len(), 2);
        assert_eq!(text_of(&out[0]), "aaa");
        assert_eq!(text_of(&out[1]), "bbb");
        assert_eq!(out[0].spans[0].style, error);
        // 跨行后样式仍逐段保留
        assert_eq!(out[1].spans[0].style, Style::new());
    }

    #[test]
    fn wrap_cjk_hard_split() {
        let line = Line::raw("中文中文");
        let out = wrap_line(&line, 4);
        assert_eq!(
            out.iter().map(text_of).collect::<Vec<_>>(),
            vec!["中文", "中文"]
        );
    }

    #[test]
    fn wrap_overlong_word_hard_splits() {
        let out = wrap_line(&Line::raw("abcdefgh"), 3);
        assert_eq!(
            out.iter().map(text_of).collect::<Vec<_>>(),
            vec!["abc", "def", "gh"]
        );
    }

    #[test]
    fn truncate_keeps_double_width_boundary() {
        let line = Line::raw("a中b");
        assert_eq!(text_of(&truncate_line(line.clone(), 2)), "a");
        assert_eq!(text_of(&truncate_line(line, 3)), "a中");
    }

    #[test]
    fn join_right_pads_to_width() {
        let joined = join_right(Line::raw("abc"), Line::raw("xyz"), 10);
        assert_eq!(text_of(&joined), "abc    xyz");
    }

    #[test]
    fn join_right_drops_right_when_too_narrow() {
        let joined = join_right(Line::raw("abcdef"), Line::raw("xyz"), 8);
        assert_eq!(text_of(&joined), "abcdef");
    }
}
