//! 终端应用壳(pi TuiMainScreen 的 ratatui 对应物)。
//!
//! 渲染模型:**Inline 视口**(`Viewport::Inline`)—— 定稿内容经 `commit_lines`
//! 插入到视口上方、滚入终端原生 scrollback(只打印一次);屏幕底部固定
//! `height` 行是活动视口(编辑器/状态栏/选择列表),每帧整体重绘。
//!
//! 视口高度可随内容增长(多行编辑器、选择列表):ratatui 的 Inline 视口
//! 不支持运行期改高,这里用「插入空行腾位 + 清屏区 + 重建 Terminal」实现
//! `set_viewport_height`;重绘全文(ctrl+o 展开切换)同用该路径。
//!
//! 不变量(继承 docs/08 踩坑记录):
//! - commit 行在内部按终端宽度折行,终端不自行回绕,行计数才成立;
//! - 构造前光标先锚定到屏幕底部(视口贴底);
//! - 终端恢复(raw mode / 光标)在 `finish` 与 `Drop` 双路径兜底。

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::execute;
use ratatui::crossterm::style::Print;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size, Clear, ClearType};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;
use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;

use crate::text::wrap_line;

/// 读取线程暂停协议:crossterm 的光标位置查询(Inline 视口构建/resize)需要
/// 从终端 fd 读 DSR 应答;按键读取线程若同时 read,会抢走应答字节导致查询
/// 超时。查询前置 PAUSE,读取线程在循环边界看到 PAUSE 后置 PARKED 并停读,
/// 查询方等到 PARKED 再发查询(crossterm fd 单读者原则)。
static READER_PAUSE: AtomicBool = AtomicBool::new(false);
static READER_PARKED: AtomicBool = AtomicBool::new(false);

/// 读取线程在每次 poll/read 循环边界调用:被暂停则挂起等待恢复。
pub fn reader_checkpoint() {
    if READER_PAUSE.load(Ordering::SeqCst) {
        READER_PARKED.store(true, Ordering::SeqCst);
        while READER_PAUSE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        READER_PARKED.store(false, Ordering::SeqCst);
    }
}

/// 暂停读取线程执行 `f`(光标查询等终端应答操作);限时等待停靠确认。
fn with_reader_paused<T>(f: impl FnOnce() -> T) -> T {
    READER_PAUSE.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_millis(300);
    while !READER_PARKED.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let out = f();
    READER_PAUSE.store(false, Ordering::SeqCst);
    out
}

/// Inline 视口终端应用。
///
/// backend 可替换:生产用 `CrosstermBackend<Stdout>`(默认类型参数),测试用
/// `TestBackend`(`TuiApp::with_test_backend`)在内存 buffer 上断言屏幕内容。
pub struct TuiApp<B: Backend = CrosstermBackend<Stdout>> {
    terminal: Terminal<B>,
    /// 当前视口行数
    height: u16,
    /// 最近一次已知的屏幕尺寸(重建视口时用)
    rows: u16,
    cols: u16,
    finished: bool,
    /// 屏幕尺寸查询(生产 = crossterm ioctl;TestBackend = 固定值)。
    query_size: Box<dyn Fn() -> io::Result<(u16, u16)>>,
    /// 是否恢复真实终端(Drop 兜底用;TestBackend 上跳过 raw mode/光标操作)。
    restores_terminal: bool,
}

impl<B: Backend> TuiApp<B> {
    /// 终端显示宽度(列数)。
    pub fn width(&self) -> usize {
        self.cols as usize
    }

    /// 当前视口高度。
    pub fn viewport_height(&self) -> u16 {
        self.height
    }

    /// 视口高度上限(屏幕行数 - 1,至少给转录留 1 行)。
    pub fn viewport_height_cap(&self) -> u16 {
        self.rows.saturating_sub(1).max(1)
    }

