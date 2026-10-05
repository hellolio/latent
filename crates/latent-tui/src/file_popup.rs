//! `@` 文件选择弹窗(pi autocomplete 的 `@` 前缀对应物):光标前 token 以
//! `@` 开头时,在编辑器上方列出候选文件/目录;↑/↓ 选择、Tab/Enter 补全、
//! Esc 关闭(query 变化后重开)。
//!
//! 与 CommandPopup 的差异:触发锚定光标 token(而非整行前缀);补全是
//! token 级替换(由上层经 Editor::replace_token_before_cursor 应用)——
//! 文件补全带尾随空格,token 随空白终结、弹窗自然退场;目录补全带 `/`
//! 尾缀,query 进入下钻语义,弹窗保持。路径含空白时用 `@"` 引号形式,
//! 保证含空格目录可继续下钻。
//!
//! **全路径补全**:设置 `base`(工作目录)后,补全文本为 `@绝对路径`——
//! 弹窗选中与手输的区分就在这里(选中 = 全路径,手输 = 原样);过滤前
//! 会把 token 里的 base 前缀剥掉,绝对路径 token 的下钻/打分照常工作。
//!
//! 组件不感知文件系统:候选集由上层采集注入(依赖方向约束:latent-tui
//! 零内部依赖,不做目录遍历,也不感知 searchIgnore)。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::text::truncate_line;
use crate::theme::Theme;
use crate::width::display_width;

/// 弹窗候选条目:相对路径(`/` 分隔;目录不含尾缀 `/`,渲染时补)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub is_dir: bool,
}

impl FileEntry {
    pub fn new(path: impl Into<String>, is_dir: bool) -> Self {
        FileEntry { path: path.into(), is_dir }
    }

