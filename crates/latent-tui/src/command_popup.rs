//! 斜杠命令自动补全弹窗(Codex CLI 风格):输入以 `/` 开头时在编辑器上方
//! 弹出命令列表;按查询过滤(前缀 > 子串 > 模糊子序列),↑/↓ 选择、
//! Tab/Enter 补全、Esc 关闭。
//!
//! 参数变体:条目可声明 `variants`(通用机制,组件不感知语义)—— 查询
//! 首词精确命中带变体的命令时,列表替换为 `命令 参数` 子项(按声明序),
//! 参数前缀继续过滤(如 `/mode c` 只剩 confirm)。Tab 补全后进入单项精确
//! 态,再次 Enter 执行;Enter 对变体行(直接执行)与带变体裸命令行(展开
//! 变体选择页)的分派由上层按键处理决定。
//!
//! 组件不感知命令语义:命令表由上层注入(依赖方向约束:不知道 agent 的
//! 存在)。状态(过滤/选中/关闭)与渲染分离,可纯单测。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::popup;
use crate::theme::Theme;
use crate::width::{display_width, truncate_to_width};

/// 弹窗条目:`name` 不含前导 `/`。
#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    /// 参数变体(变体名, 语义描述):声明后,查询首词精确命中本命令时
    /// 列表展开为 `name 变体` 子项(声明序,不重排)
    pub variants: Vec<(String, String)>,
}

impl CommandEntry {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        CommandEntry {
            name: name.into(),
            description: description.into(),
            variants: Vec::new(),
        }
    }

    /// 声明参数变体(builder)。
    pub fn with_variants(mut self, variants: Vec<(String, String)>) -> Self {
        self.variants = variants;
        self
    }
}

/// 默认最多展示的匹配行数(超出滚动,选中项保持可见)。
pub const MAX_VISIBLE_ROWS: usize = 8;

/// 补全弹窗状态。
#[derive(Debug, Clone, Default)]
pub struct CommandPopup {
    /// 静态命令表(变体展开不修改本表;匹配结果存克隆)
    entries: Vec<CommandEntry>,
    /// 当前查询(去掉前导 `/` 后的输入)
    query: String,
    /// 匹配条目(静态命中或 `命令 参数` 变体展开)
    matches: Vec<CommandEntry>,
    /// 选中项在 `matches` 中的下标
    selected: usize,
    /// Esc 显式关闭;查询再次变化时重新打开
    dismissed: bool,
}

impl CommandPopup {
    pub fn new(entries: Vec<CommandEntry>) -> Self {
        let mut popup = CommandPopup {
            entries,
            ..Default::default()
        };
        popup.sync("");
        popup
    }

    /// 编辑器内容变化后调用:重算查询、过滤与可见性。返回是否可见。
    pub fn sync(&mut self, input: &str) -> bool {
        let query = input.strip_prefix('/').unwrap_or("").to_string();
        let active = input.starts_with('/');
        if query != self.query {
            self.dismissed = false;
            self.selected = 0;
            self.query = query;
        }
        self.matches = if active {
            self.filtered(&self.query)
        } else {
            Vec::new()
        };
        self.clamp_selected();
        self.visible()
    }

