//! 语义化主题(pi theme/dark.json 的 Rust 对应物):颜色按**角色**命名,
//! 调用方不感知具体色值。两套内置调色板:真彩色终端用 RGB(pi dark 风格,
//! tokyonight 邻近色),仅 16 色的终端(如 macOS Terminal.app)退回 ANSI 基本色。

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
    /// 更弱一级(键名、footer 基调)
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
}

impl Theme {
    /// 真彩色调色板(COLORTERM=truecolor / 24bit)。
    pub fn dark() -> Self {
        Theme {
            accent: Color::Rgb(0x7a, 0xa2, 0xf7),
            user_bg: Color::Rgb(0x34, 0x35, 0x41),
            user_text: Color::Rgb(0xe6, 0xe6, 0xe6),
            assistant_text: Color::Rgb(0xc0, 0xca, 0xde),
            thinking: Color::Rgb(0x56, 0x5f, 0x89),
            tool_title: Color::Rgb(0x7d, 0xcf, 0xff),
            tool_pending: Color::Rgb(0xe0, 0xaf, 0x68),
            tool_success: Color::Rgb(0x9e, 0xce, 0x6a),
            tool_error: Color::Rgb(0xf7, 0x76, 0x8e),
            tool_output: Color::Rgb(0xa9, 0xb1, 0xd6),
            error: Color::Rgb(0xf7, 0x76, 0x8e),
            warning: Color::Rgb(0xe0, 0xaf, 0x68),
            success: Color::Rgb(0x9e, 0xce, 0x6a),
            muted: Color::Rgb(0x96, 0x9e, 0xb6),
            dim: Color::Rgb(0x56, 0x5f, 0x89),
            md_heading: Color::Rgb(0xe0, 0xaf, 0x68),
            md_link: Color::Rgb(0x7a, 0xa2, 0xf7),
            md_code: Color::Rgb(0xff, 0x9e, 0x64),
            md_code_block_border: Color::Rgb(0x3b, 0x42, 0x61),
            border_idle: Color::Rgb(0x3b, 0x42, 0x61),
            border_busy: Color::Rgb(0xbb, 0x9a, 0xf7),
            border_bash: Color::Rgb(0x9e, 0xce, 0x6a),
            spinner: Color::Rgb(0xbb, 0x9a, 0xf7),
            footer_cwd: Color::Rgb(0x7a, 0xa2, 0xf7),
        }
    }

    /// ANSI 16 色兜底(基本色相尽量对齐 dark 调色板的语义)。
    pub fn dark_ansi() -> Self {
        Theme {
            accent: Color::Blue,
            user_bg: Color::DarkGray,
            user_text: Color::White,
            assistant_text: Color::Gray,
            thinking: Color::DarkGray,
            tool_title: Color::Cyan,
            tool_pending: Color::Yellow,
            tool_success: Color::Green,
            tool_error: Color::Red,
            tool_output: Color::Gray,
            error: Color::Red,
            warning: Color::Yellow,
            success: Color::Green,
            muted: Color::Gray,
            dim: Color::DarkGray,
            md_heading: Color::Yellow,
            md_link: Color::Blue,
            md_code: Color::Magenta,
            md_code_block_border: Color::DarkGray,
            border_idle: Color::DarkGray,
            border_busy: Color::Magenta,
            border_bash: Color::Green,
            spinner: Color::Magenta,
            footer_cwd: Color::Blue,
        }
    }

    /// 按终端能力选择调色板。
    pub fn detect() -> Self {
        match std::env::var("COLORTERM").ok().as_deref() {
            Some(value) if value.contains("truecolor") || value.contains("24bit") => Self::dark(),
            _ => {
                // 256 色终端同样走 RGB(现代模拟器均支持);纯 16 色才降级
                match std::env::var("TERM").ok().as_deref() {
                    Some(term) if term.contains("256color") || term.contains("xterm") => {
                        Self::dark()
                    }
                    _ => Self::dark_ansi(),
                }
            }
        }
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