    /// 文件名(路径最后一段)。
    fn basename(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// 单次展示的匹配上限(打分排序后截断;导航不至于在数千条里翻页)。
const MAX_MATCHES: usize = 50;

/// 文件选择弹窗状态。
#[derive(Debug, Clone, Default)]
pub struct FilePopup {
    /// 上层注入的候选集(采集自工作区;组件只读不改)
    entries: Vec<FileEntry>,
    /// 补全根的绝对路径前缀(恒以 `/` 结尾;空 = 补全相对路径)。设置后
    /// 补全文本为 `@{base}{相对路径}`,过滤前同步剥离 token 中的该前缀。
    base: String,
    /// 当前查询(去掉 `@` / `@"` 前缀与 base 前缀后的相对路径)
    query: String,
    /// 匹配条目(已按打分排序并截断)
    matches: Vec<FileEntry>,
    /// 选中项在 `matches` 中的下标
    selected: usize,
    /// Esc 显式关闭;query 再次变化时重新打开
    dismissed: bool,
}

impl FilePopup {
    /// 光标 token 变化后调用:`token` 为 `Some` 且以 `@` 开头时激活(兼容
    /// `@"` 引号形式),否则失活。返回是否可见。
    pub fn sync(&mut self, token: Option<&str>) -> bool {
        let mut query = token
            .and_then(|t| t.strip_prefix("@\"").or_else(|| t.strip_prefix('@')))
            .unwrap_or("")
            .to_string();
        // 绝对路径 token:剥掉 base 前缀,过滤仍按相对路径进行
        if !self.base.is_empty() {
            if let Some(rest) = query.strip_prefix(self.base.as_str()) {
                query = rest.to_string();
            }
        }
        if query != self.query {
            self.dismissed = false;
            self.selected = 0;
            self.query = query;
        }
        let active = token.is_some_and(|t| t.starts_with('@'));
        self.matches = if active {
            self.filtered(&self.query)
        } else {
            Vec::new()
        };
        self.clamp_selected();
        self.visible()
    }

    /// 注入补全根的绝对路径(自动补尾缀 `/`;空串 = 补全相对路径)。
    pub fn set_base(&mut self, base: impl Into<String>) {
        let mut base = base.into();
        if !base.is_empty() && !base.ends_with('/') {
            base.push('/');
        }
        self.base = base;
    }

    /// 注入候选集(重复注入覆盖;过滤在下一次 sync 重算)。
    pub fn set_entries(&mut self, entries: Vec<FileEntry>) {
        self.entries = entries;
    }

    /// 清空候选集(弹窗失活时上层调用,释放内存并强制下次重新采集)。
    pub fn clear_entries(&mut self) {
        self.entries.clear();
        self.matches.clear();
    }

    pub fn visible(&self) -> bool {
        !self.dismissed && !self.matches.is_empty()
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// 当前匹配集(打分排序后;上层测试/诊断用)。
    pub fn matches(&self) -> &[FileEntry] {
        &self.matches
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
        }
    }

    pub fn selected_entry(&self) -> Option<&FileEntry> {
        self.matches.get(self.selected)
    }

    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }

    /// 选中项的补全文本 + 是否保持弹窗。路径 = base 前缀 + 相对路径
    /// (设置 base 后即绝对全路径——弹窗选中与手输的区分点)。文件带
    /// 尾随空格(token 边界让弹窗退场,引号形式先补右引号);目录带 `/`
    /// (无空格,query 进入下钻语义,弹窗保持)。路径含空白时用 `@"…`
    /// 引号形式。
    pub fn complete_text(&self) -> Option<(String, bool)> {
        self.selected_entry().map(|entry| {
            let body = format!("{}{}", self.base, entry.path);
            if entry.is_dir {
                if body.contains(char::is_whitespace) {
                    (format!("@\"{body}/"), true)
                } else {
                    (format!("@{body}/"), true)
                }
            } else if body.contains(char::is_whitespace) {
                (format!("@\"{body}\" "), false)
            } else {
                (format!("@{body} "), false)
            }
        })
    }

    /// 渲染圆角边框弹窗;`max_rows` 限制可见行数(滚动窗口)。目录带 `/`
    /// 尾缀并以强调色区分。
    pub fn render(&self, width: usize, theme: &Theme, max_rows: usize) -> Vec<Line<'static>> {
        if !self.visible() {
            return Vec::new();
        }
        let width = width.max(4);
        let rows = self.matches.len().min(max_rows.max(1));
        let start = self.selected.saturating_sub(rows - 1);
        let window: &[FileEntry] = &self.matches[start..start + rows];
        let displays: Vec<String> = window
            .iter()
            .map(|entry| {
                if entry.is_dir {
                    format!("{}/", entry.path)
                } else {
                    entry.path.clone()
                }
            })
            .collect();
        let path_w = displays.iter().map(|d| display_width(d)).max().unwrap_or(0);
        // 内容量:两侧边框各占 2 列
        let inner_w = width.saturating_sub(4).max(1);

        let border = Style::new().fg(theme.popup_border);
        let dir_style = Style::new().fg(theme.accent);
        let mut lines = vec![Line::from(Span::styled(
            format!("╭{}", "─".repeat(width.saturating_sub(2))),
            border,
        ))];
        for (row, display) in displays.iter().enumerate() {
            let entry = &window[row];
            let is_selected = start + row == self.selected;
            let marker = if is_selected { "❯ " } else { "  " };
            let path_style = if is_selected {
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else if entry.is_dir {
                dir_style
            } else {
                Style::new().fg(theme.assistant_text)
            };
            let pad = path_w.saturating_sub(display_width(display));
            let used = 2 + display_width(marker) + display_width(display) + pad;
            let trailing = " ".repeat(inner_w.saturating_sub(used));
            lines.push(Line::from(vec![
                Span::styled("│ ".to_string(), border),
                Span::styled(marker.to_string(), path_style),
                Span::styled(display.clone(), path_style),
                Span::raw(" ".repeat(pad)),
                Span::raw(trailing),
                Span::styled(" │".to_string(), border),
            ]));
        }
        lines.push(Line::from(Span::styled(
            format!("╰{}", "─".repeat(width.saturating_sub(2))),
            border,
        )));
        lines.into_iter()
            .map(|line| truncate_line(line, width))
            .collect()
    }

    fn clamp_selected(&mut self) {
        if self.selected >= self.matches.len() {
            self.selected = 0;
        }
    }

    /// 过滤 + 打分:query 含 `/` 时进入下钻语义——`/` 前的目录前缀必须与
    /// 条目路径前缀一致,且只保留该目录的**直接子项**(对齐 pi 的目录
    /// readdir 行为),剩余片段对文件名打分;不含 `/` 时对整个工作区按
    /// 文件名前缀 > 文件名子串 > 路径子串 > 模糊子序列打分。同分内目录
    /// 优先,再按路径排序。
    fn filtered(&self, query: &str) -> Vec<FileEntry> {
        let q = query.to_ascii_lowercase();
        let (dir_prefix, base) = match q.rsplit_once('/') {
            Some((dir, base)) => (Some(format!("{dir}/")), base),
            None => (None, q.as_str()),
        };
        let mut scored: Vec<(u8, bool, String, FileEntry)> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let path = entry.path.to_ascii_lowercase();
                if let Some(prefix) = &dir_prefix {
                    if !path.starts_with(prefix.as_str()) {
                        return None;
                    }
                    // 直接子项:剩余段不得再含 `/`
                    if path[prefix.len()..].contains('/') {
                        return None;
                    }
                }
                let name = entry.basename().to_ascii_lowercase();
                // base 为空时 starts_with("") 恒真,全部条目归 0 分
                let score = if name.starts_with(base) {
                    0
                } else if name.contains(base) {
                    1
                } else if path.contains(base) {
                    2
                } else if is_subsequence(&name, base) {
                    3
                } else {
                    return None;
                };
                Some((score, !entry.is_dir, entry.path.clone(), entry.clone()))
            })
            .collect();
        scored.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
        scored.truncate(MAX_MATCHES);
        scored.into_iter().map(|(_, _, _, entry)| entry).collect()
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

