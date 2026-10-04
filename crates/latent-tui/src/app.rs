//! 全帧差分终端屏幕,双渲染模式:
//!
//! - **regular**(pi TuiMainScreen 对应物):内容滚入终端原生 scrollback。
//!   追加行越过屏幕底部时打印 `\r\n` 让终端自然滚动,顶部行就此进入
//!   scrollback,此后永不再碰;
//! - **fullscreen**(pi TuiAltScreen 对应物):alternate screen(`\x1b[?1049h`)
//!   加屏幕内滚动。历史行在「历史窗口」(屏高 − 尾部高)内按视口偏移显示,
//!   尾部(预览/编辑器/footer)钉死在屏幕底部;追加内容只在上滚(follow
//!   关闭)时移动视口,follow 时视口始终贴住最新内容。退出时把定稿文档
//!   dump 回主屏,转录内容得以进入终端 scrollback。
//!
//! 两种模式共享同一套「已定稿行」(`committed`,只追加、ANSI 序列化缓存),
//! 切换模式零数据迁移。
//!
//! 不变量:
//! - committed 行按当前终端宽度折行,帧间视为不可变;
//! - regular:差分扫描跳过 committed 前缀,变化落在已滚入 scrollback 的
//!   行上时全量重绘兜底;`viewport_top` 追踪屏幕顶行对应的逻辑行号
//!   (锚定期为负:内容贴底),随 `\r\n` 滚动事件单调递增;
//! - fullscreen:整屏逐行差分(`prev_screen` 缓存上帧每行内容),写入全部
//!   绝对定位、绝不用 `\r\n` 滚动(alt screen 无 scrollback,滚过即丢);
//! - 自动换行(DECAWM)在会话期间关闭,行宽由内部折行保证,写入绝不
//!   触发终端回绕;
//! - 终端恢复(raw mode / 光标 / alternate screen)在 `finish` 与 `Drop`
//!   双路径兜底。

use std::io::{self, Stdout, Write};
use std::rc::Rc;
use std::cell::RefCell;

use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use crate::selection::{
    line_plain, osc52_clipboard, row_range, split_at_width, MouseAction, SelPoint,
};
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
/// alternate screen 进入/退出(fullscreen 模式):1049 = 切换缓冲并保存/
/// 恢复光标与主屏内容,退出时终端自动还原进入前的主屏。
const ALT_ENTER: &str = "\x1b[?1049h";
const ALT_EXIT: &str = "\x1b[?1049l";
/// 鼠标捕获(仅 fullscreen):1000 = 按下/滚轮上报,1002 = 按住拖动上报
/// (选区手势的基础),1006 = SGR 扩展坐标。捕获后终端不提供原生选择,
/// 文字选择/复制由应用层实现(见 selection.rs:拖选高亮 + cmd+C/
/// Ctrl+Shift+C / 松开自动复制,经 OSC 52 写入系统剪贴板)。
const MOUSE_ON: &str = "\x1b[?1000;1002;1006h";
const MOUSE_OFF: &str = "\x1b[?1000;1002;1006l";
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
    /// 已定稿行(折行后 ANSI 序列化缓存;只追加,帧间不可变;两模式共享)
    committed: Vec<String>,
    /// 已定稿行的折行原文(与 `committed` 一一对应;选区高亮重序列化与
    /// 复制取文本用)
    committed_cells: Vec<UiLine>,
    /// 鼠标选区(锚点+终点,内容锚定坐标;两点相同 = 无有效选区;
    /// 仅 fullscreen)。行号是帧行号(0 = committed 首行,≥ committed
    /// 长度 = 尾部行),滚动视口后高亮与复制仍跟随原文字
    selection: Option<(SelPoint, SelPoint)>,
    /// 松开拖选时自动复制到剪贴板(/setting「选中后自动复制」;选择、
    /// 高亮与快捷键复制不受此开关影响)
    auto_copy_on_select: bool,
    /// 右上角瞬时提示(文本 + 到期时刻;复制成功时显示,render_fullscreen
    /// 覆盖到第 0 行右端,到期后由差分自动消除)
    toast: Option<(String, std::time::Instant)>,
    /// 上一帧尾部折行后的行(复制选区时提取尾部文本用)
    last_tail_cells: Vec<UiLine>,
    /// 上一帧的尾部活动行(regular 模式差分用)
    prev_tail: Vec<String>,
    /// 上一帧时 committed 的长度(regular 差分稳定前缀边界)
    prev_committed_len: usize,
    /// 屏幕顶行对应的逻辑行号(锚定期为负;regular 模式)
    viewport_top: isize,
    /// fullscreen 模式开关(alternate screen + 屏幕内滚动)
    fullscreen: bool,
    /// fullscreen:视口是否贴住最新内容(true = 历史窗口底边随 committed
    /// 末端滚动;false = 停在 `scroll_offset` 不动,新内容只在下方堆积)
    follow: bool,
    /// fullscreen:follow 关闭时屏幕顶行对应的逻辑行号(绝对锚定,新内容
    /// 到达不移动)
    scroll_offset: usize,
    /// fullscreen:上一帧尾部行数(滚动 API 计算历史窗口高用)
    last_tail_len: usize,
    /// fullscreen:上一帧整屏内容(逐屏行差分;None = 空行/未写过)
    prev_screen: Vec<Option<String>>,
    finished: bool,
    /// 尺寸变化待处理:调用方须走 `redraw_all` 重折行
    needs_reshape: bool,
    query_size: Box<dyn Fn() -> io::Result<(u16, u16)>>,
    /// 是否恢复真实终端(Drop 兜底用;内存 sink 上跳过 raw mode)。
    restores_terminal: bool,
}

/// fullscreen 模式翻页重叠行数(对齐 pi `PAGE_SCROLL_OVERLAP`):翻页后
/// 上一页末尾仍可见,保持阅读连续性。
const PAGE_SCROLL_OVERLAP: usize = 4;

