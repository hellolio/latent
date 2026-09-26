//! rpi-tui —— 零依赖差分渲染终端 UI 库(08 文档 §3)。
//!
//! 与 pi 的 packages/tui 同构的定位:**不知道 agent/coding-agent 的存在**,
//! 无任何外部 crate 依赖,整体可拆卸。本 crate 提供四样东西(方针 §2 三规则):
//!
//! 1. **工厂**:`create_main_screen_tui()` 出厂主缓冲渲染器;
//! 2. **trait**:`Tui`(滚动提交 + 视口差分重绘 + 收尾)、`Component`(`render(width)` 纯函数);
//! 3. **类型**:`Key`/`KeyParser`、`Editor`、`Text`/`Markdown`/`SelectList`、`RowChange`。
//!
//! 渲染模型(08 文档 §3):**差分渲染**(视口内只重画变化行)+ **CSI 2026
//! 同步输出**(防闪烁)+ **括号粘贴模式**。主缓冲实现保留终端 scrollback:
//! 定稿内容经 `commit_lines` 追加进 scrollback(只打印一次),视口固定占据
//! 终端底部 `viewport_height` 行(状态行 + 小部件 + 输入行),每次更新按行
//! diff、仅重写变化的行。退出时 `finish` 清掉视口,最终文档留在 scrollback。
//!
//! 测试策略:diff/宽度/按键/编辑器/组件全部纯函数化,直接单测;终端 I/O
//! (`Terminal`)是唯一 sidecar,`Tui` 实现对 `Write` 泛型,可用内存缓冲验证
//! 输出的字节流。

pub mod ansi;
pub mod components;
pub mod diff;
pub mod editor;
pub mod keys;
pub mod screen;
pub mod terminal;
pub mod width;

pub use components::{Component, Markdown, SelectList, Text};
pub use diff::{diff_rows, RowChange};
pub use editor::Editor;
pub use keys::{matches_key, Key, KeyParser};
pub use screen::{create_main_screen_tui, MainScreenTui, Tui, DEFAULT_VIEWPORT_HEIGHT};
pub use terminal::Terminal;
pub use width::{char_width, display_width, truncate_to_width, wrap_to_width};
