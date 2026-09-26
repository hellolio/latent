//! 视口行差分(08 文档"差分渲染:只重画变化行"的纯函数核心)。
//!
//! 输入上一帧与下一帧的行数组,输出逐行的变更操作。渲染器(`screen.rs`)
//! 据此只移动/重写变化的行。纯函数、无 I/O,直接单测。

/// 单行的变更操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowChange {
    /// 行内容不变,跳过。
    Keep,
    /// 行号(从 0)+ 新内容:需要重写。
    Replace { row: usize, line: String },
}

/// 计算两帧的逐行差分。行数变化时:公共前缀之外,短帧剩余行视为清空、
/// 长帧新增行视为重写(渲染器负责用空串/新内容落盘)。
pub fn diff_rows(old: &[String], new: &[String]) -> Vec<RowChange> {
    let rows = old.len().max(new.len());
    let mut changes = Vec::new();
    for row in 0..rows {
        let old_line = old.get(row);
        let new_line = new.get(row);
        if old_line != new_line {
            changes.push(RowChange::Replace {
                row,
                line: new_line.cloned().unwrap_or_default(),
            });
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn identical_frames_are_all_keep() {
        let frame = lines(&["a", "b", "c"]);
        assert!(diff_rows(&frame, &frame).is_empty());
    }

    #[test]
    fn only_changed_rows_are_replaced() {
        let old = lines(&["a", "b", "c"]);
        let new = lines(&["a", "B", "c"]);
        assert_eq!(
            diff_rows(&old, &new),
            vec![RowChange::Replace { row: 1, line: "B".into() }]
        );
    }

    #[test]
    fn growing_frame_replaces_beyond_old_rows() {
        let old = lines(&["a"]);
        let new = lines(&["a", "b", "c"]);
        assert_eq!(
            diff_rows(&old, &new),
            vec![
                RowChange::Replace { row: 1, line: "b".into() },
                RowChange::Replace { row: 2, line: "c".into() },
            ]
        );
    }

    #[test]
    fn shrinking_frame_clears_stale_rows() {
        let old = lines(&["a", "b", "c"]);
        let new = lines(&["a"]);
        assert_eq!(
            diff_rows(&old, &new),
            vec![RowChange::Replace { row: 1, line: String::new() }, RowChange::Replace { row: 2, line: String::new() }]
        );
    }

    #[test]
    fn empty_to_content() {
        let changes = diff_rows(&[], &lines(&["x"]));
        assert_eq!(changes, vec![RowChange::Replace { row: 0, line: "x".into() }]);
    }
}