/// 右上角复制提示框的显示时长。
const TOAST_DURATION: std::time::Duration = std::time::Duration::from_millis(1500);

impl TuiApp<Stdout> {
    /// 打开终端(regular 模式):raw mode + 关自动换行 + 光标锚定到屏幕底部。
    pub fn open() -> io::Result<Self> {
        Self::open_with_mode(false)
    }

    /// 打开终端并选择渲染模式。fullscreen = alternate screen + 屏幕内滚动
    /// (历史窗口 + 钉底尾部 + 鼠标捕获下的应用层选区/复制);
    /// regular = 终端原生 scrollback。
    pub fn open_with_mode(fullscreen: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let (cols, rows) = size().unwrap_or((80, 24));
        let mut app = TuiApp {
            out: io::stdout(),
            cols,
            rows,
            committed: Vec::new(),
            committed_cells: Vec::new(),
            selection: None,
            auto_copy_on_select: false,
            toast: None,
            last_tail_cells: Vec::new(),
            prev_tail: Vec::new(),
            prev_committed_len: 0,
            viewport_top: if fullscreen { 0 } else { 1 - rows as isize },
            fullscreen,
            follow: true,
            scroll_offset: 0,
            last_tail_len: 0,
            prev_screen: vec![None; rows as usize],
            finished: false,
            needs_reshape: false,
            query_size: Box::new(size),
            restores_terminal: true,
        };
        let mut boot = String::new();
        if fullscreen {
            // alt screen 从空白主屏切入,无需贴底锚定
            boot.push_str(ALT_ENTER);
            boot.push_str("\x1b[2J");
        } else {
            // 锚定不变量:光标先滚到屏幕底部,内容从此贴底自然滚动(在
            // shell 提示符后启动时,首帧才不会错位)
            boot.push_str(AUTO_WRAP_OFF);
        }
        boot.push_str(KEYBOARD_PUSH);
        boot.push_str(BRACKETED_PASTE_ON);
        if fullscreen {
            boot.push_str(MOUSE_ON);
        }
        boot.push_str(HIDE_CURSOR);
        if !fullscreen {
            boot.push_str(&"\r\n".repeat(rows.saturating_sub(1) as usize));
        }
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
    /// 测试构造:内存输出汇 + 固定/可变尺寸,跳过 raw mode(regular 模式)。
    pub fn with_sink(sink: W, size: SharedSize) -> Self {
        Self::with_sink_mode(sink, size, false)
    }

    /// 测试构造(可选模式):fullscreen = true 时走 alternate screen 渲染路径。
    pub fn with_sink_mode(sink: W, size: SharedSize, fullscreen: bool) -> Self {
        let (cols, rows) = *size.0.borrow();
        TuiApp {
            out: sink,
            cols,
            rows,
            committed: Vec::new(),
            committed_cells: Vec::new(),
            selection: None,
            auto_copy_on_select: false,
            toast: None,
            last_tail_cells: Vec::new(),
            prev_tail: Vec::new(),
            prev_committed_len: 0,
            viewport_top: if fullscreen { 0 } else { 1 - rows as isize },
            fullscreen,
            follow: true,
            scroll_offset: 0,
            last_tail_len: 0,
            prev_screen: vec![None; rows as usize],
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
                self.committed_cells.push(wrapped.clone());
                self.committed.push(serialize_line(&wrapped));
            }
        }
    }

