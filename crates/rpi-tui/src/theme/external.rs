//! ratatui-themes → 语义角色的映射层(主题数据与逻辑分离的边界)。
//!
//! 分两层:
//! 1. **统一推导规则**:`ThemePalette` 的 10 个语义字段直接映射同名角色,
//!    缺失角色(dim/border/user_bg 等)由 `bg`/`fg`/`muted` 按比例混色推导
//!    —— 混色方向由 bg/fg 自身的明暗决定,深浅色主题共用同一公式;
//! 2. **精选微调**:Tokyo Night / Catppuccin Mocha / Dracula / Nord /
//!    Rosé Pine 五个主题对 user_bg、border、md_code 等角色用官方色值覆盖,
//!    其余主题走统一规则。

use ratatui::style::Color;
pub use ratatui_themes::ThemeName;

use super::Theme;

/// 主题名 → 语义主题(统一规则 + 精选微调)。
pub fn from_name(name: ThemeName) -> Theme {
    let p = name.palette();
    let mut theme = Theme {
        accent: p.accent,
        user_bg: blend(p.bg, p.fg, 0.10),
        user_text: p.fg,
        assistant_text: p.fg,
        thinking: p.muted,
        tool_title: p.info,
        tool_pending: p.warning,
        tool_success: p.success,
        tool_error: p.error,
        tool_output: blend(p.fg, p.muted, 0.30),
        error: p.error,
        warning: p.warning,
        success: p.success,
        muted: p.muted,
        dim: blend(p.muted, p.bg, 0.25),
        md_heading: p.warning,
        md_link: p.accent,
        md_code: blend(p.warning, p.error, 0.45),
        md_code_block_border: blend(p.bg, p.fg, 0.18),
        border_idle: blend(p.bg, p.fg, 0.18),
        border_busy: p.secondary,
        border_bash: p.success,
        spinner: p.secondary,
        footer_cwd: p.accent,
        is_dark: p.is_dark(),
    };
    refine(name, &mut theme);
    theme
}

/// 精选微调表:对齐各主题官方 UI 配色的低饱和角色。
fn refine(name: ThemeName, t: &mut Theme) {
    match name {
        ThemeName::TokyoNight => {
            // 与 pi dark 同源:selection 做底、3b4261 做边、ff9e64 做代码
            t.user_bg = Color::Rgb(0x29, 0x2e, 0x42);
            t.border_idle = Color::Rgb(0x3b, 0x42, 0x61);
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xff, 0x9e, 0x64);
        }
        ThemeName::CatppuccinMocha => {
            t.user_bg = Color::Rgb(0x31, 0x32, 0x44); // surface0
            t.border_idle = Color::Rgb(0x45, 0x47, 0x5a); // surface1
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xfa, 0xb3, 0x87); // peach
        }
        ThemeName::Dracula => {
            t.user_bg = Color::Rgb(0x44, 0x47, 0x5a); // selection
            t.border_idle = Color::Rgb(0x44, 0x47, 0x5a);
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xff, 0xb8, 0x6c); // orange
        }
        ThemeName::Nord => {
            t.user_bg = Color::Rgb(0x3b, 0x42, 0x52); // polar night 2
            t.border_idle = Color::Rgb(0x43, 0x4c, 0x5e); // polar night 3
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0x8f, 0xbc, 0xbb); // frost teal
        }
        ThemeName::RosePine => {
            t.user_bg = Color::Rgb(0x1f, 0x1d, 0x2e); // surface
            t.border_idle = Color::Rgb(0x26, 0x23, 0x3a); // overlay
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xeb, 0xbc, 0xba); // gold
        }
        _ => {}
    }
}

/// 线性混色:`t` 为 a→b 的插值比例(0 = 纯 a,1 = 纯 b)。
fn blend(a: Color, b: Color, t: f32) -> Color {
    let (ar, ag, ab) = rgb(a);
    let (br, bg, bb) = rgb(b);
    let mix = |x: u8, y: u8| {
        let v = f32::from(x) * (1.0 - t) + f32::from(y) * t;
        v.round().clamp(0.0, 255.0) as u8
    };
    Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
}

fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        // ratatui-themes 的调色板全部是 Rgb;其余枚举值仅为防御性兜底
        Color::Black => (0x00, 0x00, 0x00),
        Color::White => (0xff, 0xff, 0xff),
        _ => (0x80, 0x80, 0x80),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_themes_map_without_degenerate_roles() {
        for &name in ThemeName::all() {
            let t = from_name(name);
            // 每个角色都非默认黑;accent 与 dim 可区分(避免界面糊成一片)
            for (role, color) in [
                ("accent", t.accent),
                ("user_bg", t.user_bg),
                ("user_text", t.user_text),
                ("assistant_text", t.assistant_text),
                ("tool_title", t.tool_title),
                ("tool_output", t.tool_output),
                ("muted", t.muted),
                ("dim", t.dim),
                ("border_idle", t.border_idle),
                ("footer_cwd", t.footer_cwd),
            ] {
                assert_ne!(color, Color::Black, "{name}: {role} 退化为黑色");
            }
            assert_ne!(t.accent, t.dim, "{name}: accent 与 dim 同色");
        }
    }

    #[test]
    fn light_dark_flags_follow_palette() {
        assert!(from_name(ThemeName::Dracula).is_dark);
        assert!(from_name(ThemeName::CatppuccinMocha).is_dark);
        assert!(!from_name(ThemeName::CatppuccinLatte).is_dark);
        assert!(!from_name(ThemeName::GruvboxLight).is_dark);
        assert!(!from_name(ThemeName::SolarizedLight).is_dark);
    }

    #[test]
    fn curated_overrides_apply() {
        let t = from_name(ThemeName::TokyoNight);
        assert_eq!(t.md_code, Color::Rgb(0xff, 0x9e, 0x64));
        assert_eq!(t.border_idle, Color::Rgb(0x3b, 0x42, 0x61));
        let mocha = from_name(ThemeName::CatppuccinMocha);
        assert_eq!(mocha.user_bg, Color::Rgb(0x31, 0x32, 0x44));
    }

    #[test]
    fn tokyo_night_matches_legacy_dark_semantics() {
        // 默认主题与旧 dark() 的关键角色保持同色,切换默认不产生视觉跳变
        let t = from_name(ThemeName::TokyoNight);
        let legacy = super::super::builtin::dark();
        assert_eq!(t.accent, legacy.accent);
        assert_eq!(t.thinking, legacy.thinking);
        assert_eq!(t.border_busy, legacy.border_busy);
        assert_eq!(t.success, legacy.success);
    }

    #[test]
    fn blend_interpolates_linearly() {
        assert_eq!(blend(Color::Rgb(0, 0, 0), Color::Rgb(100, 100, 100), 0.5), Color::Rgb(50, 50, 50));
        assert_eq!(blend(Color::Rgb(10, 20, 30), Color::Rgb(10, 20, 30), 0.7), Color::Rgb(10, 20, 30));
    }
}
