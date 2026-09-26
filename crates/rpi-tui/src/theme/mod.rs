//! 语义化主题:颜色按**角色**命名,调用方不感知具体色值。
//!
//! 分层(逻辑与主题数据分离):
//! - 本模块 —— 角色接口(`Theme`)与解析逻辑(`resolve`/`detect`),不含色值;
//! - `builtin` —— 内置兜底调色板(ANSI 16 色与旧版真彩色 dark);
//! - `external` —— [ratatui-themes](https://docs.rs/ratatui-themes) 15+ 主题
//!   到语义角色的映射(统一推导规则 + 精选主题微调)。
//!
//! 主题选择链路:`resolve(name)`(settings.json / --theme / /theme)→
//! `detect()`(未配置时按终端能力自动选择)。

mod builtin;
mod external;

pub use external::ThemeName;

use ratatui::style::{Color, Modifier, Style};

/// 语义色 token(08 文档 theme.ts 的子集,覆盖交互模式实际用到的角色)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// 品牌强调色(logo、列表 bullet、选中项)
    pub accent: Color,
    /// 用户消息背景块(pi userMessageBg)
    pub user_bg: Color,
    pub user_text: Color,
    pub assistant_text: Color,
    /// thinking 块(pi thinkingText)
    pub thinking: Color,
    /// 工具标题(pi toolTitle,加粗使用)
    pub tool_title: Color,
    pub tool_pending: Color,
    pub tool_success: Color,
    pub tool_error: Color,
    /// 工具输出正文(pi toolOutput)
    pub tool_output: Color,
    pub error: Color,
    pub warning: Color,
    pub success: Color,
    /// 次要文字(pi muted)
    pub muted: Color,
    /// 更弱一级(键名、footer 基调、分隔线)
    pub dim: Color,
    pub md_heading: Color,
    pub md_link: Color,
    pub md_code: Color,
    pub md_code_block_border: Color,
    /// 编辑器边框:idle(pi getThinkingBorderColor 的低档)
    pub border_idle: Color,
    /// 编辑器边框:工作状态(spinner 同色)
    pub border_busy: Color,
    /// 编辑器边框:! bash 透传模式(pi bashMode)
    pub border_bash: Color,
    pub spinner: Color,
    pub footer_cwd: Color,
    /// 深色/浅色主题标记(驱动 syntect 高亮主题选择)
    pub is_dark: bool,
}

impl Theme {
    /// 按名字解析主题(kebab-case/PascalCase/常用别名均接受,如
    /// `tokyo-night`/`nord`/`mocha`)。未知名返回 None。仅 16 色终端一律
    /// 降级 `dark_ansi()`(外部主题为真彩色调色板,无法映射到 16 色)。
    pub fn resolve(name: &str, truecolor: bool) -> Option<Self> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let parsed: ThemeName = name.parse().ok()?;
        if truecolor {
            Some(external::from_name(parsed))
        } else {
            Some(builtin::dark_ansi())
        }
    }

    /// 终端是否支持真彩色/256 色(COLORTERM / TERM 探测)。
    pub fn truecolor_supported() -> bool {
        match std::env::var("COLORTERM").ok().as_deref() {
            Some(value) if value.contains("truecolor") || value.contains("24bit") => true,
            _ => matches!(
                std::env::var("TERM").ok().as_deref(),
                Some(term) if term.contains("256color") || term.contains("xterm")
            ),
        }
    }

    /// 按终端能力选择默认主题:真彩色 → ratatui-themes Tokyo Night,
    /// 仅 16 色 → ANSI 兜底。
    pub fn detect() -> Self {
        if Self::truecolor_supported() {
            external::from_name(ThemeName::TokyoNight)
        } else {
            builtin::dark_ansi()
        }
    }

    /// 按主题枚举直接构造(ratatui-themes 映射;/theme 切换用,不受
    /// 终端能力降级影响)。
    pub fn from_theme_name(name: ThemeName) -> Self {
        external::from_name(name)
    }

    /// 内置真彩色调色板(旧默认;保留供测试与兜底)。
    pub fn dark() -> Self {
        builtin::dark()
    }

    /// ANSI 16 色兜底(基本色相尽量对齐 dark 调色板的语义)。
    pub fn dark_ansi() -> Self {
        builtin::dark_ansi()
    }

    pub fn hint_key(&self) -> Style {
        Style::new().fg(self.dim)
    }

    pub fn hint_desc(&self) -> Style {
        Style::new().fg(self.muted)
    }

    pub fn bold(color: Color) -> Style {
        Style::new().fg(color).add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_accepts_kebab_and_aliases() {
        assert_eq!(Theme::resolve("tokyo-night", true), Some(external::from_name(ThemeName::TokyoNight)));
        assert_eq!(Theme::resolve("Nord", true).unwrap().is_dark, true);
        assert_eq!(Theme::resolve("mocha", true).unwrap().is_dark, true);
        assert_eq!(Theme::resolve("latte", true).unwrap().is_dark, false);
    }

    #[test]
    fn resolve_unknown_returns_none() {
        assert_eq!(Theme::resolve("nonexistent-theme", true), None);
        assert_eq!(Theme::resolve("", true), None);
        assert_eq!(Theme::resolve("  ", true), None);
    }

    #[test]
    fn resolve_falls_back_to_ansi_without_truecolor() {
        assert_eq!(Theme::resolve("nord", false), Some(Theme::dark_ansi()));
    }

    #[test]
    fn detect_always_dark_default() {
        // 默认主题链路(TokyoNight / ANSI 兜底)都是深色
        assert!(Theme::detect().is_dark);
    }
}
