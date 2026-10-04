//! 高亮单选列表(ExtensionUi select / 内部选择器的渲染基础)。
//! 状态(选项 + 选中下标)与渲染分离:状态可单测,渲染按主题出 `Line`。

/// 单选列表状态。
#[derive(Debug, Clone, Default)]
pub struct SelectList {
    pub options: Vec<String>,
    pub selected: usize,
}

impl SelectList {
    pub fn new(options: Vec<String>) -> Self {
        SelectList {
            options,
            selected: 0,
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.options.len() {
            self.selected += 1;
        }
    }

    pub fn selected_option(&self) -> Option<&String> {
        self.options.get(self.selected)
    }

    /// 渲染:选中行 `❯ ` 前缀 + accent 色,其余 muted;超宽截断。
    pub fn render(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        self.options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                let style = if i == self.selected {
                    Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.muted)
                };
                let marker = if i == self.selected { "❯ " } else { "  " };
                let line = Line::from(Span::styled(format!("{marker}{option}"), style));
                truncate_line(line, width)
            })
            .collect()
    }
}

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::text::truncate_line;
use crate::theme::Theme;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;

    #[test]
    fn highlight_and_navigation() {
        let mut list = SelectList::new(vec!["a".into(), "b".into(), "c".into()]);
        let lines = list.render(20, &Theme::dark_ansi());
        assert_eq!(lines.len(), 3);
        assert_eq!(line_text(&lines[0]), "❯ a");
        assert_eq!(line_text(&lines[1]), "  b");
        list.move_down();
        assert_eq!(list.selected_option().map(String::as_str), Some("b"));
        list.move_down();
        list.move_down(); // 到底不再下移
        assert_eq!(list.selected_option().map(String::as_str), Some("c"));
        list.move_up();
        list.move_up();
        list.move_up(); // 到顶不再上移
        assert_eq!(list.selected_option().map(String::as_str), Some("a"));
    }

    #[test]
    fn empty_list_is_empty_and_safe() {
        let mut list = SelectList::new(vec![]);
        assert!(list.render(10, &Theme::dark_ansi()).is_empty());
        list.move_down();
        assert_eq!(list.selected, 0);
    }

    #[test]
    fn overflow_is_truncated() {
        let list = SelectList::new(vec!["x".repeat(30)]);
        let lines = list.render(10, &Theme::dark_ansi());
        assert!(line_text(&lines[0]).chars().count() <= 10);
    }
}
