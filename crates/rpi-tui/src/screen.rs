//! 主缓冲差分渲染器(08 文档 `TuiMainScreen` 的 Rust 对应物)。
//!
//! 渲染模型(08 §3):**保留终端 scrollback** —— 定稿内容经 `commit_lines`
//! 追加进 scrollback 只打印一次;屏幕底部固定 `viewport_height` 行是活动
//! 视口(状态行/选择列表/输入行),每次更新走行差分,只重写变化的行。
//!
//! 不变量(正确性核心,见模块测试):
//! - 视口行数固定为 `viewport_height`(调用方不足补空行,超出截断);
//! - 光标始终停留在视口内,渲染器自己维护"视口占用最后 N 行"的事实;
//! - `viewport_height` ≤ 终端行数,且**构造前光标已被锚定到屏幕底部**
//!   (调用方负责:open 后先输出 rows-1 个换行滚到底);
//! - commit 行与视口行都按保存的终端宽度折行,终端不会自行回绕(回绕会
//!   打破行计数);
//! - 每次输出包在 CSI 2026 同步区间里,终端一次性上屏,不闪烁。
//!
//! 对 `Write` 泛型 → 测试可直接断言字节流,不需要真终端。

use std::io::Write;

use crate::ansi;
use crate::diff::{diff_rows, RowChange};

/// 视口高度上限(交互模式布局:状态行 + 选择列表 + 输入行)。
pub const DEFAULT_VIEWPORT_HEIGHT: usize = 10;

/// 渲染接口(08 文档 `TUI` 接口的主缓冲面)。
pub trait Tui {
    /// 把定稿行追加进 scrollback(只打印一次,永不再重绘;超宽行按终端
    /// 宽度折行,防止终端自行回绕打破行计数)。
    fn commit_lines(&mut self, lines: &[String]);
    /// 差分重绘视口;`cursor_col` 为光标在末行的显示列。
    /// 行数不足 `viewport_height` 补空行,超出截断。
    fn render_viewport(&mut self, lines: &[String], cursor_col: usize);
    /// 收尾:清掉视口,光标落回 scrollback 末尾(最终文档留在屏幕上)。
    fn finish(&mut self);
}

/// 主缓冲渲染器工厂(方针 §2 规则 1:调用方只认 `Tui` trait)。
pub fn create_main_screen_tui(
    out: Box<dyn Write + Send>,
    viewport_height: usize,
    width: usize,
) -> Box<dyn Tui> {
    Box::new(MainScreenTui::new(out, viewport_height, width))
}

pub struct MainScreenTui {
    out: Box<dyn Write + Send>,
    viewport_height: usize,
    /// 终端显示宽度(commit/视口行都按此折行,见模块不变量)
    width: usize,
    /// 上一帧视口内容(差分基准)
    viewport_prev: Vec<String>,
    /// 光标当前在视口内的行(0 基,从视口顶往下数)
    cursor_row: usize,
    /// 光标显示列(相对行首)
    cursor_col: usize,
    finished: bool,
}

impl MainScreenTui {
    pub fn new(out: Box<dyn Write + Send>, viewport_height: usize, width: usize) -> Self {
        let viewport_height = viewport_height.max(1);
        MainScreenTui {
            out,
            viewport_height,
            width: width.max(1),
            viewport_prev: vec![String::new(); viewport_height],
            cursor_row: viewport_height - 1,
            cursor_col: 0,
            finished: false,
        }
    }

    /// 一次性输出 helper:同步区间包住所有转义与文本,末尾 flush。
    fn write_synced(&mut self, payload: &str) {
        let _ = self.out.write_all(ansi::SYNC_START.as_bytes());
        let _ = self.out.write_all(payload.as_bytes());
        let _ = self.out.write_all(ansi::SYNC_END.as_bytes());
        let _ = self.out.flush();
    }