    /// 把定稿行追加进 scrollback(只打印一次;超宽行按终端宽度折行)。
    pub fn commit_lines(&mut self, lines: &[Line<'static>]) -> io::Result<()> {
        if self.finished || lines.is_empty() {
            return Ok(());
        }
        self.sync_size()?;
        let width = self.cols as usize;
        let wrapped: Vec<Line<'static>> = lines
            .iter()
            .flat_map(|line| wrap_line(line, width.max(1)))
            .collect();
        if wrapped.is_empty() {
            return Ok(());
        }
        let height = wrapped.len() as u16;
        self.terminal
            .insert_before(height, |buf| {
                let area = buf.area;
                for (i, line) in wrapped.iter().enumerate() {
                    buf.set_line(0, i as u16, line, area.width);
                }
            })
            .map_err(|e| io::Error::other(e.to_string()))
    }

    /// 重绘底部视口;`lines` 不足 `height` 补空行,超出截断。
    /// `cursor` 为相对视口左上角的列/行(None = 本帧隐藏光标)。
    pub fn draw_viewport(
        &mut self,
        lines: &[Line<'static>],
        cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.sync_size()?;
        let height = self.height;
        let rows: Vec<Line<'static>> = {
            let mut rows: Vec<Line<'static>> = lines
                .iter()
                .take(height as usize)
                .map(|line| crate::text::truncate_line(line.clone(), self.cols as usize))
                .collect();
            while rows.len() < height as usize {
                rows.push(Line::raw(""));
            }
            rows
        };
        self.terminal
            .draw(|frame| {
                let area = frame.area();
                for (i, line) in rows.iter().enumerate() {
                    let rect = Rect::new(area.x, area.y + i as u16, area.width, 1);
                    line.clone().render(rect, frame.buffer_mut());
                }
                if let Some((x, y)) = cursor {
                    let x = area.x + x.min(area.width.saturating_sub(1));
                    let y = area.y + y.min(height.saturating_sub(1));
                    frame.set_cursor_position(ratatui::layout::Position::new(x, y));
                }
            })
            .map(|_| ())
            .map_err(|e| io::Error::other(e.to_string()))
    }

    /// 终端尺寸同步(查询函数无 tty 时由具体实现决定;尺寸变化时的 ratatui
    /// resize 含光标查询,须暂停读取线程)。
    fn sync_size(&mut self) -> io::Result<()> {
        let (cols, rows) = (self.query_size)()?;
        if cols != self.cols || rows != self.rows {
            let (c, r) = (cols, rows);
            with_reader_paused(|| {
                self.terminal
                    .resize(Rect::new(0, 0, c, r))
                    .map_err(|e| io::Error::other(e.to_string()))
            })?;
            self.cols = cols;
            self.rows = rows;
        }
        Ok(())
    }
}

impl TuiApp<CrosstermBackend<Stdout>> {
    /// 打开终端:raw mode + 光标锚定到底部 + Inline 视口。
    pub fn open(viewport_height: u16) -> io::Result<Self> {
        let dbg = std::env::var_os("RPI_TUI_DEBUG").is_some();
        let mark = |stage: &str| {
            if dbg {
                eprintln!("[tui] {stage}");
            }
        };
        mark("raw-mode enter");
        enable_raw_mode()?;
        mark("size query");
        let mut out = io::stdout();
        let (cols, rows) = size().unwrap_or((80, 24));
        // 锚定不变量:视口占据屏幕底部,构造渲染器前先把光标滚到底
        // (shell 提示符后启动时光标可能在屏幕中部,不锚定则首帧错位)
        if rows > 1 {
            mark("anchor newlines");
            execute!(out, Print("\r\n".repeat((rows - 1) as usize)))?;
            out.flush()?;
        }
        mark("terminal construct (cursor query)");
        let terminal = Terminal::with_options(
            CrosstermBackend::new(out),
            TerminalOptions {
                viewport: Viewport::Inline(viewport_height.max(1)),
            },
        )
        .map_err(|e| io::Error::other(e.to_string()))?;
        mark("terminal ok");
        Ok(TuiApp {
            terminal,
            height: viewport_height.max(1),
            rows,
            cols,
            finished: false,
            query_size: Box::new(size),
            restores_terminal: true,
        })
    }

