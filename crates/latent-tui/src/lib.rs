//! latent-tui —— ratatui 为底的终端 UI 组件库(08 文档 §3,2026-09 起从零依赖
//! 手写渲染迁移到 ratatui/crossterm;依赖方向约束不变:**不知道 agent 的
//! 存在**,无内部 crate 依赖,整体可拆卸)。
//!
//! 本 crate 提供四样东西(方针 §2 三规则):
//!
//! 1. **工厂**:`TuiApp::open()` 出厂全帧差分终端屏幕;
//! 2. **语义主题**:`Theme`(角色 → 颜色,真彩色/ANSI16 双调色板);
//! 3. **组件**:多行 `Editor`、`Markdown`(syntect 高亮)、`SelectList`、
//!    `CommandPopup`(斜杠补全)、`FilePopup`(`@` 文件选择)、
//!    `tool_card`/`header`/`footer`、`loader`、`popup`(弹窗圆角外框);
//! 4. **类型**:`Key`(crossterm 事件归一)、`text`(span 感知折行工具)。
//!
//! 渲染模型(pi TuiMainScreen 对应):**全帧行级差分** —— 已定稿行缓存
//! ANSI 序列化结果只追加,活动尾部每帧重建;两段拼成全帧与上一帧逐行
//! 差分,只重绘变化区间,追加行越过屏幕底时自然滚入原生 scrollback
//! (`redraw_all` 支持全文重绘,用于 ctrl+o/主题/尺寸变化)。
//!
//! 测试策略:全部组件为纯函数(状态 → `Vec<Line>`),直接单测;终端 I/O
//! 集中在 `app.rs`,用内存输出汇 + ANSI 屏幕模拟器验证结构性不变量。

pub mod app;
pub mod command_popup;
pub mod editor;
pub mod file_popup;
pub mod footer;
pub mod header;
pub mod highlight;
pub mod key;
pub mod loader;
pub mod markdown;
pub mod popup;
pub mod select_list;
pub mod selection;
pub mod text;
pub mod theme;
pub mod tool_card;
pub mod width;

pub use app::{SCROLL_TO_END_LABEL, SharedBuf, SharedSize, TuiApp};
pub use command_popup::{CommandEntry, CommandPopup};
pub use editor::{Editor, EditorView};
pub use file_popup::{FileEntry, FilePopup};
pub use footer::{ctx_segment, FooterData};
pub use header as header_view;
pub use highlight::Highlighter;
pub use key::{from_event, normalize_native_enter, Key};
pub use loader as loader_view;
pub use markdown::Markdown;
pub use select_list::SelectList;
pub use selection::{MouseAction, SelPoint};
pub use theme::{Theme, ThemeName};
pub use width::{char_width, display_width, truncate_to_width, wrap_to_width};

/// UI 行类型(静态生命周期,组件纯函数的输出)。
pub type UiLine = ratatui::text::Line<'static>;
