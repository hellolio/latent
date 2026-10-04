//! 全帧差分终端屏幕(pi TuiMainScreen 的 Rust 对应物)。
//!
//! 渲染模型:**全帧行级差分**——交互层维护「已定稿行」(`committed`,只
//! 追加, ANSI 序列化结果缓存)与「活动尾部」(预览/状态行/编辑器/footer,
//! 每帧重建);屏幕层把两段拼成全帧,与上一帧逐行(ANSI 字符串)差分,
//! 只重绘变化区间。追加行越过屏幕底部时打印 `\r\n` 让终端自然滚动,
//! 顶部行就此进入原生 scrollback,此后永不再碰。
//!
//! 不变量:
//! - committed 行按当前终端宽度折行,帧间视为不可变(差分扫描跳过该
//!   前缀);变化落在已滚入 scrollback 的行上时只能全量重绘兜底;
//! - `viewport_top` 追踪屏幕顶行对应的逻辑行号(锚定期为负:内容贴底,
//!   屏幕上方留白),随 `\r\n` 滚动事件单调递增;
//! - 自动换行(DECAWM)在会话期间关闭,行宽由内部折行保证,写入绝不
//!   触发终端回绕;
//! - 终端恢复(raw mode / 光标)在 `finish` 与 `Drop` 双路径兜底。

use std::io::{self, Stdout, Write};
use std::rc::Rc;
use std::cell::RefCell;

use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use crate::text::wrap_line;
use crate::UiLine;

const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const CLEAR_LINE: &str = "\x1b[2K";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const RESET_SGR: &str = "\x1b[0m";
const AUTO_WRAP_OFF: &str = "\x1b[?7l";
const AUTO_WRAP_ON: &str = "\x1b[?7h";
/// bracketed paste(\x1b[?2004h/l):开启后终端把粘贴内容包在 `\x1b[200~…`
/// `\x1b[201~` 里整块送达,粘贴中的换行不再伪装成 Enter 击键——多行粘贴
/// 由此不会逐行触发提交。crossterm 解析为 `Event::Paste` → `Key::Paste`。
const BRACKETED_PASTE_ON: &str = "\x1b[?2004h";
const BRACKETED_PASTE_OFF: &str = "\x1b[?2004l";
/// kitty keyboard protocol:push disambiguate + report event types +
/// report alternate keys(flags 1|2|4 = 7,与上游 pi 一致)。disambiguate
/// 让修饰 Enter/Tab 等以 CSI 13;…u 上报(crossterm 解析出 SHIFT/ALT);
/// 事件类型与 alternate 子字段 crossterm 0.29 均可解析,非 Press 事件由
/// key.rs 的 from_event 过滤。不支持该协议的终端忽略未知 CSI,行为不变
/// (由 key.rs 的本地修饰键兜底接管)。字节序与 crossterm 同名命令一致
/// (有测试锚定),pop 在收尾/Drop 恢复。不启用 flag 8(全键上报):
/// 普通文本键也会转义,破坏 CJK 输入。
const KEYBOARD_PUSH: &str = "\x1b[>7u";
const KEYBOARD_POP: &str = "\x1b[<1u";

fn move_to(col: u16, row: u16) -> String {
    format!("\x1b[{};{}H", row + 1, col + 1)
}

/// 全帧差分终端应用。
///
/// 输出可替换:生产绑定 stdout(`TuiApp::open`),测试用 `with_sink`
/// 在内存缓冲上回放字节流做屏幕级断言。
pub struct TuiApp<W: Write = Stdout> {
    out: W,
    cols: u16,
    rows: u16,
    /// 已定稿行(折行后 ANSI 序列化缓存;只追加,帧间不可变)
    committed: Vec<String>,
    /// 上一帧的尾部活动行
    prev_tail: Vec<String>,
    /// 上一帧时 committed 的长度(差分稳定前缀边界)
    prev_committed_len: usize,
    /// 屏幕顶行对应的逻辑行号(锚定期为负)
    viewport_top: isize,
    finished: bool,
    /// 尺寸变化待处理:调用方须走 `redraw_all` 重折行
    needs_reshape: bool,
    query_size: Box<dyn Fn() -> io::Result<(u16, u16)>>,
    /// 是否恢复真实终端(Drop 兜底用;内存 sink 上跳过 raw mode)。
    restores_terminal: bool,
}