    /// 调整视口高度(编辑器增长、选择列表打开等)。
    pub fn set_viewport_height(&mut self, height: u16) -> io::Result<()> {
        let height = height.max(1);
        if height == self.height || self.finished {
            return Ok(());
        }
        self.sync_size()?;
        let old = self.height;
        if height > old {
            // 增高:先插入空行腾出空间(新视口顶部不覆盖既有转录)
            let extra = height - old;
            self.terminal
                .insert_before(extra, |_buf| {})
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        with_reader_paused(|| self.rebuild_terminal(height))
    }

    /// 全文重绘(ctrl+o 展开/收起):清屏 → 重建视口 → 重打全文 → 重绘视口。
    pub fn redraw_full(
        &mut self,
        transcript: &[Line<'static>],
        viewport: &[Line<'static>],
        cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.sync_size()?;
        let height = self.height;
        {
            let mut out = io::stdout();
            execute!(out, MoveTo(0, 0), Clear(ClearType::All))?;
            out.flush()?;
        }
        with_reader_paused(|| self.rebuild_terminal(height))?;
        self.commit_lines(transcript)?;
        self.draw_viewport(viewport, cursor)
    }

    /// 清掉当前视口区并按新高度重建 Terminal(光标锚定到新视口顶行)。
    fn rebuild_terminal(&mut self, height: u16) -> io::Result<()> {
        {
            let mut out = io::stdout();
            let top = self.rows.saturating_sub(self.height);
            execute!(out, MoveTo(0, top), Clear(ClearType::FromCursorDown))?;
            execute!(out, MoveTo(0, self.rows.saturating_sub(height)))?;
            out.flush()?;
        }
        self.terminal = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )
        .map_err(|e| io::Error::other(e.to_string()))?;
        self.height = height;
        Ok(())
    }

    /// 收尾:清掉视口,恢复终端(最终文档留在屏幕/scrollback)。
    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let mut out = io::stdout();
        let top = self.rows.saturating_sub(self.height);
        execute!(out, MoveTo(0, top), Clear(ClearType::FromCursorDown))?;
        let _ = self.terminal.show_cursor();
        disable_raw_mode()?;
        execute!(out, Print("\r\n"))?;
        out.flush()
    }
}

impl<B: Backend> Drop for TuiApp<B> {
    fn drop(&mut self) {
        // panic/提前返回路径兜底恢复终端(finish 幂等,Drop 只在未收尾时干活)
        if !self.finished {
            if self.restores_terminal {
                let _ = disable_raw_mode();
                let mut out = io::stdout();
                let _ = execute!(out, MoveTo(0, self.rows.saturating_sub(self.height)));
                let _ = execute!(out, Clear(ClearType::FromCursorDown));
                let _ = out.flush();
            }
            self.finished = true;
        }
    }
}

#[cfg(test)]
mod tests {
    //! L2 屏幕级测试:TestBackend 上断言 commit_lines/draw_viewport 的最终
    //! 屏幕效果(折行、截断、光标),不需要真终端。

    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    use ratatui::text::Line;

    use super::TuiApp;

    /// 构造绑定 TestBackend 的 TuiApp(跳过 raw mode/光标锚定等真实终端操作)。
    fn test_app(cols: u16, rows: u16, viewport_height: u16) -> TuiApp<TestBackend> {
        let backend = TestBackend::new(cols, rows);
        let terminal = ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(viewport_height.max(1)),
            },
        )
        .unwrap();
        TuiApp {
            terminal,
            height: viewport_height.max(1),
            rows,
            cols,
            finished: false,
            query_size: Box::new(move || Ok((cols, rows))),
            restores_terminal: false,
        }
    }