    pub fn visible(&self) -> bool {
        !self.dismissed && !self.matches.is_empty()
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
        }
    }

    pub fn selected_entry(&self) -> Option<&CommandEntry> {
        self.matches.get(self.selected)
    }

    /// 查询与某条目名完全一致:Enter 语义为执行而非补全。
    pub fn is_exact_match(&self) -> bool {
        let query = self.query.trim_end();
        self.matches.iter().any(|entry| entry.name == query)
    }

    /// 选中项的补全文本:静态命令带尾随空格(补全后弹窗退场);变体子项
    /// (`命令 参数`)不带 —— 补全后进入单项精确态,再次 Enter 执行。
    pub fn complete_text(&self) -> Option<String> {
        self.selected_entry().map(|entry| {
            if entry.name.contains(char::is_whitespace) {
                format!("/{}", entry.name)
            } else {
                format!("/{name} ", name = entry.name)
            }
        })
    }

    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }

    /// 渲染圆角边框弹窗;`max_rows` 限制可见行数(滚动窗口)。
    pub fn render(&self, width: usize, theme: &Theme, max_rows: usize) -> Vec<Line<'static>> {
        if !self.visible() {
            return Vec::new();
        }
        let width = width.max(4);
        let rows = self.matches.len().min(max_rows.max(1));
        let start = self.selected.saturating_sub(rows - 1);
        let window: &[CommandEntry] = &self.matches[start..start + rows];
        let name_w = window
            .iter()
            .map(|entry| display_width(&entry.name) + 1)
            .max()
            .unwrap_or(0);
        // 内容量:两侧边框各占 2 列
        let inner_w = width.saturating_sub(4).max(1);

        let mut content = Vec::with_capacity(window.len());
        for (row, entry) in window.iter().enumerate() {
            let is_selected = start + row == self.selected;
            let marker = if is_selected { "❯ " } else { "  " };
            let name_style = if is_selected {
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.assistant_text)
            };
            let desc_style = Style::new().fg(theme.muted);
            // name 列(带 `/` 前缀)补齐对齐;desc 截断到剩余宽度
            let pad = name_w.saturating_sub(display_width(&entry.name) + 1);
            let fixed = 2 + display_width(&entry.name) + 1 + pad + 2;
            let desc = truncate_to_width(&entry.description, inner_w.saturating_sub(fixed)).0;
            let used = fixed + display_width(&desc);
            let trailing = " ".repeat(inner_w.saturating_sub(used));
            content.push(Line::from(vec![
                Span::styled(marker.to_string(), name_style),
                Span::styled(format!("/{}", entry.name), name_style),
                Span::raw(" ".repeat(pad)),
                Span::raw("  "),
                Span::styled(desc.to_string(), desc_style),
                Span::raw(trailing),
            ]));
        }
        popup::frame(content, width, theme)
    }

    fn clamp_selected(&mut self) {
        if self.selected >= self.matches.len() {
            self.selected = 0;
        }
    }

    /// 匹配打分:前缀(0)> 子串(1)> 模糊子序列(2);同级按名字排序。
    /// 查询首词精确命中带变体的命令时,列表展开为变体子项(声明序)。
    fn filtered(&self, query: &str) -> Vec<CommandEntry> {
        let q = query.to_ascii_lowercase();
        // 查询首词精确命中带变体的命令(带或不带参数)→ 展开变体子项
        let (first, arg) = match q.split_once(char::is_whitespace) {
            Some((first, arg)) => (first, Some(arg.trim())),
            None => (q.as_str(), None),
        };
        if let Some(entry) = self.entries.iter().find(|entry| {
            entry.name.to_ascii_lowercase() == first && !entry.variants.is_empty()
        }) {
            return entry
                .variants
                .iter()
                .filter(|(variant, _)| arg.is_none_or(|arg| variant.starts_with(arg)))
                .map(|(variant, desc)| {
                    CommandEntry::new(format!("{} {}", entry.name, variant), desc.clone())
                })
                .collect();
        }
        if arg.is_some() {
            // 命令名后出现空白且命令无变体:弹窗退场(开始输自由参数)
            return Vec::new();
        }
        let mut scored: Vec<(u8, &str, CommandEntry)> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let name = entry.name.to_ascii_lowercase();
                let score = if name.starts_with(&q) {
                    0
                } else if name.contains(&q) {
                    1
                } else if is_subsequence(&name, &q) {
                    2
                } else {
                    return None;
                };
                Some((score, entry.name.as_str(), entry.clone()))
            })
            .collect();
        scored.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        scored.into_iter().map(|(_, _, entry)| entry).collect()
    }
}