    /// 渲染一帧:按当前模式派发。`tail` 为活动尾部行(预览/状态/编辑器/
    /// footer);`cursor` 为相对尾部首行的 (列, 行);None = 隐藏光标。
    pub fn render(&mut self, tail: &[UiLine], cursor: Option<(u16, u16)>) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.sync_size();
        if self.needs_reshape {
            // 尺寸已变:本帧跳过,等调用方 redraw_all 后再画
            return Ok(());
        }
        if self.fullscreen {
            self.render_fullscreen(tail, cursor)
        } else {
            self.render_regular(tail, cursor)
        }
    }

    /// 当前是否 fullscreen(alternate screen)模式。
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// regular 模式渲染:`tail` 与 `committed` 拼成全帧做行级差分后输出。
    fn render_regular(&mut self, tail: &[UiLine], cursor: Option<(u16, u16)>) -> io::Result<()> {
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

    /// fullscreen 模式渲染:历史窗口(屏高 − 尾部高,按视口偏移取
    /// committed 窗口)+ 钉底尾部,整屏逐行差分(对照 `prev_screen`),
    /// 变化行绝对定位重写,绝不用 `\r\n` 滚动。存在鼠标选区时对选中
    /// 单元格区间叠加反色高亮(重序列化该行)。
    fn render_fullscreen(&mut self, tail: &[UiLine], cursor: Option<(u16, u16)>) -> io::Result<()> {
        let width = self.cols.max(1) as usize;
        let tail_cells: Vec<UiLine> = tail
            .iter()
            .flat_map(|line| wrap_line(line, width))
            .collect();
        let tail_s: Vec<String> = tail_cells.iter().map(|line| serialize_line(line)).collect();
        let rows = self.rows as usize;
        self.last_tail_len = tail_s.len();
        let (hist_h, top) = self.viewport();
        let sel = self.selection.filter(|_| self.fullscreen);

        // 组装新帧整屏内容(历史窗口 + 尾部钉底;未覆盖的行 = 空白)。
        // 有选区的行走重序列化(反色叠加),其余用序列化缓存。选区以
        // 帧行号锚定内容:屏幕行先换算回帧行号再求列区间,滚动后高亮
        // 跟随文字而非停留在屏幕原位
        let committed_len = self.committed.len();
        let mut new_screen: Vec<Option<String>> = vec![None; rows];
        for (sr, slot) in new_screen.iter_mut().enumerate() {
            let frame_row: Option<usize> = if sr < hist_h {
                let li = top + sr as isize;
                (li >= 0).then_some(li as usize)
            } else {
                Some(committed_len + (sr - hist_h))
            };
            let range = sel.and_then(|(anchor, end)| {
                frame_row.and_then(|fr| row_range(anchor, end, fr, width as u16))
            });
            if sr < hist_h {
                let li = top + sr as isize;
                if li >= 0 && (li as usize) < committed_len {
                    let i = li as usize;
                    *slot = Some(match range {
                        Some(r) => serialize_line_selected(&self.committed_cells[i], Some(r)),
                        None => self.committed[i].clone(),
                    });
                }
            } else {
                // 尾部行从 hist_h 起铺(防御性截断:尾部超出屏高时保留
                // 末尾可见行)
                let skip = tail_s.len().saturating_sub(rows - hist_h);
                let ti = sr - hist_h + skip;
                if let Some(line) = tail_cells.get(ti) {
                    *slot = Some(match range {
                        Some(r) => serialize_line_selected(line, Some(r)),
                        None => tail_s[ti].clone(),
                    });
                }
            }
        }

        // 右上角瞬时提示框:覆盖第 0 行右端(反色标签;原行按显示宽度
        // 截断拼接),出现/消失各触发一行差分重写
        let toast = self.active_toast().map(str::to_string);
        if let Some(toast_text) = toast {
            let label = format!(" {toast_text} ");
            let label_w = crate::width::display_width(&label).min(width);
            let source = if hist_h > 0 {
                let li = top;
                if li >= 0 && (li as usize) < self.committed_cells.len() {
                    Some(line_plain(&self.committed_cells[li as usize]))
                } else if hist_h < rows {
                    self.last_tail_cells.first().map(line_plain)
                } else {
                    None
                }
            } else {
                self.last_tail_cells.first().map(line_plain)
            };
            let plain = source.unwrap_or_default();
            let (head, used) = crate::width::truncate_to_width(&plain, width - label_w);
            let pad = " ".repeat(width - label_w - used);
            let toast_line = crate::UiLine::from(vec![
                ratatui::text::Span::raw(head),
                ratatui::text::Span::raw(pad),
                ratatui::text::Span::styled(
                    label,
                    ratatui::style::Style::new().add_modifier(ratatui::style::Modifier::REVERSED),
                ),
            ]);
            new_screen[0] = Some(serialize_line(&toast_line));
        }

        // 整屏逐行差分,只重写变化行(不连续的变化行也会被夹在
        // first..=last 里重写,内容相同无副作用)
        let mut first = usize::MAX;
        let mut last = 0usize;
        for (sr, new) in new_screen.iter().enumerate() {
            let old = self.prev_screen.get(sr).and_then(|o| o.as_deref());
            if old != new.as_deref() {
                first = first.min(sr);
                last = last.max(sr);
            }
        }
        let mut out = String::from(SYNC_BEGIN);
        out.push_str(HIDE_CURSOR);
        if first != usize::MAX {
            for (sr, line) in new_screen.iter().enumerate().take(last + 1).skip(first) {
                out.push_str(&move_to(0, sr as u16));
                out.push_str(CLEAR_LINE);
                if let Some(text) = line {
                    out.push_str(text);
                }
            }
        }
        out.push_str(SYNC_END);
        if let Some((col, rel_row)) = cursor {
            // 光标永远钉在尾部(输入框)内:尾部恒定钉在屏幕底部,屏幕行 =
            // 历史窗口高 + 尾部内相对行。不随视口滚动换算 —— 上滚查看
            // 历史时光标仍停在输入框,绝不会落到屏幕外(应用中唯一可输入
            // 的位置就是输入框,regular 模式同理)
            let sr = hist_h + rel_row as usize;
            if sr < rows {
                out.push_str(&move_to(col, sr as u16));
                out.push_str(SHOW_CURSOR);
            }
            // 越界时保持帧首的隐藏态(无需重复 HIDE)
        }
        self.out.write_all(out.as_bytes())?;
        self.out.flush()?;
        self.prev_screen = new_screen;
        self.last_tail_cells = tail_cells;
        Ok(())
    }

    /// 当前视口布局:(历史窗口行数, 屏幕顶行对应的逻辑行号)。顶行可为负
    /// (follow 且内容不足一屏时内容贴住窗口底边,上方留白);非 follow 时
    /// 停在冻结偏移(clamp 防内容收缩后越界,写回保证滚动 API 一致)。
    fn viewport(&mut self) -> (usize, isize) {
        let rows = self.rows as usize;
        let hist_h = rows.saturating_sub(self.last_tail_len);
        let max_top = self.committed.len().saturating_sub(hist_h);
        let top = if self.follow {
            self.committed.len() as isize - hist_h as isize
        } else {
            let t = self.scroll_offset.min(max_top);
            self.scroll_offset = t;
            t as isize
        };
        (hist_h, top)
    }

    /// 屏幕单元格 → 选区坐标(帧行号 + 列)。历史行换算为 committed 逻辑
    /// 行号(视口上方留白 clamp 到 0),尾部行换算为 committed.len() +
    /// 尾部偏移 —— 选区从此锚定内容,滚动后仍指向原文字。
    fn frame_point(&mut self, col: u16, row: u16) -> SelPoint {
        let (hist_h, top) = self.viewport();
        let sr = row as usize;
        let fr = if sr < hist_h {
            (top + sr as isize).max(0) as usize
        } else {
            self.committed.len() + (sr - hist_h)
        };
        (fr, col)
    }

    /// 鼠标选区手势(fullscreen 专用):按下开始新手势并清除旧选区
    /// (按住 Shift 则改为扩展现有选区到点击处,锚点不变),拖动更新终点,
    /// 抬起时若拖出过非零区间则自动复制(对齐 pi
    /// fullscreenCopyOnSelect;单击不产生选区也不复制)。regular 模式无操作。
    pub fn on_mouse(&mut self, action: MouseAction) -> io::Result<()> {
        if self.finished || !self.fullscreen {
            return Ok(());
        }
        match action {
            MouseAction::Down { col, row, extend } => {
                let point = self.frame_point(col, row);
                match (extend, self.selection) {
                    (true, Some((anchor, _))) => self.selection = Some((anchor, point)),
                    _ => self.selection = Some((point, point)),
                }
            }
            MouseAction::Drag { col, row } => {
                let point = self.frame_point(col, row);
                if let Some(sel) = &mut self.selection {
                    sel.1 = point;
                } else {
                    self.selection = Some((point, point));
                }
            }
            MouseAction::Up { col, row } => {
                let point = self.frame_point(col, row);
                let anchor = match self.selection {
                    Some((anchor, _)) => anchor,
                    None => return Ok(()),
                };
                if anchor == point {
                    // 单击:清选区,无复制
                    self.selection = None;
                    return Ok(());
                }
                self.selection = Some((anchor, point));
                // 自动复制是可选行为(/setting);选择/高亮/快捷键复制恒可用
                if self.auto_copy_on_select {
                    self.copy_selection()?;
                }
            }
        }
        Ok(())
    }

    /// 设置「松开拖选自动复制」开关(不影响选择/高亮/快捷键复制)。
    pub fn set_auto_copy_on_select(&mut self, on: bool) {
        self.auto_copy_on_select = on;
    }

    /// 当前是否有有效选区(非单击的拖选区间;Ctrl+C 复制的判据)。
    pub fn has_selection(&self) -> bool {
        match self.selection {
            Some((anchor, end)) => anchor != end,
            None => false,
        }
    }

    /// 清除选区(高亮由下一帧差分自动消除)。
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// 把当前选区文本写入系统剪贴板(OSC 52;终端不支持时静默忽略)。
    /// 选区锚定内容坐标:即使两端已滚出屏幕,仍按帧行号完整复制。
    /// 返回是否真的复制了内容(无选区/选中文本为空 = false)。
    pub fn copy_selection(&mut self) -> io::Result<bool> {
        let Some(((ar, ac), (er, ec))) = self.selection else {
            return Ok(false);
        };
        let width = self.cols.max(1);
        let committed_len = self.committed_cells.len();
        let mut rows: Vec<(String, (u16, u16))> = Vec::new();
        for fr in ar.min(er)..=ar.max(er) {
            let Some(range) = row_range((ar, ac), (er, ec), fr, width) else {
                continue;
            };
            // 帧行号 → 文本来源(committed 或尾部行;越界/空行跳过)
            let plain = if fr < committed_len {
                Some(line_plain(&self.committed_cells[fr]))
            } else {
                self.last_tail_cells
                    .get(fr - committed_len)
                    .map(line_plain)
                    .filter(|text| !text.is_empty())
            };
            if let Some(text) = plain {
                rows.push((text, range));
            }
        }
        let text = crate::selection::selection_text(&rows);
        if text.is_empty() {
            return Ok(false);
        }
        self.write_raw(osc52_clipboard(&text).as_bytes())?;
        self.show_toast("Copied!");
        Ok(true)
    }

    /// 显示右上角瞬时提示(约 1.5s 后由渲染差分自动消除)。
    pub fn show_toast(&mut self, text: &str) {
        self.toast = Some((
            text.to_string(),
            std::time::Instant::now() + TOAST_DURATION,
        ));
    }

    /// toast 到期时刻(事件循环据此在空闲状态下也能触发重绘消除)。
    pub fn toast_deadline(&self) -> Option<std::time::Instant> {
        self.toast.as_ref().map(|(_, deadline)| *deadline)
    }

    #[cfg(test)]
    fn set_toast_expiry_for_test(&mut self, at: std::time::Instant) {
        if let Some((_, deadline)) = &mut self.toast {
            *deadline = at;
        }
    }

    /// 取仍有效的 toast(过期即清除并返回 None)。
    fn active_toast(&mut self) -> Option<&str> {
        let expired = matches!(&self.toast, Some((_, deadline)) if std::time::Instant::now() >= *deadline);
        if expired {
            self.toast = None;
        }
        self.toast.as_ref().map(|(text, _)| text.as_str())
    }

    /// 翻页上滚(fullscreen 专用,其余模式无操作):视口上移一页,关闭
    /// follow——后续新内容不再拉动视口。
    pub fn scroll_page_up(&mut self) {
        self.scroll_lines(-(self.page_size() as isize));
    }

    /// 翻页下滚:视口下移一页,到达底部时恢复 follow。
    pub fn scroll_page_down(&mut self) {
        self.scroll_lines(self.page_size() as isize);
    }

    /// 滚到顶 / 滚到底(End 恢复 follow)。
    pub fn scroll_top(&mut self) {
        if !self.fullscreen {
            return;
        }
        self.follow = false;
        self.scroll_offset = 0;
    }

    pub fn scroll_bottom(&mut self) {
        if !self.fullscreen {
            return;
        }
        self.follow = true;
    }

    /// 有向滚动 n 行:正数向下、负数向上(键盘翻页与鼠标滚轮共用)。上滚
    /// 关闭 follow;下滚触底自动恢复 follow。regular 模式无操作(终端
    /// scrollback 自管)。
    pub fn scroll_lines(&mut self, n: isize) {
        if !self.fullscreen || n == 0 {
            return;
        }
        let rows = self.rows as usize;
        let hist_h = rows.saturating_sub(self.last_tail_len);
        let max_top = self.committed.len().saturating_sub(hist_h);
        let current = if self.follow {
            max_top
        } else {
            self.scroll_offset
        } as isize;
        let next = (current + n).clamp(0, max_top as isize) as usize;
        if next >= max_top {
            self.follow = true;
        } else {
            self.follow = false;
            self.scroll_offset = next;
        }
    }

    /// 翻页行数:屏高 − 重叠行,至少 1。
    fn page_size(&self) -> usize {
        (self.rows as usize).saturating_sub(PAGE_SCROLL_OVERLAP).max(1)
    }

    /// 全量重绘:清可视屏幕 → 重新锚定 → 重打整份定稿文档(ctrl+o /
    /// 主题切换 / /new / 尺寸变化 / 模式切换路径)。活动尾部由紧随其后的
    /// `render` 追加。
    pub fn redraw_all(&mut self, lines: &[UiLine]) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.sync_size();
        self.committed.clear();
        self.committed_cells.clear();
        self.selection = None;
        self.append_committed(lines);
        let mut out = String::from(SYNC_BEGIN);
        out.push_str(HIDE_CURSOR); // 全量重绘期间隐藏光标(同 render 帧首隐藏)
        if self.fullscreen {
            // alt screen:清屏即可,历史窗口整屏交由下一帧 render 重写
            // (prev_screen 全 None → 差分视为全变化);不用 \r\n 锚定
            out.push_str("\x1b[2J");
            self.follow = true;
            self.scroll_offset = 0;
            self.prev_screen = vec![None; self.rows as usize];
        } else {
            self.reset_screen(&mut out);
            for i in 0..self.committed.len() {
                self.write_row(i, &[], &mut out);
            }
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
        let mut out = String::new();
        if self.fullscreen {
            // 先退出 alt screen:子进程继承进入前的原始主屏;不清屏
            // (1049l 已由终端还原主屏内容)
            out.push_str(ALT_EXIT);
            out.push_str(MOUSE_OFF);
            out.push_str(AUTO_WRAP_ON);
            out.push_str(SHOW_CURSOR);
        } else {
            out.push_str(AUTO_WRAP_ON);
            out.push_str(SHOW_CURSOR);
            // 清可视屏幕:外部编辑器从干净屏幕开始(scrollback 不受影响)
            out.push_str("\x1b[2J");
            out.push_str(&move_to(0, 0));
        }
        if self.restores_terminal {
            // 内存 sink(测试)不写,避免污染回放字节流
            out.push_str(BRACKETED_PASTE_OFF);
            out.push_str(KEYBOARD_POP);
        }
        self.write_raw(out.as_bytes())?;
        if self.restores_terminal {
            disable_raw_mode()?;
        }
        Ok(())
    }

    /// 从 `suspend` 恢复:重回 raw mode + 终端模式序列,按当前定稿文档
    /// 全量重绘(编辑期间窗口尺寸变化经 sync_size 感知,走重折行)。
    pub fn resume(&mut self, lines: &[UiLine]) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        if self.restores_terminal {
            enable_raw_mode()?;
        }
        let mut out = String::new();
        if self.fullscreen {
            out.push_str(ALT_ENTER);
            out.push_str(MOUSE_ON);
        }
        out.push_str(AUTO_WRAP_OFF);
        out.push_str(HIDE_CURSOR);
        if self.restores_terminal {
            out.push_str(KEYBOARD_PUSH);
            out.push_str(BRACKETED_PASTE_ON);
        }
        self.write_raw(out.as_bytes())?;
        self.redraw_all(lines)
    }

    /// 运行时切换渲染模式(fullscreen ↔ regular):写 alternate screen 序列
    /// 并重置屏幕侧状态;`committed` 两模式共享,调用方随后以当前文档
    /// `redraw_all` + `render` 重画(切回 regular 时主屏仍是进入前的旧
    /// 内容,必须全量重打)。
    pub fn set_fullscreen(&mut self, on: bool) -> io::Result<()> {
        if self.finished || self.fullscreen == on {
            return Ok(());
        }
        self.fullscreen = on;
        let mut out = String::new();
        if on {
            out.push_str(ALT_ENTER);
            out.push_str("\x1b[2J");
            out.push_str(MOUSE_ON);
            self.follow = true;
            self.scroll_offset = 0;
            self.prev_screen = vec![None; self.rows as usize];
        } else {
            out.push_str(ALT_EXIT);
            out.push_str(MOUSE_OFF);
            self.prev_screen.clear();
            self.prev_tail.clear();
        }
        self.write_raw(out.as_bytes())
    }

    /// 收尾:regular 模式清掉活动尾部区,光标落回最后一条定稿行末尾,
    /// 恢复终端(定稿文档留在屏幕/scrollback)。fullscreen 模式退出
    /// alternate screen 后把定稿文档打回主屏(对齐 pi
    /// `fullscreenExitOutput = "transcript"`:全屏期间的内容经 dump 进入
    /// 终端 scrollback,不随 1049l 消失)。
    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let mut out = String::new();
        if self.fullscreen {
            // 退出 alt screen(终端自动还原进入前的主屏),再打印转录。
            // 定稿行已按当前宽度折行,主屏上不会触发意外回绕
            out.push_str(ALT_EXIT);
            out.push_str(MOUSE_OFF);
            out.push_str(AUTO_WRAP_ON);
            for line in &self.committed {
                out.push_str(line);
                out.push_str("\r\n");
            }
            out.push_str(BRACKETED_PASTE_OFF);
            out.push_str(SHOW_CURSOR);
            if self.restores_terminal {
                // 内存 sink(测试)不写,避免污染回放字节流
                out.push_str(KEYBOARD_POP);
            }
            self.out.write_all(out.as_bytes())?;
            self.out.flush()?;
            disable_raw_mode()?;
            return Ok(());
        }
        let rows = self.rows as isize;
        let frame_len = self.prev_committed_len + self.prev_tail.len();
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
                if self.fullscreen {
                    // panic 路径不做 dump,仅退出 alt screen 还原主屏
                    let _ = self.out.write_all(ALT_EXIT.as_bytes());
                    let _ = self.out.write_all(MOUSE_OFF.as_bytes());
                }
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
    serialize_line_selected(line, None)
}

