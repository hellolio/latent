//! rpi-tui —— ratatui 为底的终端 UI 组件库(08 文档 §3,2026-09 起从零依赖
//! 手写渲染迁移到 ratatui/crossterm;依赖方向约束不变:**不知道 agent 的
//! 存在**,无内部 crate 依赖,整体可拆卸)。
//!
//! 本 crate 提供四样东西(方针 §2 三规则):
//!
//! 1. **工厂**:`TuiApp::open()` 出厂 Inline 视口终端应用;
//! 2. **语义主题**:`Theme`(角色 → 颜色,真彩色/ANSI16 双调色板);
//! 3. **组件**:多行 `Editor`、`Markdown`(syntect 高亮)、`SelectList`、
//!    `tool_card`/`header`/`footer`、`loader`;
//! 4. **类型**:`Key`(crossterm 事件归一)、`text`(span 感知折行工具)。
//!
//! 渲染模型(pi TuiMainScreen 对应):**Inline 视口** —— 定稿内容经
//! `commit_lines` 插入视口上方、滚入终端原生 scrollback(保留回滚);
//! 屏幕底部固定视口承载编辑器/状态栏/选择列表,每帧重绘。视口高度随内容
//! 动态调整(`set_viewport_height`),`redraw_full` 支持全文重绘(ctrl+o)。
//!
//! 测试策略:全部组件为纯函数(状态 → `Vec<Line>`),直接单测;终端 I/O
//! 集中在 `app.rs`,用 TestBackend 验证结构性不变量。

pub mod app;
pub mod editor;
pub mod footer;
pub mod header;
pub mod highlight;
pub mod key;
pub mod loader;
pub mod markdown;
pub mod select_list;
pub mod text;
pub mod theme;
pub mod tool_card;
pub mod width;

pub use app::{reader_checkpoint, TuiApp};
pub use editor::{Editor, EditorView};
pub use footer::{ctx_segment, FooterData};
pub use header as header_view;
pub use highlight::Highlighter;
pub use key::{from_event, Key};
pub use loader as loader_view;
pub use markdown::Markdown;
pub use select_list::SelectList;
pub use theme::Theme;
pub use width::{char_width, display_width, truncate_to_width, wrap_to_width};

/// UI 行类型(静态生命周期,组件纯函数的输出)。
pub type UiLine = ratatui::text::Line<'static>;