impl TuiApp<Stdout> {
    /// 打开终端:raw mode + 关自动换行 + 光标锚定到屏幕底部。
    pub fn open() -> io::Result<Self> {
        enable_raw_mode()?;
        let (cols, rows) = size().unwrap_or((80, 24));
        let mut app = TuiApp {
            out: io::stdout(),
            cols,
            rows,
            committed: Vec::new(),
            prev_tail: Vec::new(),
            prev_committed_len: 0,
            viewport_top: 1 - rows as isize,
            finished: false,
            needs_reshape: false,
            query_size: Box::new(size),
            restores_terminal: true,
        };
        // 锚定不变量:光标先滚到屏幕底部,内容从此贴底自然滚动(在
        // shell 提示符后启动时,首帧才不会错位)
        let mut boot = String::from(AUTO_WRAP_OFF);
        boot.push_str(KEYBOARD_PUSH);
        boot.push_str(BRACKETED_PASTE_ON);
        boot.push_str(HIDE_CURSOR);
        boot.push_str(&"\r\n".repeat(rows.saturating_sub(1) as usize));
        app.write_raw(boot.as_bytes())?;
        Ok(app)
    }
}

/// 测试共享尺寸槽(`with_sink` 的 query_size 数据源,可运行期改尺寸)。
#[derive(Clone, Default)]
pub struct SharedSize(Rc<RefCell<(u16, u16)>>);

impl SharedSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self(Rc::new(RefCell::new((cols, rows))))
    }

    pub fn set(&self, cols: u16, rows: u16) {
        *self.0.borrow_mut() = (cols, rows);
    }
}

/// 测试共享输出汇(收集全部写入字节,供屏幕模拟器回放)。
#[derive(Clone, Default)]
pub struct SharedBuf(Rc<RefCell<Vec<u8>>>);