/// `serialize_line` 的选区扩展:`sel` 为该行被选中的单元格半开区间,
/// 区间内的文本以 REVERSED 修饰输出(跨 span 按显示宽度切分,样式与
/// 原行精确一致,差分字符串仍可比较)。
fn serialize_line_selected(line: &Line<'_>, sel: Option<(u16, u16)>) -> String {
    let mut out = String::new();
    let mut current = Style::new();
    let mut cell = 0usize;
    for span in &line.spans {
        let style = line.style.patch(span.style);
        let content = span.content.as_ref();
        let (head, selected, rest) = match sel {
            None => (content, "", ""),
            Some((start, end)) => {
                let (start, end) = (start as usize, end as usize);
                let span_w = crate::width::display_width(content);
                let span_end = cell + span_w;
                // 与选区间求交(半开;空交集 = 整段原样)
                let lo = cell.max(start);
                let hi = span_end.min(end);
                if lo >= hi {
                    (content, "", "")
                } else {
                    let (pre, rest) = split_at_width(content, lo - cell);
                    let (mid, post) = split_at_width(rest, hi - lo);
                    (pre, mid, post)
                }
            }
        };
        for (text, reversed) in
            [(head, false), (selected, true), (rest, false)]
        {
            if text.is_empty() {
                continue;
            }
            let seg_style = if reversed {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            };
            if seg_style != current {
                out.push_str(&sgr_sequence(seg_style));
                current = seg_style;
            }
            out.push_str(text);
        }
        cell += crate::width::display_width(content);
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
            if next == Some(']') {
                // OSC 序列(如 OSC 52 剪贴板):吞到 BEL 或 ST,不上网格
                let mut prev_esc = false;
                for c in chars.by_ref() {
                    if c == '\x07' {
                        break;
                    }
                    if prev_esc && c == '\\' {
                        break;
                    }
                    prev_esc = c == '\x1b';
                }
                return;
            }
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
                // alternate screen 进出:模拟为清屏 + 光标复位(退出时终端
                // 还原的主屏内容不追踪,dump 内容落进空白网格可断言)
                (true, Some('h') | Some('l')) if nums.contains(&1049) => {
                    for row in &mut self.grid {
                        row.fill(' ');
                    }
                    self.cursor = (0, 0);
                }
                _ => {} // SGR / 其他模式开关(鼠标、DECAWM…):不影响字符网格
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

    fn app_fullscreen(cols: u16, rows: u16) -> (TuiApp<SharedBuf>, SharedBuf, SharedSize) {
        let sink = SharedBuf::default();
        let size = SharedSize::new(cols, rows);
        let app = TuiApp::with_sink_mode(sink.clone(), size.clone(), true);
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

    // ---- fullscreen 模式(alternate screen + 屏幕内滚动)----

    #[test]
    fn fullscreen_pins_tail_to_bottom() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        let lines: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        app.render(&[line("tail")], None).unwrap();
        let mut sim = ScreenSim::new(40, 8);
        sim.feed(&sink.take());
        let texts = sim.texts();
        // 历史窗口 7 行(line14..line20)+ 尾部钉死在底行
        assert_eq!(texts[0].trim_end(), "line14", "{texts:?}");
        assert_eq!(texts[6].trim_end(), "line20", "{texts:?}");
        assert_eq!(texts[7].trim_end(), "tail", "{texts:?}");
    }

    #[test]
    fn fullscreen_page_up_shifts_window_keeps_tail() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        let lines: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        let mut sim = ScreenSim::new(40, 8);
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        // 翻页 = 屏高 8 − 重叠 4 = 4 行:视口顶 13 → 9(line10..line16);
        // 尾部行内容不变、不重写,屏幕上保持原样
        app.scroll_page_up();
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        let texts = sim.texts();
        assert_eq!(texts[0].trim_end(), "line10", "{texts:?}");
        assert_eq!(texts[6].trim_end(), "line16", "{texts:?}");
        assert_eq!(texts[7].trim_end(), "tail", "{texts:?}");
    }

    #[test]
    fn fullscreen_page_down_returns_to_follow() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        let lines: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        let mut sim = ScreenSim::new(40, 8);
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        // 上翻两页(顶 13 → 9 → 5)再下翻两页 → 回到底部,恢复 follow
        app.scroll_page_up();
        app.scroll_page_up();
        app.scroll_page_down();
        app.scroll_page_down();
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        let texts = sim.texts();
        assert_eq!(texts[0].trim_end(), "line14", "{texts:?}");
        assert_eq!(texts[7].trim_end(), "tail", "{texts:?}");
    }

    #[test]
    fn fullscreen_scroll_stays_put_on_new_content() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        let lines: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        let mut sim = ScreenSim::new(40, 8);
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        // 上滚后追加新内容:视口冻结,新内容不拉走屏幕
        app.scroll_page_up();
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        let more: Vec<UiLine> = (21..=24).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&more);
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        let texts = sim.texts();
        assert_eq!(texts[0].trim_end(), "line10", "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("line24")), "{texts:?}");
        // End 恢复 follow:最新内容回到视口
        app.scroll_bottom();
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        let texts = sim.texts();
        assert_eq!(texts[6].trim_end(), "line24", "{texts:?}");
        assert_eq!(texts[7].trim_end(), "tail", "{texts:?}");
    }

    #[test]
    fn fullscreen_wheel_scroll_lines() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        let lines: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        app.render(&[line("tail")], None).unwrap();
        sink.take();
        // 滚轮上行 3 行:顶 13 → 10
        app.scroll_lines(-3);
        app.render(&[line("tail")], None).unwrap();
        let mut sim = ScreenSim::new(40, 8);
        sim.feed(&sink.take());
        assert_eq!(sim.texts()[0].trim_end(), "line11");
    }

    #[test]
    fn fullscreen_cursor_pinned_to_editor_while_scrolled() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        let lines: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        // 光标(相对尾部首行 rel_row=0)钉在底行上一行:屏幕行 = hist_h = 7
        app.render(&[line("tail")], Some((3, 0))).unwrap();
        assert!(sink.take().contains("\x1b[8;4H"));
        // 上滚后光标仍在输入框原位(绝不随视口换算出屏)
        app.scroll_page_up();
        app.render(&[line("tail")], Some((3, 0))).unwrap();
        let frame = sink.take();
        assert!(frame.contains("\x1b[8;4H"), "{frame:?}");
        assert!(frame.contains(SHOW_CURSOR), "{frame:?}");
    }

    #[test]
    fn fullscreen_mouse_selection_highlights_and_copies() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        app.set_auto_copy_on_select(true);
        app.append_committed(&[line("hello world")]);
        app.render(&[line("tail")], None).unwrap();
        sink.take();
        // 拖选 "hello"(内容贴底在屏幕第 6 行,列 0..5),松开自动复制
        app.on_mouse(MouseAction::Down { col: 0, row: 6, extend: false }).unwrap();
        app.on_mouse(MouseAction::Drag { col: 4, row: 6 }).unwrap();
        app.on_mouse(MouseAction::Up { col: 4, row: 6 }).unwrap();
        let up_frame = sink.take();
        assert!(
            up_frame.contains("\x1b]52;c;aGVsbG8=\x07"),
            "松开应复制 hello: {up_frame:?}"
        );
        // 高亮:选中段带 REVERSED(0;7m),行尾段还原
        app.render(&[line("tail")], None).unwrap();
        let hl = sink.take();
        assert!(hl.contains("\x1b[0;7mhello"), "{hl:?}");
        assert!(hl.contains("\x1b[0m world"), "{hl:?}");

        // 换选尾部行:松开复制尾部文本(last_tail_cells 提取)
        app.on_mouse(MouseAction::Down { col: 0, row: 7, extend: false }).unwrap();
        app.on_mouse(MouseAction::Up { col: 3, row: 7 }).unwrap();
        let tail_copy = sink.take();
        assert!(
            tail_copy.contains("\x1b]52;c;dGFpbA==\x07"),
            "应复制 tail: {tail_copy:?}"
        );

        // 单击(按下与抬起同点)不复制,且清除上一轮选区高亮
        app.on_mouse(MouseAction::Down { col: 2, row: 2, extend: false }).unwrap();
        app.on_mouse(MouseAction::Up { col: 2, row: 2 }).unwrap();
        let click = sink.take();
        assert!(!click.contains("\x1b]52;"), "单击不应复制: {click:?}");
        app.render(&[line("tail")], None).unwrap();
        assert!(
            !sink.take().contains("\x1b[0;7m"),
            "单击后高亮应清除"
        );
    }

    #[test]
    fn fullscreen_selection_follows_scroll_and_shift_extends() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        app.set_auto_copy_on_select(true);
        let lines: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        let mut sim = ScreenSim::new(40, 8);
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        // 拖选 line14(屏幕第 0 行,committed 逻辑行 13)
        app.on_mouse(MouseAction::Down { col: 0, row: 0, extend: false }).unwrap();
        app.on_mouse(MouseAction::Up { col: 5, row: 0 }).unwrap();
        let copied = sink.take();
        assert!(
            copied.contains("\x1b]52;c;bGluZTE0\x07"),
            "应复制 line14: {copied:?}"
        );
        // 翻页:高亮跟随文字移到屏幕第 4 行(视口顶 13→9),不再停留原位
        app.scroll_page_up();
        app.render(&[line("tail")], None).unwrap();
        let frame = sink.take();
        sim.feed(&frame);
        let texts = sim.texts();
        assert_eq!(texts[4].trim_end(), "line14", "{texts:?}");
        assert!(frame.contains("\x1b[0;7mline14"), "高亮应在新位置: {frame:?}");
        // 再复制:锚定内容,文本不变
        assert!(app.copy_selection().unwrap());
        assert!(sink.take().contains("\x1b]52;c;bGluZTE0\x07"));
        // Shift+点击屏幕第 5 行(frame 14)第 2 列:扩展选区(锚点不变)
        app.on_mouse(MouseAction::Down { col: 2, row: 5, extend: true }).unwrap();
        app.on_mouse(MouseAction::Up { col: 2, row: 5 }).unwrap();
        let extended = sink.take();
        let expected = crate::selection::osc52_clipboard("line14\nlin");
        assert!(
            extended.contains(&expected),
            "Shift 应扩展选区到 line14+lin: {extended:?}"
        );
    }

    #[test]
    fn fullscreen_selection_works_without_auto_copy() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        app.append_committed(&[line("hello world")]);
        app.render(&[line("tail")], None).unwrap();
        sink.take();
        // 自动复制关闭:拖选仍然高亮,松开不写剪贴板
        app.on_mouse(MouseAction::Down { col: 0, row: 6, extend: false }).unwrap();
        app.on_mouse(MouseAction::Up { col: 4, row: 6 }).unwrap();
        let up = sink.take();
        assert!(!up.contains("\x1b]52;"), "自动复制关闭时松开不应复制: {up:?}");
        app.render(&[line("tail")], None).unwrap();
        let hl = sink.take();
        assert!(hl.contains("\x1b[0;7mhello"), "选区高亮应不受开关影响: {hl:?}");
        // 快捷键复制(copy_selection)不受开关影响
        assert!(app.copy_selection().unwrap());
        assert!(sink.take().contains("\x1b]52;c;aGVsbG8=\x07"));
    }

    #[test]
    fn fullscreen_toast_overlays_top_right_on_copy() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        app.set_auto_copy_on_select(true);
        app.append_committed(&[line("hello world")]);
        app.render(&[line("tail")], None).unwrap();
        sink.take();
        app.on_mouse(MouseAction::Down { col: 0, row: 6, extend: false }).unwrap();
        app.on_mouse(MouseAction::Up { col: 4, row: 6 }).unwrap();
        // 复制成功即置 toast;渲染:第 0 行右端出现反色 " Copied! "
        assert!(app.toast_deadline().is_some());
        app.render(&[line("tail")], None).unwrap();
        let frame = sink.take();
        assert!(frame.contains("\x1b[0;7m Copied! "), "{frame:?}");
    }

    #[test]
    fn fullscreen_toast_disappears_after_expiry() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        app.append_committed(&[line("doc")]);
        app.render(&[line("tail")], None).unwrap();
        sink.take();
        assert!(app.copy_selection().is_err() || true);
        // 直接显示 toast 再回拨到期时刻,验证过期后差分消除
        app.show_toast("Copied!");
        app.render(&[line("tail")], None).unwrap();
        assert!(sink.take().contains(" Copied! "));
        app.set_toast_expiry_for_test(std::time::Instant::now() - std::time::Duration::from_millis(1));
        app.render(&[line("tail")], None).unwrap();
        assert!(!sink.take().contains(" Copied! "), "过期后应消失");
        assert!(app.toast_deadline().is_none(), "过期应清除 toast 状态");
    }

    #[test]
    fn regular_mode_ignores_mouse_gestures() {
        let (mut app, sink, _size) = app(40, 8);
        app.append_committed(&[line("doc")]);
        app.render(&[line("tail")], None).unwrap();
        sink.take();
        app.on_mouse(MouseAction::Down { col: 0, row: 0, extend: false }).unwrap();
        app.on_mouse(MouseAction::Up { col: 3, row: 0 }).unwrap();
        assert!(!sink.take().contains("\x1b]52;"), "regular 不捕获鼠标");
    }

    #[test]
    fn fullscreen_tail_only_change_rewrites_tail_region() {
        let (mut app, sink, _size) = app_fullscreen(40, 10);
        app.append_committed(&[line("committed-line")]);
        app.render(&[line("tail-a")], None).unwrap();
        sink.take();
        // 只改尾部:历史行不重写
        app.render(&[line("tail-b")], None).unwrap();
        let frame = sink.take();
        assert!(frame.contains("tail-b"), "{frame:?}");
        assert!(!frame.contains("committed-line"), "{frame:?}");
        // 尾部不变:无写入
        app.render(&[line("tail-b")], None).unwrap();
        let frame = sink.take();
        assert!(!frame.contains("tail-b"), "无变化帧不应重写: {frame:?}");
    }

    #[test]
    fn fullscreen_finish_dumps_transcript_to_main_screen() {
        let (mut app, sink, _size) = app_fullscreen(40, 10);
        app.append_committed(&[line("keep-me"), line("and-me")]);
        app.render(&[line("tail")], None).unwrap();
        app.finish().unwrap();
        let all = sink.take();
        assert!(all.contains(ALT_EXIT), "收尾应退出 alternate screen");
        assert!(all.contains(MOUSE_OFF), "收尾应关闭鼠标捕获");
        assert!(all.contains(AUTO_WRAP_ON));
        let mut sim = ScreenSim::new(40, 10);
        sim.feed(&all);
        // dump 回主屏:转录内容在 1049l 之后的网格中可见
        assert!(sim.find("keep-me").is_some(), "{:?}", sim.texts());
        assert!(sim.find("and-me").is_some(), "{:?}", sim.texts());
        assert!(!sim.texts().iter().any(|t| t.contains("tail")), "{:?}", sim.texts());
    }

    #[test]
    fn fullscreen_suspend_resume_rewrites_screen() {
        let (mut app, sink, _size) = app_fullscreen(40, 8);
        let lines: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        let mut sim = ScreenSim::new(40, 8);
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        // 挂起:退出 alt screen 让屏($EDITOR 继承主屏);sim 网格随 1049l 清空
        app.suspend().unwrap();
        let suspended = sink.take();
        assert!(suspended.contains(ALT_EXIT), "{suspended:?}");
        assert!(suspended.contains(MOUSE_OFF), "{suspended:?}");
        // 恢复:重进 alt screen + 全量重绘(整窗重写,含尾部)
        app.resume(&lines).unwrap();
        let resumed = sink.take();
        assert!(resumed.contains(ALT_ENTER), "{resumed:?}");
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        let texts = sim.texts();
        assert_eq!(texts[0].trim_end(), "line14", "{texts:?}");
        assert_eq!(texts[7].trim_end(), "tail", "{texts:?}");
    }

    #[test]
    fn set_fullscreen_switches_modes_at_runtime() {
        let (mut app, sink, _size) = app(40, 8);
        let lines: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.append_committed(&lines);
        let mut sim = ScreenSim::new(40, 8);
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        assert!(!app.is_fullscreen());
        // 切全屏:进入 1049(sim 清屏),重画后历史窗口 + 钉底尾部
        app.set_fullscreen(true).unwrap();
        assert!(sink.take().contains(ALT_ENTER));
        let document: Vec<UiLine> = (1..=20).map(|i| line(&format!("line{i}"))).collect();
        app.redraw_all(&document).unwrap();
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        let texts = sim.texts();
        assert_eq!(texts[7].trim_end(), "tail", "{texts:?}");
        assert_eq!(texts[0].trim_end(), "line14", "{texts:?}");
        // 切回 regular:退出 1049,重画后恢复终端自然滚动 + 贴底锚定语义
        // (短文档:内容贴底、顶部留白 —— fullscreen 下是顶对齐,语义可区分)
        app.set_fullscreen(false).unwrap();
        assert!(sink.take().contains(ALT_EXIT));
        let short: Vec<UiLine> = (1..=6).map(|i| line(&format!("s{i}"))).collect();
        app.redraw_all(&short).unwrap();
        app.render(&[line("tail")], None).unwrap();
        sim.feed(&sink.take());
        let texts = sim.texts();
        assert!(texts[0].trim().is_empty(), "贴底锚定顶部应留白: {texts:?}");
        assert_eq!(texts[6].trim_end(), "s6", "{texts:?}");
        assert_eq!(texts[7].trim_end(), "tail", "{texts:?}");
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
