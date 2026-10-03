//! 内置兜底调色板:旧版真彩色 dark(与 ratatui-themes Tokyo Night 同源,
//! 保留供测试与比对)与 ANSI 16 色兜底(仅 16 色的终端,如 macOS
//! Terminal.app)。

use ratatui::style::Color;

use super::Theme;

/// 真彩色调色板(COLORTERM=truecolor / 24bit)。
pub fn dark() -> Theme {
    Theme {
        // accent 去蓝:与默认主题(Tokyo Night 精选覆盖)同用橙色
        accent: Color::Rgb(0xff, 0x9e, 0x64),
        user_bg: Color::Rgb(0x33, 0x34, 0x38),
        user_text: Color::Rgb(0xbe, 0xbe, 0xbe),
        assistant_text: Color::Rgb(0xd4, 0xd4, 0xd4),
        thinking: Color::Rgb(0x8c, 0x8c, 0x8c),
        tool_title: Color::Rgb(0x7d, 0xcf, 0xff),
        tool_pending: Color::Rgb(0xe0, 0xaf, 0x68),
        tool_success: Color::Rgb(0x9e, 0xce, 0x6a),
        tool_error: Color::Rgb(0xf7, 0x76, 0x8e),
        // 卡片背景压到接近纯黑的低明度色调:只比终端底色稍带一点墨绿/
        // 暗红/中性灰,避免大面积发灰发白刺眼
        tool_pending_bg: Color::Rgb(0x1a, 0x1c, 0x1e),
        tool_success_bg: Color::Rgb(0x12, 0x20, 0x16),
        tool_error_bg: Color::Rgb(0x24, 0x11, 0x13),
        plan_bg: Color::Rgb(0x3a, 0x30, 0x55),
        // 工具输出与思考链同用中性灰(去掉调色板的蓝色偏向)
        tool_output: Color::Rgb(0x8c, 0x8c, 0x8c),
        error: Color::Rgb(0xf7, 0x76, 0x8e),
        warning: Color::Rgb(0xe0, 0xaf, 0x68),
        success: Color::Rgb(0x9e, 0xce, 0x6a),
        muted: Color::Rgb(0x96, 0x9e, 0xb6),
        dim: Color::Rgb(0x7e, 0x85, 0x97),
        md_heading: Color::Rgb(0xe0, 0xaf, 0x68),
        md_link: Color::Rgb(0x7a, 0xa2, 0xf7),
        md_code: Color::Rgb(0xff, 0x9e, 0x64),
        md_code_block_border: Color::Rgb(0x3b, 0x42, 0x61),
        border_idle: Color::Rgb(0x3b, 0x42, 0x61),
        border_busy: Color::Rgb(0xbb, 0x9a, 0xf7),
        border_bash: Color::Rgb(0x9e, 0xce, 0x6a),
        spinner: Color::Rgb(0xbb, 0x9a, 0xf7),
        footer_cwd: Color::Rgb(0xff, 0x9e, 0x64),
        usage_input: Color::Rgb(0x7d, 0xcf, 0xff),
        usage_output: Color::Rgb(0x9e, 0xce, 0x6a),
        usage_cache: Color::Rgb(0xbb, 0x9a, 0xf7),
        usage_cost: Color::Rgb(0xe0, 0xaf, 0x68),
        usage_ctx: Color::Rgb(0x7a, 0xa2, 0xf7),
        usage_reasoning: Color::Rgb(0x7a, 0xa2, 0xf7),
        popup_border: Color::Rgb(0x6b, 0x73, 0x94),
        is_dark: true,
    }
}

/// ANSI 16 色兜底(基本色相尽量对齐 dark 调色板的语义;accent 用黄少蓝)。
pub fn dark_ansi() -> Theme {
    Theme {
        accent: Color::Yellow,
        user_bg: Color::DarkGray,
        user_text: Color::White,
        assistant_text: Color::Gray,
        thinking: Color::DarkGray,
        tool_title: Color::Cyan,
        tool_pending: Color::Yellow,
        tool_success: Color::Green,
        tool_error: Color::Red,
        // 16 色调色板无柔和中间色:成功/失败用基本色相的暗档(Green/Red 即
        // ANSI 2/1),运行中与计划块用 DarkGray 保守降级
        tool_pending_bg: Color::DarkGray,
        tool_success_bg: Color::Green,
        tool_error_bg: Color::Red,
        plan_bg: Color::DarkGray,
        // 工具输出与思考链同色(16 色下调到 DarkGray 中性灰)
        tool_output: Color::DarkGray,
        error: Color::Red,
        warning: Color::Yellow,
        success: Color::Green,
        muted: Color::Gray,
        dim: Color::Gray,
        md_heading: Color::Yellow,
        md_link: Color::Blue,
        md_code: Color::Magenta,
        md_code_block_border: Color::DarkGray,
        border_idle: Color::DarkGray,
        border_busy: Color::Magenta,
        border_bash: Color::Green,
        spinner: Color::Magenta,
        footer_cwd: Color::Yellow,
        usage_input: Color::Cyan,
        usage_output: Color::Green,
        usage_cache: Color::Magenta,
        usage_cost: Color::Yellow,
        usage_ctx: Color::Blue,
        usage_reasoning: Color::Blue,
        popup_border: Color::DarkGray,
        is_dark: true,
    }
}