    /// 光标移到视口第 `row` 行行首并清到屏幕底(视口内容随后整体重写)。
    fn home_viewport_and_clear(&mut self, payload: &mut String) {
        if self.cursor_row > 0 {
            payload.push_str(&ansi::cursor_up(self.cursor_row));
        }
        payload.push_str(ansi::CR);
        payload.push_str(ansi::CLEAR_TO_SCREEN_END);
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// 把一行写进视口第 `row` 行:光标移到位、清行、写内容。
    fn write_viewport_row(&mut self, payload: &mut String, row: usize, line: &str, cursor_col: Option<usize>) {
        debug_assert!(row < self.viewport_height);
        if self.cursor_row < row {
            payload.push_str(&ansi::cursor_down(row - self.cursor_row));
        } else if self.cursor_row > row {
            payload.push_str(&ansi::cursor_up(self.cursor_row - row));
        }
        payload.push_str(ansi::CR);
        payload.push_str(ansi::CLEAR_TO_EOL);
        payload.push_str(line);
        self.cursor_row = row;
        match cursor_col {
            Some(col) => {
                self.cursor_col = col;
                // 光标定位:内容写入后光标在行尾;退回到目标列
                let line_width = crate::width::display_width(line);
                if line_width > col {
                    payload.push_str(&format!("\x1b[{}D", line_width - col));
                }
            }
            None => {
                self.cursor_col = crate::width::display_width(line);
            }
        }
    }
}

impl Tui for MainScreenTui {
    fn commit_lines(&mut self, lines: &[String]) {
        if self.finished {
            return;
        }
        // 折行在内部完成(调用方传逻辑行):终端不自行回绕,行计数才成立
        let wrapped: Vec<String> = lines
            .iter()
            .flat_map(|line| crate::width::wrap_to_width(line, self.width))
            .collect();
        if wrapped.is_empty() {
            return;
        }
        let mut payload = String::new();
        // 1. 光标移到视口顶,清掉视口(其内容已在调用方的模型里定稿)
        self.home_viewport_and_clear(&mut payload);
        // 2. 追加进 scrollback:每行后换行;超出终端行数时终端自然滚动,
        //    视口仍然重画在底部 —— 不变量由"视口高度 ≤ 终端行数"保证
        for line in wrapped {
            payload.push_str(&line);
            payload.push_str("\r\n");
        }
        // 3. 整体重画视口(行差分失效:基准行可能已被滚出屏幕)
        let prev = self.viewport_prev.clone();
        for (row, line) in prev.iter().enumerate() {
            self.write_viewport_row(&mut payload, row, line, None);
        }
        self.write_synced(&payload);
    }

    fn render_viewport(&mut self, lines: &[String], cursor_col: usize) {
        if self.finished {
            return;
        }
        let mut frame: Vec<String> = lines
            .iter()
            .take(self.viewport_height)
            .map(|line| crate::width::truncate_to_width(line, self.width).0)
            .collect();
        while frame.len() < self.viewport_height {
            frame.push(String::new());
        }
        let cursor_col = cursor_col.min(self.width.saturating_sub(1));
        let mut payload = String::new();
        let changes = diff_rows(&self.viewport_prev, &frame);
        for change in changes {
            match change {
                RowChange::Replace { row, line } => {
                    let col = if row == self.viewport_height - 1 { Some(cursor_col) } else { None };
                    self.write_viewport_row(&mut payload, row, &line, col);
                }
                RowChange::Keep => {}
            }
        }
        // 无变化也要保证光标列正确(输入行光标可能移动)
        if payload.is_empty() && self.cursor_col != cursor_col {
            let row = self.viewport_height - 1;
            let line = frame[row].clone();
            let mut payload = String::new();
            self.write_viewport_row(&mut payload, row, &line, Some(cursor_col));
            self.write_synced(&payload);
            self.viewport_prev = frame;
            return;
        }
        self.viewport_prev = frame;
        if !payload.is_empty() {
            self.write_synced(&payload);
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        let mut payload = String::new();
        self.home_viewport_and_clear(&mut payload);
        self.write_synced(&payload);
        self.finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn output(buffer: &SharedBuffer) -> String {
        String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn render_viewport_writes_full_frame_then_only_changes() {
        let buffer = SharedBuffer::default();
        {
            let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 2, 80);
            tui.render_viewport(&["status".into(), "> hi".into()], 4);
            tui.render_viewport(&["status".into(), "> hix".into()], 5);
            tui.finish();
        }
        let out = output(&buffer);
        // 第一帧:两行都写(清行序列在文本之前);第二帧:只重写变化的第 1 行
        assert!(out.contains("\x1b[Kstatus"));
        assert!(out.contains("\x1b[K> hi"));
        assert!(out.contains("\x1b[K> hix"));
        assert!(out.contains(ansi::SYNC_START));
        assert!(out.ends_with(ansi::SYNC_END));
        // 第二帧没有重复写 "status" 行(差分生效)
        assert_eq!(out.matches("\x1b[Kstatus").count(), 1);
    }

    #[test]
    fn commit_lines_appends_and_redraws_viewport() {
        let buffer = SharedBuffer::default();
        {
            let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 2, 80);
            tui.render_viewport(&["".into(), "> ".into()], 2);
            tui.commit_lines(&["hello".into(), "world".into()]);
            tui.finish();
        }
        let out = output(&buffer);
        // commit 的行按序出现,后随视口重画与收尾
        assert!(out.contains("hello\r\nworld\r\n"));
        // 收尾清掉视口
        assert!(out.contains(ansi::CLEAR_TO_SCREEN_END));
    }

    #[test]
    fn commit_after_commit_keeps_order() {
        let buffer = SharedBuffer::default();
        {
            let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 1, 80);
            tui.commit_lines(&["a".into()]);
            tui.commit_lines(&["b".into()]);
            tui.finish();
        }
        let out = output(&buffer);
        let a = out.find("a\r\n").unwrap();
        let b = out.find("b\r\n").unwrap();
        assert!(a < b);
    }

