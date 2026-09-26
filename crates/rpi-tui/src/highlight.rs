//! 代码块语法高亮(syntect):VS Code 同源 TextMate 语法 + base16-ocean.dark
//! 主题,输出 ratatui spans。首次使用时惰性加载语法集(启动开销 ~100ms,
//! 无代码块的会话不付出成本)。

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// 高亮器(syntect 默认语法集 + base16-ocean.dark)。
pub struct Highlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();

impl Highlighter {
    fn load() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let mut theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_default();
        Highlighter { syntax_set, theme }
    }

    /// 全局单例(惰性初始化)。
    pub fn shared() -> &'static Highlighter {
        HIGHLIGHTER.get_or_init(Highlighter::load)
    }

    fn find_syntax(&self, lang: Option<&str>) -> &SyntaxReference {
        match lang {
            Some(token) if !token.is_empty() => self
                .syntax_set
                .find_syntax_by_token(token)
                .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text()),
            _ => self.syntax_set.find_syntax_plain_text(),
        }
    }

    /// 高亮一段代码 → 每行一组 spans(前景色;背景取终端默认)。
    /// `lang` 为围栏标记的语言 token(rust/python/…);未知语言按纯文本。
    pub fn highlight(&self, code: &str, lang: Option<&str>) -> Vec<Line<'static>> {
        let syntax = self.find_syntax(lang);
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut out = Vec::new();
        for line in syntect::util::LinesWithEndings::from(code) {
            let ranges = highlighter
                .highlight_line(line, &self.syntax_set)
                .unwrap_or_default();
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (style, chunk) in ranges {
                let cleaned = chunk.trim_end_matches(['\n', '\r']);
                if cleaned.is_empty() {
                    continue;
                }
                let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                spans.push(Span::styled(cleaned.to_string(), Style::new().fg(fg)));
            }
            out.push(Line::from(spans));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_keywords_and_strings_get_colors() {
        let lines = Highlighter::shared().highlight("let a = \"x\";", Some("rust"));
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("let a = \"x\";"), "{text}");
        // 至少有两种前景色(keyword/string 不同)
        let fg: std::collections::HashSet<_> = lines[0].spans.iter().map(|s| s.style.fg).collect();
        assert!(fg.len() > 1, "rust 代码应有多种颜色: {fg:?}");
    }

    #[test]
    fn unknown_language_falls_back_to_plain() {
        let lines = Highlighter::shared().highlight("plain text", Some("nope"));
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "plain text");
    }

    #[test]
    fn empty_code_renders_empty() {
        let lines = Highlighter::shared().highlight("", Some("rust"));
        assert!(lines.is_empty() || (lines.len() == 1 && lines[0].spans.is_empty()));
    }
}
