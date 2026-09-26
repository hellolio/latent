//! ANSI 转义序列常量(08 文档:CSI 2026 同步输出 + 括号粘贴)。

/// 同步输出开始(`CSI ? 2026 h`):终端把此区间内的更新一次性上屏,防闪烁。
pub const SYNC_START: &str = "\x1b[?2026h";
/// 同步输出结束(`CSI ? 2026 l`)。
pub const SYNC_END: &str = "\x1b[?2026l";

/// 启用括号粘贴模式(`CSI ? 2004 h`):粘贴的文本以 `ESC[200~`/`ESC[201~`
/// 包裹,编辑器可整体处理而不是逐字符解释。
pub const BRACKETED_PASTE_ON: &str = "\x1b[?2004h";
pub const BRACKETED_PASTE_OFF: &str = "\x1b[?2004l";

/// 光标上移 n 行。
pub fn cursor_up(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("\x1b[{n}A")
    }
}

/// 光标下移 n 行。
pub fn cursor_down(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("\x1b[{n}B")
    }
}

/// 回行首。
pub const CR: &str = "\r";

/// 清除从光标到行尾。
pub const CLEAR_TO_EOL: &str = "\x1b[K";

/// 清除从光标到屏幕末尾(含整行)。
pub const CLEAR_TO_SCREEN_END: &str = "\x1b[0J";

/// 隐藏/显示光标。
pub const HIDE_CURSOR: &str = "\x1b[?25l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";

/// 常用 SGR 样式(组件渲染用;终端不支持时原样输出,可接受)。
pub mod style {
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const ITALIC: &str = "\x1b[3m";
    pub const UNDERLINE: &str = "\x1b[4m";
    pub const REVERSE: &str = "\x1b[7m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const CYAN: &str = "\x1b[36m";
    pub const RESET: &str = "\x1b[0m";
}
