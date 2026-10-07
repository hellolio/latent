//! 斜杠命令解析已下沉 `latent-runtime::slash`(与聊天端命令共用解析层);
//! 本模块 re-export 保持既有 `crate::modes::slash::…` 引用路径兼容。
//! `popup_entries()` 依赖 latent_tui::CommandEntry,是 interactive 专用
//! (TUI 补全弹窗),不进 runtime。

pub use latent_runtime::slash::{parse, help_markdown, SlashAction, SlashCommand, SlashInput, COMMANDS, MODE_VARIANTS, SESSION_VARIANTS};

/// 补全弹窗条目(静态命令表 + `/mode` 参数变体)。
pub fn popup_entries() -> Vec<latent_tui::CommandEntry> {
    COMMANDS
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
        .collect()
}