    fn popup(entries: Vec<FileEntry>) -> FilePopup {
        let mut p = FilePopup::default();
        p.set_entries(entries);
        p
    }

    fn sample() -> FilePopup {
        popup(vec![
            FileEntry::new("README.md", false),
            FileEntry::new("src", true),
            FileEntry::new("src/lib.rs", false),
            FileEntry::new("src/main.rs", false),
            FileEntry::new("src/ui", true),
            FileEntry::new("src/ui/button.rs", false),
            FileEntry::new("tests", true),
            FileEntry::new("tests/e2e", true),
            FileEntry::new("tests/e2e/test_app.py", false),
        ])
    }

    #[test]
    fn sync_activates_on_at_token_and_deactivates_otherwise() {
        let mut p = sample();
        assert!(!p.sync(Some("hello")), "非 @ token 不激活");
        assert!(!p.sync(None), "无 token 不激活");
        assert!(p.sync(Some("@")));
        assert!(p.visible());
        assert!(!p.sync(Some("@zzz")), "无匹配时隐藏");
        assert!(p.sync(Some("@read")));
    }

    #[test]
    fn bare_query_scores_filename_prefix_then_substring() {
        let mut p = sample();
        p.sync(Some("@read"));
        assert_eq!(p.match_count(), 1, "README.md 文件名前缀");
        assert_eq!(p.selected_entry().unwrap().path, "README.md");
        // "es":tests 文件名子串(1)、test_app.py 文件名子串(1)、tests/e2e
        // 路径子串(2)——同分内目录优先,tests 居首
        p.sync(Some("@es"));
        assert_eq!(p.match_count(), 3);
        assert_eq!(p.selected_entry().unwrap().path, "tests");
    }

    #[test]
    fn empty_query_lists_all_dirs_first() {
        let mut p = sample();
        p.sync(Some("@"));
        // 同分(0)内目录优先、按路径排序
        assert_eq!(p.matches[0].path, "src");
        assert!(p.matches[0].is_dir);
        assert_eq!(p.match_count(), 9);
    }

    #[test]
    fn dir_descent_lists_direct_children() {
        let mut p = sample();
        p.sync(Some("@src/"));
        // 直接子项:ui(目录优先)在 lib.rs/main.rs 前;深层 button.rs 不出现
        let names: Vec<&str> = (0..p.match_count())
            .map(|i| p.matches[i].path.as_str())
            .collect();
        assert_eq!(names, vec!["src/ui", "src/lib.rs", "src/main.rs"], "{names:?}");
        // 下钻 tests:直接子项只有 e2e 目录
        p.sync(Some("@tests/"));
        assert_eq!(p.match_count(), 1);
        assert_eq!(p.selected_entry().unwrap().path, "tests/e2e");
    }

    #[test]
    fn descent_with_base_filters_children() {
        let mut p = sample();
        // button.rs 是 src/ui 的直接子项,两级下钻后按文件名前缀命中
        p.sync(Some("@src/ui/bu"));
        assert_eq!(p.match_count(), 1);
        assert_eq!(p.selected_entry().unwrap().path, "src/ui/button.rs");
    }

    #[test]
    fn quoted_prefix_strips_at_and_quote() {
        let mut p = sample();
        // `@"` 引号形式(路径含空白时):去掉两个前缀字符
        p.sync(Some("@\"read"));
        assert_eq!(p.match_count(), 1);
        assert_eq!(p.selected_entry().unwrap().path, "README.md");
    }

    #[test]
    fn complete_text_file_appends_space_dir_keeps_slash() {
        let mut p = sample();
        p.sync(Some("@read"));
        let (text, keeps) = p.complete_text().unwrap();
        assert_eq!(text, "@README.md ");
        assert!(!keeps, "文件补全后弹窗退场");
        p.sync(Some("@src"));
        let (text, keeps) = p.complete_text().unwrap();
        assert_eq!(text, "@src/");
        assert!(keeps, "目录补全后弹窗保持下钻");
    }

