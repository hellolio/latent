//! 斜杠命令解析已下沉 `latent-runtime::slash`(与聊天端命令共用解析层);
//! 本模块 re-export 保持既有 `crate::modes::slash::…` 引用路径兼容。
//! `popup_entries()` 依赖 latent_tui::CommandEntry,是 interactive 专用
//! (TUI 补全弹窗),不进 runtime。

pub use latent_runtime::slash::{parse, help_markdown, SlashAction, SlashCommand, SlashInput, COMMANDS, MODE_VARIANTS, SESSION_VARIANTS};

/// 补全弹窗条目(静态命令表 + `/mode`/`/session` 参数变体 + `/skill`
/// 伪命令的技能变体)。`/skill <名称>` 不是命令:补全/过滤后留在输入框,
/// 用户继续输正文;技能表由调用方注入(与工具装配同源的发现结果)。
pub fn popup_entries(skills: &[latent_core::SkillDef]) -> Vec<latent_tui::CommandEntry> {
    let mut entries: Vec<latent_tui::CommandEntry> = COMMANDS
        .iter()
        .map(|command| {
            let entry = latent_tui::CommandEntry::new(command.name, command.description);
            if command.name == "mode" {
                entry.with_variants(
                    MODE_VARIANTS
                        .iter()
                        .map(|(name, desc)| ((*name).to_string(), (*desc).to_string()))
                        .collect(),
                )
            } else if command.name == "session" {
                entry.with_variants(
                    SESSION_VARIANTS
                        .iter()
                        .map(|(name, desc)| ((*name).to_string(), (*desc).to_string()))
                        .collect(),
                )
            } else {
                entry
            }
        })
        .collect();
    if !skills.is_empty() {
        entries.push(
            latent_tui::CommandEntry::new(
                "skill",
                "加载技能:SKILL.md 指南以 developer 角色随消息注入(补全后继续输入正文)",
            )
            .with_variants(
                skills
                    .iter()
                    .map(|skill| (skill.name.clone(), skill.description.clone()))
                    .collect(),
            ),
        );
    }
    entries
}
