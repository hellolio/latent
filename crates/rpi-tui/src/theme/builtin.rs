//! 内置兜底调色板:旧版真彩色 dark(与 ratatui-themes Tokyo Night 同源,
//! 保留供测试与比对)与 ANSI 16 色兜底(仅 16 色的终端,如 macOS
//! Terminal.app)。

use ratatui::style::Color;

use super::Theme;

/// 真彩色调色板(COLORTERM=truecolor / 24bit)。
pub fn dark() -> Theme {
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
        is_dark: true,
    }
}

/// ANSI 16 色兜底(基本色相尽量对齐 dark 调色板的语义)。
pub fn dark_ansi() -> Theme {
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
        is_dark: true,
    }
}
