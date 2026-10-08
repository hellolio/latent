//! ratatui-themes → 语义角色的映射层(主题数据与逻辑分离的边界)。
//!
//! 分两层:
//! 1. **统一推导规则**:语义色相全局一致,换主题只变色值不变布局语义——
//!    绿(success)=主色(accent/cwd/行内代码)、蓝(info)=信息主色(tool_title/
//!    链接/↑prompt)、蓝紫(info×secondary 混色)=信息第二色阶(ctx%/reasoning)、
//!    紫(secondary)=扩展活跃(subagent/spinner/cache)、黄(warning)=警示与
//!    git 分支、灰=统一中性弱文字(thinking/muted/dim);背景/边框等结构性
//!    角色由 `bg`/`fg` 按比例混色推导,深浅色主题共用同一公式;
//! 2. **精选微调**:各主题对 user_bg、border、popup_border 等结构性角色用
//!    官方色值覆盖,mode_plan 用各自官方粉;语义色相不微调。

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
    // thinking:偏暗的中性灰(去掉调色板 muted 的蓝色偏向);muted/dim/tool_output
    // 与之统一为同一弱文字灰
    let thinking = if p.is_dark() {
        Color::Rgb(0x8c, 0x8c, 0x8c)
    } else {
        Color::Rgb(0x76, 0x76, 0x76)
    };
    // 信息第二色阶:info 与 secondary 的中点(蓝紫过渡),与信息主色同段显示时
    // 可区分(中点必异于两端),又不逸出冷色信息家族
    let info_alt = blend(p.info, p.secondary, 0.5);
    // footer agent 值与 thinking 段:深色主题统一纯白(与弱文字灰拉开亮度
    // 差);浅色主题退化为正文字色,避免白底白字不可读
    let footer_white = if p.is_dark() {
        Color::Rgb(0xff, 0xff, 0xff)
    } else {
        Color::Rgb(0x38, 0x3a, 0x42)
    };
    let mut theme = Theme {
        // 绿=主色:与 success 同源,用于选中项/cwd/行内代码等强调
        accent: p.success,
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
        // 弱文字统一中性灰(与 thinking 同值):键名/提示/分隔符/系统通知一档
        muted: thinking,
        dim: thinking,
        md_heading: p.warning,
        md_link: p.info,
        // 行内代码用绿主色(与选中项/cwd 同源)
        md_code: p.success,
        md_code_block_border: blend(p.bg, p.fg, 0.18),
        border_idle: blend(p.bg, p.fg, 0.18),
        border_busy: p.secondary,
        border_bash: p.success,
        spinner: p.secondary,
        footer_cwd: p.success,
        footer_agent: footer_white,
        footer_thinking: footer_white,
        usage_input: p.info,
        usage_output: p.success,
        usage_cache: p.secondary,
        usage_cost: p.warning,
        // ctx%/reasoning 用信息第二色阶:同一 token 段内与 ↑prompt 的 info 主色可区分
        usage_ctx: info_alt,
        usage_reasoning: info_alt,
        // 粉红:secondary(多主题为紫/品红系)向 error(红)偏移;subagent
        // 独立色相直接取 secondary,与 plan 粉、success 绿区分
        mode_plan: blend(p.secondary, p.error, 0.4),
        subagent: p.secondary,
        popup_border: blend(p.bg, p.fg, 0.45),
        is_dark: p.is_dark(),
    };
    refine(name, &mut theme);
    theme
}

