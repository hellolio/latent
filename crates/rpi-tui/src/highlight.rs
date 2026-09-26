//! 代码块语法高亮(syntect):VS Code 同源 TextMate 语法,输出 ratatui
//! spans。主题跟随界面主题的明暗:深色 → base16-ocean.dark,浅色 →
//! InspiredGitHub。首次使用时惰性加载语法集(启动开销 ~100ms,无代码块
//! 的会话不付出成本)。

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// 高亮器(syntect 默认语法集 + 按明暗选择的高亮主题)。
pub struct Highlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

static DARK_HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();
static LIGHT_HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();

const DARK_SYNTAX_THEME: &str = "base16-ocean.dark";
const LIGHT_SYNTAX_THEME: &str = "InspiredGitHub";

impl Highlighter {
    fn load(syntax_theme: &str) -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let mut theme_set = ThemeSet::load_defaults();
        let theme = theme_set.themes.remove(syntax_theme).unwrap_or_default();
        Highlighter { syntax_set, theme }
    }

    /// 全局单例(按明暗各一个,惰性初始化)。`is_dark` 来自 `Theme::is_dark`。
    pub fn shared(is_dark: bool) -> &'static Highlighter {
        if is_dark {
            DARK_HIGHLIGHTER.get_or_init(|| Highlighter::load(DARK_SYNTAX_THEME))
        } else {
            LIGHT_HIGHLIGHTER.get_or_init(|| Highlighter::load(LIGHT_SYNTAX_THEME))
        }
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
        let lines = Highlighter::shared(true).highlight("let a = \"x\";", Some("rust"));
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("let a = \"x\";"), "{text}");
        // 至少有两种前景色(keyword/string 不同)
        let fg: std::collections::HashSet<_> = lines[0].spans.iter().map(|s| s.style.fg).collect();
        assert!(fg.len() > 1, "rust 代码应有多种颜色: {fg:?}");
    }

    #[test]
    fn unknown_language_falls_back_to_plain() {
        let lines = Highlighter::shared(true).highlight("plain text", Some("nope"));
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "plain text");
    }

    #[test]
    fn empty_code_renders_empty() {
        let lines = Highlighter::shared(true).highlight("", Some("rust"));
        assert!(lines.is_empty() || (lines.len() == 1 && lines[0].spans.is_empty()));
    }

    #[test]
    fn light_theme_uses_light_syntax_theme() {
        // 浅色界面配浅色高亮:背景亮度高于深色主题
        let dark = Highlighter::shared(true).theme.settings.background.unwrap();
        let light = Highlighter::shared(false).theme.settings.background.unwrap();
        let brightness = |c: syntect::highlighting::Color| {
            (u32::from(c.r) * 299 + u32::from(c.g) * 587 + u32::from(c.b) * 114) / 1000
        };
        assert!(brightness(dark) < brightness(light), "{dark:?} vs {light:?}");
    }
}