impl SharedBuf {
    pub fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.borrow_mut())).unwrap_or_default()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> TuiApp<W> {
    /// 测试构造:内存输出汇 + 固定/可变尺寸,跳过 raw mode。
    pub fn with_sink(sink: W, size: SharedSize) -> Self {
        let (cols, rows) = *size.0.borrow();
        TuiApp {
            out: sink,
            cols,
            rows,
            committed: Vec::new(),
            prev_tail: Vec::new(),
            prev_committed_len: 0,
            viewport_top: 1 - rows as isize,
            finished: false,
            needs_reshape: false,
            query_size: Box::new(move || Ok(*size.0.borrow())),
            restores_terminal: false,
        }
    }

    /// 终端显示宽度(列数)。
    pub fn width(&self) -> usize {
        self.cols as usize
    }

    /// 屏幕总行数(尾部帧预算的来源)。
    pub fn screen_rows(&self) -> u16 {
        self.rows
    }

    /// 取走并清空尺寸变化标记(宽度/高度变化都要求调用方以新尺寸
    /// `redraw_all` 重折行重绘)。
    pub fn take_needs_reshape(&mut self) -> bool {
        std::mem::take(&mut self.needs_reshape)
    }

    /// 追加已定稿行(折行 + ANSI 序列化进缓存;不发 I/O,由下一次
    /// `render` 的差分统一输出)。
    pub fn append_committed(&mut self, lines: &[UiLine]) {
        if self.finished {
            return;
        }
        let width = self.cols.max(1) as usize;
        for line in lines {
            for wrapped in wrap_line(line, width) {
                self.committed.push(serialize_line(&wrapped));
            }
        }
    }

    /// 渲染一帧:`tail` 为活动尾部行(预览/状态/编辑器/footer),与
    /// `committed` 拼成全帧做行级差分后输出。`cursor` 为相对尾部首行
    /// 的 (列, 行);None = 隐藏光标。
    pub fn render(&mut self, tail: &[UiLine], cursor: Option<(u16, u16)>) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.sync_size();
        if self.needs_reshape {
            // 尺寸已变:本帧跳过,等调用方 redraw_all 后再画
            return Ok(());
        }
        let width = self.cols.max(1) as usize;
        let tail_s: Vec<String> = tail
            .iter()
            .flat_map(|line| wrap_line(line, width))
            .map(|line| serialize_line(&line))
            .collect();
        let new_len = self.committed.len() + tail_s.len();
        let old_len = self.prev_committed_len + self.prev_tail.len();

        // 差分:committed 前缀帧间不可变,扫描从上帧 committed 边界开始
        let scan_from = self.prev_committed_len.min(new_len);
        let mut first = usize::MAX;
        let mut last = 0usize;
        for i in scan_from..new_len.max(old_len) {
            if self.prev_frame_line(i) != self.frame_line(i, &tail_s) {
                first = first.min(i);
                last = last.max(i);
            }
        }

        let mut out = String::from(SYNC_BEGIN);
        // 帧写入期间隐藏光标:不支持同步输出(DECSET 2026)的终端会随行写入
        // 即时渲染,diff 重写会让光标在输出区跳动闪烁;帧尾再按 `cursor`
        // 位置恢复显示(隐藏比显示是后写的,帧间无残留)
        out.push_str(HIDE_CURSOR);
        if first != usize::MAX {
            if (first as isize) < self.viewport_top {
                // 变化落在已滚入 scrollback 的区域,无法原地修改:全量重绘
                self.reset_screen(&mut out);
                for i in 0..new_len {
                    self.write_row(i, &tail_s, &mut out);
                }
            } else {
                for i in first..=last {
                    if i < new_len {
                        self.write_row(i, &tail_s, &mut out);
                    }
                }
            }
        }
        // 帧收缩:清理新帧尾部之后残留的旧行(已滚出屏幕的行无需处理)
        for i in new_len..old_len {
            if let Some(sr) = self.screen_row_of(i) {
                out.push_str(&move_to(0, sr));
                out.push_str(CLEAR_LINE);
            }
        }
        out.push_str(SYNC_END);
        if let Some((col, rel_row)) = cursor {
            let logical = new_len as isize - tail_s.len() as isize + rel_row as isize;
            let sr = logical - self.viewport_top;
            if sr >= 0 && sr < self.rows as isize {
                out.push_str(&move_to(col, sr as u16));
                out.push_str(SHOW_CURSOR);
            }
            // sr 越界时保持帧首的隐藏态(无需重复 HIDE)
        } // None: 帧首已隐藏
        self.out.write_all(out.as_bytes())?;
        self.out.flush()?;
        self.prev_tail = tail_s;
        self.prev_committed_len = self.committed.len();
        Ok(())
    }

    /// 全量重绘:清可视屏幕 → 重新锚定到底部 → 重打整份定稿文档
    /// (ctrl+o / 主题切换 / /new / 尺寸变化路径)。活动尾部由紧随其后的
    /// `render` 追加。
    pub fn redraw_all(&mut self, lines: &[UiLine]) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.sync_size();
        self.committed.clear();
        self.append_committed(lines);
        let mut out = String::from(SYNC_BEGIN);
        out.push_str(HIDE_CURSOR); // 全量重绘期间隐藏光标(同 render 帧首隐藏)
        self.reset_screen(&mut out);
        for i in 0..self.committed.len() {
            self.write_row(i, &[], &mut out);
        }
        out.push_str(SYNC_END);
        out.push_str(HIDE_CURSOR);
        self.out.write_all(out.as_bytes())?;
        self.out.flush()?;
        self.prev_tail.clear();
        self.prev_committed_len = self.committed.len();
        self.needs_reshape = false;
        Ok(())
    }

    /// 挂起 TUI:恢复终端常规态,把整个屏幕让给外部子进程(如 $EDITOR)。
    /// 期间调用方不得渲染,且应暂停自己的输入读取;子进程退出后必须调用
    /// `resume` 恢复(全量重绘)。
    pub fn suspend(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        let mut out = String::from(AUTO_WRAP_ON);
        out.push_str(SHOW_CURSOR);
        // 清可视屏幕:外部编辑器从干净屏幕开始(scrollback 不受影响)
        out.push_str("\x1b[2J");
        out.push_str(&move_to(0, 0));
        if self.restores_terminal {
            // 内存 sink(测试)不写,避免污染回放字节流
            out.push_str(BRACKETED_PASTE_OFF);
            out.push_str(KEYBOARD_POP);
        }
        self.write_raw(out.as_bytes())?;
        disable_raw_mode()?;
        Ok(())
    }

    /// 从 `suspend` 恢复:重回 raw mode + 终端模式序列,按当前定稿文档
    /// 全量重绘(编辑期间窗口尺寸变化经 sync_size 感知,走重折行)。
    pub fn resume(&mut self, lines: &[UiLine]) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        enable_raw_mode()?;
        let mut out = String::from(AUTO_WRAP_OFF);
        out.push_str(HIDE_CURSOR);
        if self.restores_terminal {
            out.push_str(KEYBOARD_PUSH);
            out.push_str(BRACKETED_PASTE_ON);
        }
        self.write_raw(out.as_bytes())?;
        self.redraw_all(lines)
    }

    /// 收尾:清掉活动尾部区,光标落回最后一条定稿行末尾,恢复终端
    /// (定稿文档留在屏幕/scrollback)。
    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let rows = self.rows as isize;
        let frame_len = self.prev_committed_len + self.prev_tail.len();
        let mut out = String::new();
        if frame_len > 0 {
            let committed_end_sr =
                ((self.prev_committed_len as isize - 1) - self.viewport_top).clamp(0, rows - 1);
            if committed_end_sr < rows - 1 {
                out.push_str(&move_to(0, (committed_end_sr + 1) as u16));
                out.push_str("\x1b[0J");
            }
            out.push_str(&move_to(0, committed_end_sr as u16));
            out.push_str("\r\n");
        }
        out.push_str(AUTO_WRAP_ON);
        out.push_str(BRACKETED_PASTE_OFF);
        out.push_str(SHOW_CURSOR);
        if self.restores_terminal {
            // 内存 sink(测试)不写,避免污染回放字节流
            out.push_str(KEYBOARD_POP);
        }
        self.out.write_all(out.as_bytes())?;
        self.out.flush()?;
        disable_raw_mode()?;
        Ok(())
    }

    fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.out.flush()
    }

    /// 每帧查询终端尺寸;任何尺寸变化都置 `needs_reshape`(调用方走
    /// `redraw_all`,内容按新宽度重新折行)。
    fn sync_size(&mut self) {
        if let Ok((cols, rows)) = (self.query_size)() {
            if cols != self.cols || rows != self.rows {
                self.cols = cols;
                self.rows = rows;
                self.needs_reshape = true;
            }
        }
    }

    /// 清可视屏幕(不动 scrollback)并重新锚定:内容重新贴底。
    fn reset_screen(&mut self, out: &mut String) {
        out.push_str("\x1b[2J");
        out.push_str(&move_to(0, 0));
        out.push_str(&"\r\n".repeat(self.rows.saturating_sub(1) as usize));
        self.viewport_top = 1 - self.rows as isize;
    }

    /// 逻辑行 i 的屏幕行号(不在可视区内则 None)。
    fn screen_row_of(&self, i: usize) -> Option<u16> {
        let sr = i as isize - self.viewport_top;
        (sr >= 0 && sr < self.rows as isize).then_some(sr as u16)
    }

    /// 新帧第 i 行的文本(committed 前缀或尾部)。
    fn frame_line<'a>(&'a self, i: usize, tail_s: &'a [String]) -> Option<&'a str> {
        if i < self.committed.len() {
            Some(&self.committed[i])
        } else {
            tail_s.get(i - self.committed.len()).map(String::as_str)
        }
    }

    /// 上一帧第 i 行的文本(超出上帧长度返回 None)。
    fn prev_frame_line(&self, i: usize) -> Option<&str> {
        if i < self.prev_committed_len {
            // committed 只追加,前缀即上帧内容
            Some(&self.committed[i])
        } else {
            self.prev_tail
                .get(i - self.prev_committed_len)
                .map(String::as_str)
        }
    }

    /// 把逻辑行 i 写到屏幕(必要时以 `\r\n` 滚动;每帧内按行序递增写入,
    /// 滚动推入 scrollback 的行要么与本帧无关、要么已在本帧重写过)。
    fn write_row(&mut self, i: usize, tail_s: &[String], out: &mut String) {
        let rows = self.rows as usize;
        let sr = (i as isize - self.viewport_top) as usize;
        if sr >= rows {
            // 越过屏幕底:从底行起让终端自然滚动
            let scrolls = sr - (rows - 1);
            out.push_str(&move_to(0, rows as u16 - 1));
            for _ in 0..scrolls {
                out.push_str("\r\n");
            }
            self.viewport_top += scrolls as isize;
        } else {
            out.push_str(&move_to(0, sr as u16));
        }
        out.push_str(CLEAR_LINE);
        let text = self.frame_line(i, tail_s).unwrap_or("");
        out.push_str(text);
    }
}

