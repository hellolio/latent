## TUI 页面 UI 修改方案

涉及文件：`crates/rpi-cli/src/modes/interactive/view.rs`（布局/渲染）、`crates/rpi-tui/src/theme/external.rs`（配色）、`crates/rpi-tui/src/footer.rs`（footer）、`crates/rpi-tui/src/loader.rs`（spinner）、`crates/rpi-cli/src/modes/interactive/mod.rs` 与 `state.rs`（session id 移除），以及各文件内对应测试。

### 1. 缩小输入框与交流区空隙（约两行）
- `view.rs:13`：`MAX_PREVIEW_ROWS` 从 `8` 改为 `2`（预览区固定补空行逻辑 view.rs:224-228 不变，空隙即变为 2 行）。
- 流式输出预览也只显示尾部 2 行，行为一致。
- `mod.rs` 的 `draw()` 收缩逻辑（mod.rs:206-228）无需改动，自动适配。
- 同步更新 `view.rs` 中依赖行数的测试（`viewport_layout_shape`、`viewport_reserves_full_preview_height` 等）。

### 2 + 3. 用户消息与模型输出字体统一为灰白色（用户消息保留灰底）
- `theme/external.rs`：`user_text` 与 `assistant_text` 目前都取 `p.fg`（Tokyo Night 偏蓝）。改为按明暗取中性色：暗色主题 → 灰白 `Rgb(0xd4,0xd4,0xd4)`，浅色主题 → 中性深灰 `Rgb(0x38,0x3a,0x42)`。两者同值即满足"模型输出与用户输入字体一致，仅差背景色"。
- 该改动同时影响 markdown 正文、编辑器文字、流式预览（均引用 `assistant_text`），整体统一。
- `theme/builtin.rs` 的 ANSI 兜底主题本来就是 White，不动。跑 `all_themes_map_without_degenerate_roles` 等主题测试确认无回归。

### 4. 去掉左下角 session id（git branch 已在显示）
- `footer.rs:50-52`：删除 `• {session}` span；`FooterData.session_label` 字段一并删除。
- `state.rs:138` 删除 `session_label` 字段；`mod.rs:124-126` 删除赋值。
- 更新 `footer.rs` 测试 `first_line_has_cwd_branch_session`（期望变为 `~/work (main)`），grep 其他引用一并清理。

### 5. 输入框改为带背景色、无边框（保留 `❯` 前缀）
- `view.rs` `viewport()`：
  - 删除顶边框（view.rs:247-250）与底边框（view.rs:284-287）两行；`border_style()`（view.rs:346-355）随之删除（busy 语义改由状态行承担）。
  - 编辑器每行改为整行铺 `theme.user_bg` 背景（与 `user_block` 同款：内容 + 行尾补空格铺满宽度），文字用 `user_text`（即灰白色），保留首行 `❯ ` 前缀（accent 色，加背景）。
  - 空输入时占位文本 "Ask rpi to do anything" 同样铺背景。
  - 光标定位（view.rs:276-281）列偏移保持 `2 + col`（前缀仍在）。
- 同步更新所有涉及 `╭`/`╰` 边框断言的测试（`viewport_layout_shape`、`viewport_shows_slash_popup_above_composer`、`viewport_empty_editor_shows_placeholder` 等）。

### 6. 字符级转圈动画 + 等待信息
- `loader.rs:4`：`FRAMES` 从星形族 `["✶","✸","✹","✺",…]` 改为等宽半圆转圈 `["◐","◓","◑","◒"]`（与字符大小相当）。
- 等待信息已在输入框正上方的状态行（`status_line()`，view.rs:317-344），保留现有格式并微调：`◐ Working (3s · esc to interrupt)`（含 spinner + 已等待秒数 + esc 提示）；工具执行时 `◐ Running {name} (Ns · esc to interrupt)`。驱动逻辑（120ms tick，mod.rs:186-195）不变。

### 验证
- `cargo test -p rpi-tui -p rpi-cli` 全绿（重点：view/footer/theme/loader 的单测）。
- `cargo build` 通过后，用户手动运行 `cargo run` 目视确认六处效果。