    /// 把 TestBackend 当前 buffer 渲染成等宽文本行(便于断言)。
    fn screen_lines(app: &TuiApp<TestBackend>) -> Vec<String> {
        let buffer = app.terminal.backend().buffer();
        let width = buffer.area.width as usize;
        buffer
            .content
            .chunks(width)
            .map(|cells| {
                let mut line = String::new();
                let mut skip = 0u16;
                for cell in cells {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    skip = cell.cell_width().saturating_sub(1);
                    line.push_str(cell.symbol());
                }
                line
            })
            .collect()
    }

    #[test]
    fn committed_lines_land_above_viewport() {
        let mut app = test_app(40, 10, 3);
        app.commit_lines(&[Line::raw("第一行定稿"), Line::raw("第二行定稿")])
            .unwrap();
        app.draw_viewport(&[Line::raw("❯ 编辑器")], Some((4, 0)))
            .unwrap();
        // TestBackend 未做真实终端的光标锚定,视口位置不固定;
        // 断言相对结构:commit 行按序落在视口上方。
        let lines = screen_lines(&app);
        let find = |needle: &str| {
            lines
                .iter()
                .position(|line| line.trim_end().starts_with(needle))
                .unwrap_or_else(|| panic!("{needle} 应上屏: {lines:#?}"))
        };
        let (first, second, viewport) = (find("第一行定稿"), find("第二行定稿"), find("❯ 编辑器"));
        assert!(first < second && second < viewport, "顺序: {lines:#?}");
        // 视口共 3 行:首行是内容,其余为补空
        for line in &lines[viewport + 1..viewport + 3] {
            assert!(line.trim().is_empty(), "视口其余行应为空: {lines:#?}");
        }
        // 光标在视口首行、列 4
        assert!(app.terminal.backend().cursor_visible());
        assert_eq!(
            app.terminal.backend().cursor_position(),
            ratatui::layout::Position { x: 4, y: viewport as u16 }
        );
    }

    #[test]
    fn commit_wraps_lines_to_terminal_width() {
        let mut app = test_app(10, 8, 2);
        // 20 个半角字符在 10 列宽终端上折成两行
        app.commit_lines(&[Line::raw("abcdefghij0123456789")]).unwrap();
        app.draw_viewport(&[Line::raw("")], None).unwrap();
        let lines = screen_lines(&app);
        assert_eq!(lines[0], "abcdefghij");
        assert_eq!(lines[1], "0123456789");
    }

    #[test]
    fn draw_viewport_truncates_overlong_lines() {
        let mut app = test_app(6, 6, 2);
        app.draw_viewport(&[Line::raw("很长很长很长的一行")], None)
            .unwrap();
        let lines = screen_lines(&app);
        // 视口首行被截断到 6 列(3 个 CJK 字符)
        assert_eq!(lines[0], "很长很", "全屏: {:#?}", screen_lines(&app));
    }

    #[test]
    fn commit_lines_land_adjacent_above_viewport() {
        // render_tick 顺序重构依赖的机制:flush 内容必须紧贴视口顶行落位
        // (无论视口上方此前的行是内容还是收缩留下的空行)。
        // TestBackend 无真实光标锚定,视口位置不固定,断言相对结构:
        // 提交的多行按序相邻落位、不留空行,且整体在视口上方。
        let mut app = test_app(10, 12, 2);
        app.draw_viewport(&[Line::raw(""), Line::raw("")], None)
            .unwrap();
        app.commit_lines(&[Line::raw("AAAA"), Line::raw("BBBB"), Line::raw("CCCC")])
            .unwrap();
        app.draw_viewport(&[Line::raw("▌")], Some((2, 0)))
            .unwrap();
        let lines = screen_lines(&app);
        let find = |needle: &str| {
            lines
                .iter()
                .position(|line| line.trim_end().starts_with(needle))
                .unwrap_or_else(|| panic!("{needle} 应上屏: {lines:#?}"))
        };
        let (aaaa, bbbb, cccc, viewport) =
            (find("AAAA"), find("BBBB"), find("CCCC"), find("▌"));
        assert!(
            aaaa + 1 == bbbb && bbbb + 1 == cccc && cccc < viewport,
            "内容应相邻且整体在视口上方: {lines:#?}"
        );
    }
}