impl<W: Write> Drop for TuiApp<W> {
    fn drop(&mut self) {
        // panic/提前返回路径兜底恢复终端(finish 幂等,Drop 只在未收尾时干活)
        if !self.finished {
            self.finished = true;
            if self.restores_terminal {
                let _ = disable_raw_mode();
                let _ = self.out.write_all(AUTO_WRAP_ON.as_bytes());
                let _ = self.out.write_all(BRACKETED_PASTE_OFF.as_bytes());
                let _ = self.out.write_all(SHOW_CURSOR.as_bytes());
                let _ = self.out.write_all(KEYBOARD_POP.as_bytes());
                let _ = self.out.flush();
            }
        }
    }
}

/// `Line` → ANSI 字符串:遍历 span 输出 SGR(整行样式 patch 进每个 span),
/// 行尾带样式的补 reset,保证行边界不泄漏样式、两帧字符串可比较。
/// 宽字符由终端自行推进占位,序列化只关心文本与样式。
fn serialize_line(line: &Line<'_>) -> String {
    let mut out = String::new();
    let mut current = Style::new();
    for span in &line.spans {
        let style = line.style.patch(span.style);
        if style != current {
            out.push_str(&sgr_sequence(style));
            current = style;
        }
        out.push_str(span.content.as_ref());
    }
    if current != Style::new() {
        out.push_str(RESET_SGR);
    }
    out
}