    #[test]
    fn viewport_lines_are_padded_to_fixed_height() {
        let buffer = SharedBuffer::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 3, 80);
        // 只给 1 行:其余补空行,内部基准应为 3 行
        tui.render_viewport(&["x".into()], 1);
        tui.render_viewport(&["x".into()], 1);
        // 第二帧无变化:不应再写 "x"(清行 + 文本)
        let out = output(&buffer);
        assert_eq!(out.matches("\x1b[Kx").count(), 1);
    }

    #[test]
    fn viewport_overflow_is_truncated() {
        let buffer = SharedBuffer::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 2, 80);
        tui.render_viewport(&["1".into(), "2".into(), "3".into()], 0);
        let out = output(&buffer);
        assert!(out.contains("\x1b[K2"));
        assert!(!out.contains("\x1b[K3")); // 第 3 行被截断
    }

    #[test]
    fn cursor_col_correction_when_row_unchanged() {
        let buffer = SharedBuffer::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 1, 80);
        tui.render_viewport(&["ab".into()], 2);
        tui.render_viewport(&["ab".into()], 1); // 只动光标
        let out = output(&buffer);
        // 第二帧:光标左移 1 列
        assert!(out.contains("\x1b[1D"));
    }

    #[test]
    fn ops_after_finish_are_noops() {
        let buffer = SharedBuffer::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 2, 80);
        tui.finish();
        let len = buffer.0.lock().unwrap().len();
        tui.commit_lines(&["x".into()]);
        tui.render_viewport(&["y".into()], 0);
        tui.finish();
        assert_eq!(buffer.0.lock().unwrap().len(), len);
    }

    #[test]
    fn empty_commit_is_noop() {
        let buffer = SharedBuffer::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 1, 80);
        tui.render_viewport(&["x".into()], 0);
        let len = buffer.0.lock().unwrap().len();
        tui.commit_lines(&[]);
        assert_eq!(buffer.0.lock().unwrap().len(), len);
    }

    #[test]
    fn factory_returns_trait_object() {
        let mut tui = create_main_screen_tui(Box::new(Vec::new()), DEFAULT_VIEWPORT_HEIGHT, 80);
        tui.render_viewport(&["ok".into()], 2);
        tui.finish();
    }

    #[test]
    fn commit_lines_wrap_overlong_lines() {
        // 超宽行按终端宽度折行(终端不自行回绕,行计数才成立)
        let buffer = SharedBuffer::default();
        {
            let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 1, 5);
            tui.commit_lines(&["abcdefghij".into()]);
            tui.finish();
        }
        let out = output(&buffer);
        assert!(out.contains("abcde\r\nfghij\r\n"));
    }

    #[test]
    fn render_viewport_truncates_overlong_rows() {
        let buffer = SharedBuffer::default();
        let mut tui = MainScreenTui::new(Box::new(buffer.clone()), 1, 5);
        tui.render_viewport(&["abcdefghijkl".into()], 100);
        let out = output(&buffer);
        assert!(out.contains("abcde"));
        assert!(!out.contains("fghij"), "视口行截断而非回绕");
    }
}