/// 模糊子序列:`q` 的字符按序出现在 `name` 中。
fn is_subsequence(name: &str, q: &str) -> bool {
    let mut chars = name.chars();
    q.chars().all(|qc| chars.any(|nc| nc == qc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;

    fn popup() -> CommandPopup {
        CommandPopup::new(vec![
            CommandEntry::new("model", "查看/切换模型"),
            CommandEntry::new("memory", "记忆"),
            CommandEntry::new("help", "显示帮助"),
        ])
    }

    #[test]
    fn sync_opens_on_slash_and_filters_by_prefix() {
        let mut p = popup();
        assert!(!p.sync(""), "空输入不可见");
        assert!(!p.sync("hello"), "非斜杠不可见");
        assert!(p.sync("/m"));
        assert_eq!(p.match_count(), 2, "model/memory 前缀匹配");
        // 同分按字母序:memory 在 model 前
        assert_eq!(p.selected_entry().map(|e| e.name.as_str()), Some("memory"));
    }

    #[test]
    fn whitespace_or_no_match_hides() {
        let mut p = popup();
        assert!(!p.sync("/model "), "出现空白后隐藏");
        assert!(!p.visible());
        assert!(!p.sync("/zzz"), "无匹配时隐藏");
        assert!(!p.visible());
        assert!(p.sync("/model"));
        assert!(p.visible(), "查询回退后恢复");
    }

    #[test]
    fn esc_dismisses_until_query_changes() {
        let mut p = popup();
        p.sync("/m");
        p.dismiss();
        assert!(!p.visible());
        p.sync("/mo");
        assert!(p.visible(), "查询变化后重新打开");
    }

    #[test]
    fn exact_match_and_complete_text() {
        let mut p = popup();
        p.sync("/model");
        assert!(p.is_exact_match());
        assert_eq!(p.complete_text().as_deref(), Some("/model "));
        p.sync("/mo");
        assert!(!p.is_exact_match());
    }

    #[test]
    fn navigation_clamps_at_edges() {
        let mut p = popup();
        p.sync("/m");
        p.move_up();
        assert_eq!(p.selected, 0);
        p.move_down();
        p.move_down();
        assert_eq!(p.selected, 1);
        p.move_down();
        assert_eq!(p.selected, 1);
    }

    #[test]
    fn render_is_boxed_with_selected_marker() {
        let mut p = popup();
        p.sync("/m");
        let lines = p.render(40, &Theme::dark_ansi(), 8);
        assert_eq!(lines.len(), 4, "上下边框 + 2 匹配行: {lines:?}");
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts[0].starts_with('╭'));
        assert!(texts[1].starts_with("│ ❯ /memory"), "{texts:?}");
        assert!(texts[2].starts_with("│   /model"));
        assert!(texts[3].starts_with('╰'));
        for text in &texts {
            assert!(display_width(text) <= 40, "{text:?}");
        }
    }

    #[test]
    fn render_scrolls_to_keep_selection_visible() {
        let entries: Vec<CommandEntry> = (0..12)
            .map(|i| CommandEntry::new(format!("cmd{i:02}"), "d"))
            .collect();
        let mut p = CommandPopup::new(entries);
        p.sync("/cmd");
        assert_eq!(p.match_count(), 12);
        // 窗口 8 行:选中第 9 项(下标 8)时 cmd00 滚出窗口
        for _ in 0..8 {
            p.move_down();
        }
        let lines = p.render(40, &Theme::dark_ansi(), 8);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("cmd08")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("cmd00")));
    }

    #[test]
    fn variants_expand_on_exact_command_and_filter_by_arg_prefix() {
        let mut p = CommandPopup::new(vec![CommandEntry::new("mode", "切换模式").with_variants(
            vec![
                ("plan".into(), "只读".into()),
                ("confirm".into(), "确认".into()),
                ("full-access".into(), "全自动".into()),
            ],
        )]);
        // 输入 /mode:直接展开三个子项(声明序,不按字母重排)
        assert!(p.sync("/mode"));
        let names: Vec<&str> = (0..p.match_count())
            .map(|i| p.matches[i].name.as_str())
            .collect();
        assert_eq!(names, vec!["mode plan", "mode confirm", "mode full-access"]);
        // 参数前缀继续过滤
        assert!(p.sync("/mode c"));
        assert_eq!(p.match_count(), 1);
        assert_eq!(
            p.selected_entry().map(|e| e.name.as_str()),
            Some("mode confirm")
        );
        // 补全不带尾随空格 → 单项精确态 → Enter 语义为执行
        assert_eq!(p.complete_text().as_deref(), Some("/mode confirm"));
        assert!(p.sync("/mode confirm"));
        assert!(p.is_exact_match(), "补全后应为精确匹配(Enter 执行)");
    }

    #[test]
    fn trailing_space_after_variant_arg_stays_exact() {
        let mut p = CommandPopup::new(vec![CommandEntry::new("mode", "切换模式").with_variants(
            vec![("plan".into(), "只读".into()), ("confirm".into(), "确认".into())],
        )]);
        p.sync("/mode confirm ");
        assert!(p.is_exact_match(), "尾随空格不影响精确匹配判定");
        assert_eq!(p.match_count(), 1);
    }

    #[test]
    fn fuzzy_subsequence_matches() {
        let mut p = popup();
        p.sync("/moel");
        assert_eq!(p.match_count(), 1, "moel 是 model 的模糊子序列");
    }
}
