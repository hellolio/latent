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
    // 正文统一中性色(assistant_text):取中性灰白而非调色板 fg,避免部分
    // 主题(如 Tokyo Night)的 fg 带明显色相偏向。
    let text = if p.is_dark() {
        Color::Rgb(0xd4, 0xd4, 0xd4)
    } else {
        Color::Rgb(0x38, 0x3a, 0x42)
    };
    // 用户块文字比正文暗一档(柔和);背景与中性灰混色,不带 fg 的色相
    let user_text = if p.is_dark() {
        Color::Rgb(0xbe, 0xbe, 0xbe)
    } else {
        Color::Rgb(0x4a, 0x4a, 0x4a)
    };
    // thinking:偏暗的中性灰(去掉调色板 muted 的蓝色偏向)
    let thinking = if p.is_dark() {
        Color::Rgb(0x8c, 0x8c, 0x8c)
    } else {
        Color::Rgb(0x76, 0x76, 0x76)
    };
    let mut theme = Theme {
        accent: p.accent,
        // 0.14:用户块背景要柔和他可辨,太暗会与终端底色混在一起
        user_bg: blend(p.bg, user_text, 0.14),
        user_text,
        assistant_text: text,
        thinking,
        tool_title: p.info,
        tool_pending: p.warning,
        tool_success: p.success,
        tool_error: p.error,
        tool_output: thinking,
        // 卡片背景:暗色主题以纯黑为底向语义色低比例混色——只比终端底色
        // (常见纯黑)稍带色相、明度压到最低,避免大面积发灰发白刺眼;
        // 浅色主题保持浅色 tint
        tool_pending_bg: if p.is_dark() {
            blend(Color::Black, p.fg, 0.10)
        } else {
            blend(p.bg, p.fg, 0.12)
        },
        tool_success_bg: if p.is_dark() {
            blend(Color::Black, p.success, 0.15)
        } else {
            blend(p.bg, p.success, 0.25)
        },
        tool_error_bg: if p.is_dark() {
            blend(Color::Black, p.error, 0.15)
        } else {
            blend(p.bg, p.error, 0.25)
        },
        plan_bg: blend(p.bg, p.secondary, 0.25),
        error: p.error,
        warning: p.warning,
        success: p.success,
        muted: p.muted,
        // dim 靠近 fg 一侧(而非 bg):footer/提示等大面积弱文字必须可读
        dim: blend(p.muted, p.fg, 0.25),
        md_heading: p.warning,
        md_link: p.accent,
        md_code: blend(p.warning, p.error, 0.45),
        md_code_block_border: blend(p.bg, p.fg, 0.18),
        border_idle: blend(p.bg, p.fg, 0.18),
        border_busy: p.secondary,
        border_bash: p.success,
        spinner: p.secondary,
        footer_cwd: p.accent,
        usage_input: p.info,
        usage_output: p.success,
        usage_cache: p.secondary,
        usage_cost: p.warning,
        usage_ctx: p.accent,
        usage_reasoning: p.accent,
        popup_border: blend(p.bg, p.fg, 0.45),
        is_dark: p.is_dark(),
    };
    refine(name, &mut theme);
    theme
}