/// 精选微调表:只覆盖结构性角色(user_bg/边框/弹窗边框)与 mode_plan 官方粉,
/// 保证主题之间背景观感差异明显;语义色相(绿/灰/蓝/紫)不在此微调,
/// 由统一推导决定,避免污染全局语义映射。
fn refine(name: ThemeName, t: &mut Theme) {
    match name {
        ThemeName::TokyoNight => {
            t.mode_plan = Color::Rgb(0xff, 0x79, 0xc6); // pink
            t.subagent = Color::Rgb(0xbb, 0x9a, 0xf7); // purple
            // 大面积背景用低饱和石墨色,弱文字保持统一中性灰
            t.user_bg = Color::Rgb(0x33, 0x34, 0x38); // 中性石墨,去蓝
            t.border_idle = Color::Rgb(0x3b, 0x42, 0x61);
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x6b, 0x73, 0x94); // comment 提亮
        }
        ThemeName::CatppuccinMocha => {
            t.mode_plan = Color::Rgb(0xf5, 0xc2, 0xe7); // pink
            // 调色板 secondary 槽位放的即是官方粉,紫系改用官方 mauve 与粉区分
            t.subagent = Color::Rgb(0xcb, 0xa6, 0xf7);
            t.spinner = t.subagent;
            t.usage_cache = t.subagent;
            t.border_busy = t.subagent;
            t.user_bg = Color::Rgb(0x31, 0x32, 0x44); // surface0
            t.border_idle = Color::Rgb(0x45, 0x47, 0x5a); // surface1
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x6c, 0x70, 0x86); // overlay0
        }
        ThemeName::CatppuccinLatte => {
            t.mode_plan = Color::Rgb(0xea, 0x76, 0xcb); // pink
            // 同 Mocha:secondary 槽位为粉,紫系用官方 mauve
            t.subagent = Color::Rgb(0x88, 0x39, 0xef);
            t.spinner = t.subagent;
            t.usage_cache = t.subagent;
            t.border_busy = t.subagent;
            t.user_bg = Color::Rgb(0xdf, 0xdf, 0xe1); // surface0
            t.border_idle = Color::Rgb(0xcc, 0xd0, 0xda); // surface1
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x9c, 0xa0, 0xa7); // overlay0
        }
        ThemeName::Dracula => {
            t.mode_plan = Color::Rgb(0xff, 0x79, 0xc6); // pink
            // 调色板 secondary 槽位放的即是官方粉,紫系改用官方紫与粉区分
            t.subagent = Color::Rgb(0xbd, 0x93, 0xf9);
            t.spinner = t.subagent;
            t.usage_cache = t.subagent;
            t.border_busy = t.subagent;
            t.user_bg = Color::Rgb(0x44, 0x47, 0x5a); // selection
            t.border_idle = Color::Rgb(0x44, 0x47, 0x5a);
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x62, 0x64, 0x83); // comment
        }
        ThemeName::Nord => {
            t.mode_plan = Color::Rgb(0xb4, 0x8e, 0xad); // aurora purple
            t.user_bg = Color::Rgb(0x3b, 0x42, 0x52); // polar night 2
            t.border_idle = Color::Rgb(0x43, 0x4c, 0x5e); // polar night 3
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x4c, 0x56, 0x6a); // polar night 3.5
        }
        ThemeName::RosePine => {
            t.mode_plan = Color::Rgb(0xeb, 0xbc, 0xba); // rose
            t.user_bg = Color::Rgb(0x1f, 0x1d, 0x2e); // surface
            t.border_idle = Color::Rgb(0x26, 0x23, 0x3a); // overlay
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x40, 0x3d, 0x52); // overlay
        }
        ThemeName::GruvboxDark => {
            // mode_plan 走统一混色(secondary 向 error 偏移):调色板 secondary
            // 即官方紫 #d3869b,官方粉覆盖会与紫系撞色
            t.user_bg = Color::Rgb(0x3c, 0x38, 0x36); // bg1
            t.border_idle = Color::Rgb(0x50, 0x49, 0x45); // bg2
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x66, 0x5c, 0x54); // bg4
        }
        ThemeName::GruvboxLight => {
            // 同 GruvboxDark:secondary 即官方紫,mode_plan 走统一混色
            t.user_bg = Color::Rgb(0xeb, 0xdb, 0xb2); // bg1
            t.border_idle = Color::Rgb(0xd5, 0xc4, 0xa1); // bg2
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0xbd, 0xae, 0x93); // bg4
        }
        ThemeName::OneDarkPro => {
            // 同上:secondary 即官方紫 #c678dd,mode_plan 走统一混色
            t.user_bg = Color::Rgb(0x31, 0x37, 0x3b); // selection
            t.border_idle = Color::Rgb(0x3e, 0x44, 0x51);
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x5c, 0x63, 0x70); // comment
        }
        ThemeName::SolarizedDark => {
            t.mode_plan = Color::Rgb(0xd3, 0x36, 0x82); // magenta
            t.user_bg = Color::Rgb(0x07, 0x36, 0x42); // base02
            t.border_idle = Color::Rgb(0x58, 0x6e, 0x75); // base01
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x58, 0x6e, 0x75); // base01
        }
        ThemeName::SolarizedLight => {
            t.mode_plan = Color::Rgb(0xd3, 0x36, 0x82); // magenta
            t.user_bg = Color::Rgb(0xee, 0xe8, 0xd5); // base2
            t.border_idle = Color::Rgb(0x93, 0xa1, 0xa1); // base1
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x93, 0xa1, 0xa1); // base1
        }
        ThemeName::MonokaiPro => {
            t.mode_plan = Color::Rgb(0xf9, 0x26, 0x72); // pink/red
            t.user_bg = Color::Rgb(0x41, 0x41, 0x41); // dim selection
            t.border_idle = Color::Rgb(0x52, 0x52, 0x52);
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x7a, 0x76, 0x6f); // comment
        }
        ThemeName::Kanagawa => {
            t.user_bg = Color::Rgb(0x2d, 0x4f, 0x67); // waveBlue1
            t.border_idle = Color::Rgb(0x22, 0x32, 0x44); // waveBlue0
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x2d, 0x4f, 0x67); // waveBlue1
        }
        ThemeName::Everforest => {
            t.user_bg = Color::Rgb(0x3a, 0x45, 0x3f); // bg1
            t.border_idle = Color::Rgb(0x4d, 0x58, 0x50); // bg3
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x4d, 0x58, 0x50); // bg3
        }
        ThemeName::Cyberpunk => {
            t.user_bg = Color::Rgb(0x21, 0x22, 0x33);
            t.border_idle = Color::Rgb(0x33, 0x34, 0x4c);
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x51, 0x54, 0x70);
        }
        ThemeName::MidnightCommander => {
            t.user_bg = Color::Rgb(0x2b, 0x2b, 0x33);
            t.border_idle = Color::Rgb(0x3f, 0x3f, 0x48);
            t.md_code_block_border = t.border_idle;
            t.popup_border = Color::Rgb(0x5f, 0x5f, 0x6b);
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
        assert_eq!(t.user_bg, Color::Rgb(0x33, 0x34, 0x38));
        assert_eq!(t.border_idle, Color::Rgb(0x3b, 0x42, 0x61));
        assert_eq!(t.mode_plan, Color::Rgb(0xff, 0x79, 0xc6));
        let mocha = from_name(ThemeName::CatppuccinMocha);
        assert_eq!(mocha.user_bg, Color::Rgb(0x31, 0x32, 0x44));
    }

    #[test]
    fn semantic_scheme_is_unified() {
        // 语义色相全局钉死:绿=主色、灰=统一弱文字、蓝=信息主色、蓝紫=信息
        // 第二色阶、紫=扩展活跃、黄=警示;防止后续微调重新污染语义映射
        for &name in ThemeName::all() {
            let t = from_name(name);
            // 绿系:选中项/cwd/行内代码与 success 同源
            assert_eq!(t.accent, t.success, "{name}: accent 应为绿主色");
            assert_eq!(t.footer_cwd, t.success, "{name}: cwd 应为绿主色");
            assert_eq!(t.md_code, t.success, "{name}: 行内代码应为绿主色");
            // 灰系:弱文字与 thinking 统一
            assert_eq!(t.muted, t.thinking, "{name}: muted 应与 thinking 同灰");
            assert_eq!(t.dim, t.thinking, "{name}: dim 应与 thinking 同灰");
            assert_eq!(t.tool_output, t.thinking, "{name}: 工具输出应与 thinking 同灰");
            // 蓝系:信息主色
            assert_eq!(t.md_link, t.tool_title, "{name}: 链接应为信息主色");
            assert_eq!(t.usage_input, t.tool_title, "{name}: ↑prompt 应为信息主色");
            // 蓝紫:信息第二色阶,与主色可区分
            assert_eq!(t.usage_ctx, t.usage_reasoning, "{name}: ctx/reasoning 应同阶");
            assert_ne!(t.usage_ctx, t.usage_input, "{name}: ctx 与 ↑prompt 应可区分");
            // 紫系:扩展活跃,且与信息主色可区分
            assert_eq!(t.subagent, t.spinner, "{name}: subagent 应为紫");
            assert_eq!(t.usage_cache, t.spinner, "{name}: cache 应为紫");
            assert_ne!(t.subagent, t.usage_input, "{name}: 紫系与信息主色应可区分");
            // 黄系:警示
            assert_eq!(t.usage_cost, t.warning, "{name}: $cost 应为黄");
            // 粉(模式)与紫(扩展)可区分
            assert_ne!(t.mode_plan, t.subagent, "{name}: plan 粉与 subagent 紫应可区分");
            // token 段渲染四色两两可区分
            let token_hues = [t.usage_input, t.usage_output, t.usage_ctx, t.usage_cost];
            for (i, &a) in token_hues.iter().enumerate() {
                for &b in &token_hues[i + 1..] {
                    assert_ne!(a, b, "{name}: token 段四色出现重合");
                }
            }
        }
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