    #[test]
    fn complete_text_quotes_paths_with_whitespace() {
        let mut p = popup(vec![
            FileEntry::new("my dir", true),
            FileEntry::new("my dir/notes.txt", false),
        ]);
        p.sync(Some("@my"));
        let (text, keeps) = p.complete_text().unwrap();
        assert_eq!(text, "@\"my dir/");
        assert!(keeps);
        // 下钻到文件:引号闭合 + 尾随空格
        p.sync(Some("@\"my dir/no"));
        let (text, keeps) = p.complete_text().unwrap();
        assert_eq!(text, "@\"my dir/notes.txt\" ");
        assert!(!keeps);
    }

    #[test]
    fn base_completes_absolute_paths_and_filters_strip_it() {
        let mut p = popup(vec![
            FileEntry::new("README.md", false),
            FileEntry::new("src", true),
            FileEntry::new("src/main.rs", false),
        ]);
        p.set_base("/work/proj");
        assert_eq!(p.base, "/work/proj/", "自动补尾缀 /");
        // 手输部分绝对路径(已含完整 base 前缀):过滤剥离 base 后照常打分
        assert!(p.sync(Some("@/work/proj/rea")));
        assert_eq!(p.selected_entry().unwrap().path, "README.md");
        // 绝对路径补全 = 全路径(弹窗选中与手输的区分点)
        let (text, keeps) = p.complete_text().unwrap();
        assert_eq!(text, "@/work/proj/README.md ");
        assert!(!keeps);
        // 目录补全 = 全路径 + /(下钻 token 也是绝对形式)
        p.sync(Some("@/work/proj/src"));
        let (text, keeps) = p.complete_text().unwrap();
        assert_eq!(text, "@/work/proj/src/");
        assert!(keeps);
        // 下钻:绝对 token 剥离 base 后按相对路径取直接子项
        assert!(p.sync(Some("@/work/proj/src/")));
        assert_eq!(p.selected_entry().unwrap().path, "src/main.rs");
        // base 不匹配的绝对前缀(手输中)无匹配,弹窗隐藏
        assert!(!p.sync(Some("@/work/pro")));
    }

    #[test]
    fn base_with_quoted_paths() {
        let mut p = popup(vec![FileEntry::new("my dir/notes.txt", false)]);
        p.set_base("/work/proj");
        assert!(p.sync(Some("@\"/work/proj/my dir/no")));
        let (text, keeps) = p.complete_text().unwrap();
        assert_eq!(text, "@\"/work/proj/my dir/notes.txt\" ");
        assert!(!keeps);
    }

    #[test]
    fn esc_dismisses_until_query_changes() {
        let mut p = sample();
        p.sync(Some("@read"));
        p.dismiss();
        assert!(!p.visible());
        p.sync(Some("@readm"));
        assert!(p.visible(), "query 变化后重开(含回退到原 query)");
        p.sync(Some("@read"));
        assert!(p.visible());
    }

    #[test]
    fn navigation_clamps_at_edges() {
        let mut p = sample();
        p.sync(Some("@"));
        p.move_up();
        assert_eq!(p.selected, 0);
        for _ in 0..20 {
            p.move_down();
        }
        assert_eq!(p.selected, p.match_count() - 1);
    }

    #[test]
    fn render_is_boxed_with_dir_suffix() {
        let mut p = sample();
        p.sync(Some("@"));
        let lines = p.render(60, &Theme::dark_ansi(), 8);
        assert!(lines.len() >= 3, "上下边框 + 至少一行: {lines:?}");
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts[0].starts_with('╭'));
        assert!(texts.iter().any(|t| t.contains("src/")), "目录渲染带尾缀: {texts:?}");
        for text in &texts {
            assert!(display_width(text) <= 60, "{text:?}");
        }
    }

    #[test]
    fn render_scrolls_to_keep_selection_visible() {
        let entries: Vec<FileEntry> = (0..30)
            .map(|i| FileEntry::new(format!("file{i:02}.txt"), false))
            .collect();
        let mut p = popup(entries);
        p.sync(Some("@file"));
        assert_eq!(p.match_count(), 30);
        // 窗口 8 行:选中第 9 项(下标 8)时 file00 滚出窗口
        for _ in 0..8 {
            p.move_down();
        }
        let lines = p.render(40, &Theme::dark_ansi(), 8);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.contains("file08")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("file00")), "{texts:?}");
    }

    #[test]
    fn matches_are_capped() {
        let entries: Vec<FileEntry> = (0..200)
            .map(|i| FileEntry::new(format!("file{i:03}.txt"), false))
            .collect();
        let mut p = popup(entries);
        p.sync(Some("@file"));
        assert_eq!(p.match_count(), MAX_MATCHES);
    }
}