/// 完整 SGR 序列(reset 起底,再叠加前景/背景/修饰)。
fn sgr_sequence(style: Style) -> String {
    let mut codes: Vec<String> = vec!["0".into()];
    if let Some(fg) = style.fg {
        push_color(&mut codes, fg, true);
    }
    if let Some(bg) = style.bg {
        push_color(&mut codes, bg, false);
    }
    const SGR_ON: [(Modifier, &str); 9] = [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::SLOW_BLINK, "5"),
        (Modifier::RAPID_BLINK, "6"),
        (Modifier::REVERSED, "7"),
        (Modifier::HIDDEN, "8"),
        (Modifier::CROSSED_OUT, "9"),
    ];
    for (bit, code) in SGR_ON {
        if style.add_modifier.contains(bit) {
            codes.push(code.into());
        }
    }
    format!("\x1b[{}m", codes.join(";"))
}

fn push_color(codes: &mut Vec<String>, color: Color, fg: bool) {
    let base = if fg { 30 } else { 40 };
    let bright = if fg { 90 } else { 100 };
    let extended = if fg { "38" } else { "48" };
    let default = if fg { "39" } else { "49" };
    match color {
        Color::Reset => codes.push(default.into()),
        Color::Black => codes.push(base.to_string()),
        Color::Red => codes.push((base + 1).to_string()),
        Color::Green => codes.push((base + 2).to_string()),
        Color::Yellow => codes.push((base + 3).to_string()),
        Color::Blue => codes.push((base + 4).to_string()),
        Color::Magenta => codes.push((base + 5).to_string()),
        Color::Cyan => codes.push((base + 6).to_string()),
        Color::Gray => codes.push((base + 7).to_string()),
        Color::DarkGray => codes.push(bright.to_string()),
        Color::LightRed => codes.push((bright + 1).to_string()),
        Color::LightGreen => codes.push((bright + 2).to_string()),
        Color::LightYellow => codes.push((bright + 3).to_string()),
        Color::LightBlue => codes.push((bright + 4).to_string()),
        Color::LightMagenta => codes.push((bright + 5).to_string()),
        Color::LightCyan => codes.push((bright + 6).to_string()),
        Color::White => codes.push((bright + 7).to_string()),
        Color::Rgb(r, g, b) => {
            codes.push(extended.into());
            codes.push("2".into());
            codes.push(r.to_string());
            codes.push(g.to_string());
            codes.push(b.to_string());
        }
        Color::Indexed(i) => {
            codes.push(extended.into());
            codes.push("5".into());
            codes.push(i.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    //! L2 屏幕级测试:内存输出汇 + 最小 ANSI 屏幕模拟器回放字节流,
    //! 断言差分输出的最终屏幕效果(贴底滚动、区间重绘、残行清理、
    //! CJK 折行、光标定位),不需要真终端。

    use super::*;
    use crate::width::display_width;
    use ratatui::text::Span;

    /// 最小 ANSI 屏幕模拟器:处理本模块会发出的全部控制序列
    /// (MoveTo / 2K / 0J / 2J / SGR / 同步与模式开关 / \r\n)。
    struct ScreenSim {
        cols: usize,
        grid: Vec<Vec<char>>,
        cursor: (usize, usize),
    }

    impl ScreenSim {
        fn new(cols: usize, rows: usize) -> Self {
            ScreenSim {
                cols,
                grid: vec![vec![' '; cols]; rows],
                cursor: (0, 0),
            }
        }

        fn feed(&mut self, stream: &str) {
            let mut chars = stream.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\r' => self.cursor.1 = 0,
                    '\n' => self.newline(),
                    '\x1b' => self.escape(&mut chars),
                    c => {
                        let (row, col) = self.cursor;
                        if row < self.grid.len() && col < self.cols {
                            self.grid[row][col] = c;
                        }
                        self.cursor.1 += display_width(c.to_string().as_str());
                    }
                }
            }
        }

        fn newline(&mut self) {
            self.cursor.0 += 1;
            if self.cursor.0 >= self.grid.len() {
                self.grid.remove(0);
                self.grid.push(vec![' '; self.cols]);
                self.cursor.0 = self.grid.len() - 1;
            }
        }

        fn escape(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
            let next = chars.next();
            if next != Some('[') {
                return;
            }
            let mut params = String::new();
            let mut private = false;
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() || c == ';' {
                    params.push(c);
                    chars.next();
                } else {
                    if c == '?' {
                        private = true;
                        chars.next();
                        continue;
                    }
                    break;
                }
            }
            let final_byte = chars.next();
            let nums: Vec<usize> = params
                .split(';')
                .map(|p| p.parse::<usize>().unwrap_or(0))
                .collect();
            match (private, final_byte) {
                (false, Some('H')) => {
                    let row = nums.first().copied().unwrap_or(1).saturating_sub(1);
                    let col = nums.get(1).copied().unwrap_or(1).saturating_sub(1);
                    self.cursor = (row.min(self.grid.len() - 1), col.min(self.cols - 1));
                }
                (false, Some('J')) => match nums.first().copied().unwrap_or(0) {
                    2 => {
                        for row in &mut self.grid {
                            row.fill(' ');
                        }
                    }
                    0 => {
                        let (row, col) = self.cursor;
                        for cell in &mut self.grid[row][col..] {
                            *cell = ' ';
                        }
                        for r in row + 1..self.grid.len() {
                            self.grid[r].fill(' ');
                        }
                    }
                    _ => {}
                },
                (false, Some('K')) => {
                    self.grid[self.cursor.0].fill(' ');
                }
                _ => {} // SGR / 模式开关:不影响字符网格
            }
        }

        fn texts(&self) -> Vec<String> {
            self.grid
                .iter()
                .map(|row| row.iter().collect::<String>())
                .collect()
        }

        fn find(&self, needle: &str) -> Option<usize> {
            self.texts()
                .iter()
                .position(|t| t.trim_end().contains(needle))
        }
    }

    fn app(cols: u16, rows: u16) -> (TuiApp<SharedBuf>, SharedBuf, SharedSize) {
        let sink = SharedBuf::default();
        let size = SharedSize::new(cols, rows);
        let app = TuiApp::with_sink(sink.clone(), size.clone());
        (app, sink, size)
    }

    fn line(text: &str) -> UiLine {
        Line::raw(text.to_string())
    }

    #[test]
    fn committed_content_pins_to_bottom_and_scrolls_naturally() {
        let (mut app, sink, _size) = app(40, 8);
        let lines: Vec<UiLine> = (1..=12).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        app.render(&[line("tail")], None).unwrap();
        let sim_texts = {
            let mut sim = ScreenSim::new(40, 8);
            sim.feed(&sink.take());
            sim
        };
        // 13 行内容贴底滚动:屏幕剩 line6..line12 + tail
        let texts = sim_texts.texts();
        assert!(!texts.iter().any(|t| t.trim_end() == "line1"), "{texts:?}");
        assert_eq!(texts[0].trim_end(), "line6", "{texts:?}");
        assert_eq!(texts[6].trim_end(), "line12", "{texts:?}");
        assert_eq!(texts[7].trim_end(), "tail", "{texts:?}");
    }

    #[test]
    fn tail_rewrite_does_not_reprint_committed() {
        let (mut app, sink, _size) = app(40, 10);
        app.append_committed(&[line("committed-line")]);
        app.render(&[line("status-a")], None).unwrap();
        sink.take();
        // 第二帧只改尾部:输出流中不应再出现已定稿内容
        app.render(&[line("status-b")], None).unwrap();
        let frame = sink.take();
        assert!(!frame.contains("committed-line"), "{frame:?}");
        assert!(frame.contains("status-b"), "{frame:?}");
        // 第三帧尾部不变:无变化行,输出只含同步开关与光标控制
        app.render(&[line("status-b")], None).unwrap();
        let frame = sink.take();
        assert!(!frame.contains("status-b"), "无变化帧不应重写: {frame:?}");
    }

    #[test]
    fn shrink_clears_residual_rows() {
        let (mut app, sink, _size) = app(40, 10);
        app.append_committed(&[line("doc")]);
        app.render(&[line("tail-1"), line("tail-2"), line("tail-3")], None)
            .unwrap();
        app.render(&[line("tail-1")], None).unwrap();
        let mut sim = ScreenSim::new(40, 10);
        sim.feed(&sink.take());
        let texts = sim.texts();
        let doc = sim.find("doc").unwrap();
        assert!(texts[doc + 1].contains("tail-1"), "{texts:?}");
        assert!(
            texts[doc + 2].trim().is_empty() && texts[doc + 3].trim().is_empty(),
            "收缩残留应被清掉: {texts:?}"
        );
    }

    #[test]
    fn committed_growth_rewrites_tail_seam_only() {
        // 追加定稿后,旧尾部行被定稿内容替换、新尾部接在其下:
        // 定稿之前的内容行不动
        let (mut app, sink, _size) = app(40, 12);
        app.append_committed(&[line("old-doc")]);
        app.render(&[line("preview-old")], None).unwrap();
        app.append_committed(&[line("new-doc")]);
        app.render(&[line("preview-new")], None).unwrap();
        let mut sim = ScreenSim::new(40, 12);
        sim.feed(&sink.take());
        let texts = sim.texts();
        let old = sim.find("old-doc").unwrap();
        assert!(texts[old + 1].contains("new-doc"), "{texts:?}");
        assert!(texts[old + 2].contains("preview-new"), "{texts:?}");
        assert!(
            !texts.iter().any(|t| t.contains("preview-old")),
            "旧尾部应被替换: {texts:?}"
        );
    }

    #[test]
    fn width_change_requests_reshape() {
        let (mut app, _sink, size) = app(40, 10);
        app.append_committed(&[line("hello world hello")]);
        app.render(&[], None).unwrap();
        assert!(!app.take_needs_reshape());
        size.set(20, 10);
        app.render(&[], None).unwrap();
        assert!(app.take_needs_reshape(), "宽度变化应请求全量重绘");
    }

    #[test]
    fn cjk_wraps_without_injected_spaces() {
        let (mut app, sink, _size) = app(10, 8);
        app.append_committed(&[line(&"汉".repeat(12))]);
        app.render(&[], None).unwrap();
        let all = sink.take();
        // 字节流中汉字连续、无插入空格(ratatui buffer diff 空位 bug 的等价回归)
        assert!(all.contains("汉汉汉汉汉"), "{all:?}");
        assert!(!all.contains("汉 汉"), "{all:?}");
        let mut sim = ScreenSim::new(10, 8);
        sim.feed(&all);
        let texts = sim.texts();
        // 网格中宽字符占两格:整行 5 个汉字恰好铺满 10 列,余 2 字落到末行
        assert_eq!(texts[5].trim_end(), "汉 汉 汉 汉 汉", "{texts:?}");
        assert_eq!(texts[6].trim_end(), "汉 汉 汉 汉 汉", "{texts:?}");
        assert_eq!(texts[7].trim_end(), "汉 汉", "{texts:?}");
    }

    #[test]
    fn cursor_positions_inside_tail() {
        let (mut app, sink, _size) = app(40, 10);
        let doc: Vec<UiLine> = (1..=8).map(|i| line(&format!("doc{i}"))).collect();
        app.append_committed(&doc);
        // 帧 = 8 定稿 + 2 尾 = 10 行 = 屏高,viewport_top = 0;
        // 光标逻辑行 = 10 - 2 + 1 = 9 → 屏幕行 9(0 基)→ \x1b[10;4H
        app.render(&[line("tail-a"), line("tail-b")], Some((3, 1)))
            .unwrap();
        let frame = sink.take();
        assert!(frame.contains("\x1b[10;4H"), "{frame:?}");
        assert!(frame.contains(SHOW_CURSOR), "{frame:?}");
    }

    #[test]
    fn cursor_visible_when_frame_fills_screen() {
        let (mut app, sink, _size) = app(40, 6);
        let doc: Vec<UiLine> = (1..=6).map(|i| line(&format!("doc{i}"))).collect();
        app.append_committed(&doc);
        // 尾部 3 行,帧 9 行 > 6 行屏:viewport_top = 9 - 6 = 3
        app.render(
            &[line("tail-a"), line("tail-b"), line("tail-c")],
            Some((2, 1)),
        )
        .unwrap();
        let frame = sink.take();
        // 光标逻辑行 = 9 - 3 + 1 = 7 → 屏幕 7 - 3 = 4(0 基)→ \x1b[5;3H
        assert!(frame.contains("\x1b[5;3H"), "{frame:?}");
        assert!(frame.contains(SHOW_CURSOR), "{frame:?}");
    }

    #[test]
    fn cursor_hidden_during_frame_writes() {
        // 帧写入期间光标必须隐藏:不支持同步输出的终端会随行写入渲染,
        // 光标会跟着 diff 在输出区跳动闪烁;帧尾才按目标位置恢复显示
        let (mut app, sink, _size) = app(40, 10);
        app.append_committed(&[line("doc1")]);
        app.render(&[line("tail")], Some((0, 0))).unwrap();
        let frame = sink.take();
        let sync_begin = frame.find(SYNC_BEGIN).unwrap();
        // 帧首(SYNC_BEGIN 之后、任何行内容写入之前)先隐藏
        let head = &frame[sync_begin + SYNC_BEGIN.len()..];
        assert!(head.starts_with(HIDE_CURSOR), "{head:?}");
        assert!(frame.contains(SHOW_CURSOR), "帧尾应恢复显示: {frame:?}");
        // SHOW 在所有行写入之后(帧尾)
        assert!(frame.rfind(SHOW_CURSOR).unwrap() > frame.rfind("tail").unwrap());

        // cursor = None:整帧保持隐藏,不出现 SHOW
        app.render(&[line("tail")], None).unwrap();
        let frame = sink.take();
        assert!(frame.contains(HIDE_CURSOR));
        assert!(!frame.contains(SHOW_CURSOR), "{frame:?}");
    }

    #[test]
    fn serialize_line_emits_sgr_and_resets() {
        let theme_fg = Color::Rgb(0x10, 0x20, 0x30);
        let styled = Line::from(vec![
            Span::styled("红", Style::new().fg(Color::Red).bg(Color::Blue)),
            Span::raw("素"),
            Span::styled("粗", Style::new().fg(theme_fg).add_modifier(Modifier::BOLD)),
        ]);
        let out = serialize_line(&styled);
        assert!(out.starts_with("\x1b[0;31;44m红"), "{out:?}");
        assert!(out.contains("素"), "{out:?}");
        assert!(out.contains("\x1b[0;38;2;16;32;48;1m粗"), "{out:?}");
        assert!(out.ends_with("\x1b[0m"), "{out:?}");
        // 纯文本行不产生任何转义
        assert_eq!(serialize_line(&Line::raw("plain")), "plain");
    }

    #[test]
    fn finish_clears_tail_and_keeps_committed() {
        let (mut app, sink, _size) = app(40, 10);
        app.append_committed(&[line("keep-me")]);
        app.render(&[line("tail")], None).unwrap();
        app.finish().unwrap();
        let all = sink.take();
        assert!(all.contains(AUTO_WRAP_ON), "收尾应恢复自动换行");
        let mut sim = ScreenSim::new(40, 10);
        sim.feed(&all);
        let texts = sim.texts();
        let keep = sim.find("keep-me").unwrap();
        assert!(texts[keep + 1].trim().is_empty(), "尾部应被清掉: {texts:?}");
        assert!(!texts.iter().any(|t| t.contains("tail")), "{texts:?}");
    }

    #[test]
    fn keyboard_protocol_bytes_match_crossterm_commands() {
        // 锦定手写转义与 crossterm 官方命令字节一致,防两处漂移
        use ratatui::crossterm::Command;
        use ratatui::crossterm::event::{
            DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
            PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
        };
        let mut push = String::new();
        // 与上游 pi 一致:disambiguate + 事件类型 + alternate keys(不启用
        // 全键上报,避免破坏 CJK 输入)
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
        PushKeyboardEnhancementFlags(flags)
            .write_ansi(&mut push)
            .unwrap();
        assert_eq!(push, KEYBOARD_PUSH);
        let mut pop = String::new();
        PopKeyboardEnhancementFlags.write_ansi(&mut pop).unwrap();
        assert_eq!(pop, KEYBOARD_POP);
        let mut paste_on = String::new();
        EnableBracketedPaste.write_ansi(&mut paste_on).unwrap();
        assert_eq!(paste_on, BRACKETED_PASTE_ON);
        let mut paste_off = String::new();
        DisableBracketedPaste.write_ansi(&mut paste_off).unwrap();
        assert_eq!(paste_off, BRACKETED_PASTE_OFF);
        // 启动序列开启、收尾序列关闭
        assert!(paste_on != paste_off);
    }
}