/// 精选微调表:全部主题用各自官方 UI 色板覆盖低饱和角色,保证主题之间
/// 观感差异明显(统一规则只兜底,特色靠这里)。
fn refine(name: ThemeName, t: &mut Theme) {
    match name {
        ThemeName::TokyoNight => {
            // 去蓝化:accent/提示符/目录用橙色(pi 风格),大面积背景用
            // 低饱和石墨色,弱文字(dim)提亮保证可读
            t.accent = Color::Rgb(0xff, 0x9e, 0x64); // orange
            t.footer_cwd = t.accent;
            t.user_bg = Color::Rgb(0x33, 0x34, 0x38); // 中性石墨,去蓝
            t.border_idle = Color::Rgb(0x3b, 0x42, 0x61);
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xff, 0x9e, 0x64);
            t.popup_border = Color::Rgb(0x6b, 0x73, 0x94); // comment 提亮
            t.dim = Color::Rgb(0x7e, 0x85, 0x97); // 中性灰,弱文字可读
        }
        ThemeName::CatppuccinMocha => {
            t.user_bg = Color::Rgb(0x31, 0x32, 0x44); // surface0
            t.border_idle = Color::Rgb(0x45, 0x47, 0x5a); // surface1
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xfa, 0xb3, 0x87); // peach
            t.popup_border = Color::Rgb(0x6c, 0x70, 0x86); // overlay0
            t.usage_input = Color::Rgb(0x89, 0xb4, 0xfa); // blue
            t.usage_cache = Color::Rgb(0xcb, 0xa6, 0xf7); // mauve
        }
        ThemeName::CatppuccinLatte => {
            t.user_bg = Color::Rgb(0xdf, 0xdf, 0xe1); // surface0
            t.border_idle = Color::Rgb(0xcc, 0xd0, 0xda); // surface1
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xfe, 0x64, 0x0b); // orange? peach
            t.popup_border = Color::Rgb(0x9c, 0xa0, 0xa7); // overlay0
            t.usage_input = Color::Rgb(0x1e, 0x66, 0xf5); // blue
            t.usage_cache = Color::Rgb(0x88, 0x39, 0xef); // mauve
        }
        ThemeName::Dracula => {
            t.user_bg = Color::Rgb(0x44, 0x47, 0x5a); // selection
            t.border_idle = Color::Rgb(0x44, 0x47, 0x5a);
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xff, 0xb8, 0x6c); // orange
            t.popup_border = Color::Rgb(0x62, 0x64, 0x83); // comment
            t.usage_input = Color::Rgb(0x8b, 0xe9, 0xfd); // cyan
            t.usage_cache = Color::Rgb(0xff, 0x79, 0xc6); // pink
        }
        ThemeName::Nord => {
            t.user_bg = Color::Rgb(0x3b, 0x42, 0x52); // polar night 2
            t.border_idle = Color::Rgb(0x43, 0x4c, 0x5e); // polar night 3
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0x8f, 0xbc, 0xbb); // frost teal
            t.popup_border = Color::Rgb(0x4c, 0x56, 0x6a); // polar night 3.5
            t.usage_input = Color::Rgb(0x88, 0xc0, 0xd0); // frost
            t.usage_cache = Color::Rgb(0xb4, 0x8e, 0xad); // aurora purple
        }
        ThemeName::RosePine => {
            t.user_bg = Color::Rgb(0x1f, 0x1d, 0x2e); // surface
            t.border_idle = Color::Rgb(0x26, 0x23, 0x3a); // overlay
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xeb, 0xbc, 0xba); // gold
            t.popup_border = Color::Rgb(0x40, 0x3d, 0x52); // overlay
            t.usage_input = Color::Rgb(0x31, 0x84, 0xbf); // foam
            t.usage_cache = Color::Rgb(0xc4, 0xa7, 0xe7); // iris
        }
        ThemeName::GruvboxDark => {
            t.user_bg = Color::Rgb(0x3c, 0x38, 0x36); // bg1
            t.border_idle = Color::Rgb(0x50, 0x49, 0x45); // bg2
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xfe, 0x80, 0x19); // orange
            t.popup_border = Color::Rgb(0x66, 0x5c, 0x54); // bg4
            t.usage_input = Color::Rgb(0x83, 0xa5, 0x98); // aqua
            t.usage_cache = Color::Rgb(0xd3, 0x86, 0x9b); // purple
        }
        ThemeName::GruvboxLight => {
            t.user_bg = Color::Rgb(0xeb, 0xdb, 0xb2); // bg1
            t.border_idle = Color::Rgb(0xd5, 0xc4, 0xa1); // bg2
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xd6, 0x5d, 0x0e); // orange
            t.popup_border = Color::Rgb(0xbd, 0xae, 0x93); // bg4
            t.usage_input = Color::Rgb(0x4d, 0x76, 0x60); // aqua
            t.usage_cache = Color::Rgb(0x8f, 0x3f, 0x71); // purple
        }
        ThemeName::OneDarkPro => {
            t.user_bg = Color::Rgb(0x31, 0x37, 0x3b); // selection
            t.border_idle = Color::Rgb(0x3e, 0x44, 0x51);
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xd1, 0x9a, 0x66); // orange
            t.popup_border = Color::Rgb(0x5c, 0x63, 0x70); // comment
            t.usage_input = Color::Rgb(0x61, 0xaf, 0xef); // blue
            t.usage_cache = Color::Rgb(0xc6, 0x78, 0xdd); // purple
        }
        ThemeName::SolarizedDark => {
            t.user_bg = Color::Rgb(0x07, 0x36, 0x42); // base02
            t.border_idle = Color::Rgb(0x58, 0x6e, 0x75); // base01
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xcb, 0x4b, 0x16); // orange
            t.popup_border = Color::Rgb(0x58, 0x6e, 0x75); // base01
            t.usage_input = Color::Rgb(0x26, 0x8b, 0xd2); // blue
            t.usage_cache = Color::Rgb(0x6c, 0x71, 0xc4); // violet
        }
        ThemeName::SolarizedLight => {
            t.user_bg = Color::Rgb(0xee, 0xe8, 0xd5); // base2
            t.border_idle = Color::Rgb(0x93, 0xa1, 0xa1); // base1
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xcb, 0x4b, 0x16); // orange
            t.popup_border = Color::Rgb(0x93, 0xa1, 0xa1); // base1
            t.usage_input = Color::Rgb(0x26, 0x8b, 0xd2); // blue
            t.usage_cache = Color::Rgb(0x6c, 0x71, 0xc4); // violet
        }
        ThemeName::MonokaiPro => {
            t.user_bg = Color::Rgb(0x41, 0x41, 0x41); // dim selection
            t.border_idle = Color::Rgb(0x52, 0x52, 0x52);
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xfc, 0x98, 0x67); // orange
            t.popup_border = Color::Rgb(0x7a, 0x76, 0x6f); // comment
            t.usage_input = Color::Rgb(0x78, 0xdc, 0xe2); // cyan
            t.usage_cache = Color::Rgb(0xab, 0x9d, 0xf2); // purple
        }
        ThemeName::Kanagawa => {
            t.user_bg = Color::Rgb(0x2d, 0x4f, 0x67); // waveBlue1
            t.border_idle = Color::Rgb(0x22, 0x32, 0x44); // waveBlue0
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xff, 0x9e, 0x3b); // surimiOrange
            t.popup_border = Color::Rgb(0x2d, 0x4f, 0x67); // waveBlue1
            t.usage_input = Color::Rgb(0x7f, 0xb4, 0xca); // crystalBlue
            t.usage_cache = Color::Rgb(0x95, 0x7f, 0xb8); // oniViolet
        }
        ThemeName::Everforest => {
            t.user_bg = Color::Rgb(0x3a, 0x45, 0x3f); // bg1
            t.border_idle = Color::Rgb(0x4d, 0x58, 0x50); // bg3
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xe6, 0x98, 0x75); // orange
            t.popup_border = Color::Rgb(0x4d, 0x58, 0x50); // bg3
            t.usage_input = Color::Rgb(0x7f, 0xbb, 0xb3); // aqua
            t.usage_cache = Color::Rgb(0xdf, 0x69, 0x87); // red
        }
        ThemeName::Cyberpunk => {
            t.user_bg = Color::Rgb(0x21, 0x22, 0x33);
            t.border_idle = Color::Rgb(0x33, 0x34, 0x4c);
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xf7, 0xfd, 0x39); // neon yellow
            t.popup_border = Color::Rgb(0x51, 0x54, 0x70);
            t.usage_input = Color::Rgb(0x00, 0xd9, 0xff); // neon cyan
            t.usage_cache = Color::Rgb(0xff, 0x2b, 0xd5); // neon pink
        }
        ThemeName::MidnightCommander => {
            t.user_bg = Color::Rgb(0x2b, 0x2b, 0x33);
            t.border_idle = Color::Rgb(0x3f, 0x3f, 0x48);
            t.md_code_block_border = t.border_idle;
            t.md_code = Color::Rgb(0xb2, 0x6b, 0x00);
            t.popup_border = Color::Rgb(0x5f, 0x5f, 0x6b);
            t.usage_input = Color::Rgb(0x6b, 0xa9, 0xe0);
            t.usage_cache = Color::Rgb(0xb2, 0x8c, 0xd0);
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
                ("usage_input", t.usage_input),
                ("usage_output", t.usage_output),
                ("usage_cache", t.usage_cache),
                ("usage_cost", t.usage_cost),
                ("popup_border", t.popup_border),
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
    fn themes_are_visually_distinct() {
        // 全部主题的 user_bg/border_idle/md_code 两两不同(统一规则兜底之外,
        // 精选覆盖保证换主题时界面观感真的会变)
        let key = |name: ThemeName| {
            let t = from_name(name);
            (t.user_bg, t.border_idle, t.md_code, t.popup_border)
        };
        let names = ThemeName::all();
        for (i, &a) in names.iter().enumerate() {
            for &b in &names[i + 1..] {
                assert_ne!(key(a), key(b), "{a:?} 与 {b:?} 观感色完全相同");
            }
        }
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
