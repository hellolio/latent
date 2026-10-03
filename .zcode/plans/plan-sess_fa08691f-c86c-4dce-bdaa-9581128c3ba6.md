# 全帧差分渲染重构 + 显示布局调整

## 背景与目标

现状：`rpi-tui/src/app.rs` 用 ratatui `Viewport::Inline`——定稿内容经 `insert_before` 滚入 scrollback，底部固定高度视口每帧重绘。这带来"预览高度守恒律"（busy 必须预留 `STREAM_PREVIEW_ROWS=1` 行、收缩前必须落盘、`set_viewport_height` 要插空行+重建 Terminal），导致：流式预览只有 1 行、输入框与输出间距固定为 3 行且不可调、改高/全文重绘走 hack 路径。

上游 pi（`packages/tui/src/tui-main-screen.ts`）的做法：**全帧行级差分**——维护整份文档的行数组，与上一帧渲染结果逐行比较，只重绘变化区间；追加行靠打印 `\r\n` 让终端自然滚入原生 scrollback；只有改到已滚出屏幕的历史行、宽度变化等才全量重绘。无任何高度预留约束。

## 布局需求（已确认）

尾部帧自上而下：

```
…已定稿转录…
[实时预览 ≤4 行]        ← 流式正文尾部 4 行 / thinking 尾部 4 行+提示 / 工具命令卡片(命令全显不设上限)
[状态行]                ← 仅 busy 时渲染（spinner + esc to interrupt），紧贴输出下方；idle 不占行
[空行]
[空行]                  ← 输出→输入框恒定两行间隔（流式中与完成后一致）
[补全弹窗(如有)]
[编辑器区(背景块)]
[footer 三行]
```

- 工具命令本身（`⏺ name args` 与 `! cmd`）永远完整折行显示，续行对齐参数列，不受 4 行上限与 ctrl+o 影响；ctrl+o 只作用于输出/思考折叠。
- 工具输出定稿后仍按 `COLLAPSED_OUTPUT_ROWS=4` 折叠 + 提示行（现状保持）。

## 一、rpi-tui：重写 app.rs 为全帧差分屏幕

保留 `TuiApp` 类型名，内部从 ratatui Terminal/Inline 视口改为 pi 式自管渲染（组件层仍用 ratatui 的 Line/Span/Style，只替换终端层）：

1. **状态**：`committed: Vec<String>`（定稿行的 ANSI 序列化缓存，只追加）、`previous: Vec<String>`（上一帧全帧）、`cols/rows`、`viewport_top`（屏幕顶对应的逻辑行号）、`hardware_row`、`finished`。
2. **行序列化**：新增 `Line<'static> → ANSI String`（遍历 span 输出 SGR fg/bg/修饰符，行尾 reset；折行复用 `text::wrap_line`）。直接打印文本，CJK 宽字符由终端自行推进，天然规避 ratatui buffer diff 的宽字符空位 bug（保留 E2E 回归）。
3. **API**：
   - `open()`：raw mode + 尺寸查询 + 打印 `rows-1` 个 `\r\n` 锚定底部（保留现行为）。
   - `append_committed(&[UiLine])`：折行+序列化进 `committed`，不发 I/O。
   - `render(tail: &[UiLine], cursor: Option<(col, tail_rel_row)>)`：全帧 = committed ++ tail 序列化，与 `previous` 做 pi 式 diff 后输出：
     - 求 firstChanged/lastChanged（committed 前缀不变可用上帧 committed 长度做稳定前缀加速）；
     - `firstChanged < viewport_top` → 全量重绘兜底；
     - 纯尾部追加 → 逐行打印 + `\r\n` 自然滚动；
     - 中段改写 → 移动光标逐行 `\r\x1b[2K` + 重打印，越过屏幕底时以 `\r\n` 滚动并同步 `viewport_top`；
     - 帧收缩 → 清掉残留行；光标按 `(cursor, doc_end - tail_len + rel_row - viewport_top)` 绝对定位或隐藏；
     - 每次写入包 `\x1b[?2026h/l` 同步输出（不支持的终端忽略）。
   - `redraw_all(full_doc_lines)`：清屏重打（ctrl+o / 主题 / /new / 宽度变化路径）。
   - `finish()`/`Drop`：清尾部活动区、恢复终端（语义同现在）。
4. **删除**：`Viewport::Inline`、`set_viewport_height`/`rebuild_terminal`、PAUSE/PARKED 读取线程暂停协议（不再有光标应答查询；`reader_checkpoint` 一并移除，改 mod.rs 键盘线程）。
5. **测试**：用 `Vec<u8>` 输出汇 + 一个最小 ANSI 屏幕模拟器（处理 `\r\n`/`\x1b[2K`/定位）做单测：追加自然滚动、中段改写只重绘变化区间、帧收缩清残留、宽度变化全量重绘、CJK 折行不插空格、光标定位。Cargo.toml 中 `scrolling-regions` 相关注释更新。

## 二、interactive 层调整

- **view.rs**：`viewport()` 改为组装上述尾部布局——预览区不再恒占固定行数（去掉补空行/drain），`STREAM_PREVIEW_ROWS` 改 4（正文尾部窗口取尾逻辑保留）；状态行仅 busy 渲染；固定空行 1 行改 2 行；工具参数预览从单行截断 `⚙ args` 改为完整折行的命令卡片（复用 `tool_box_top` Pending 态）；删除守恒律注释与 `MAX_PREVIEW_ROWS`；`ViewportFrame.cursor` 语义改为相对尾部首行。终端过矮时的收缩循环保留（预算改为屏幕行数，先砍预览再砍弹窗/编辑器）。
- **mod.rs**：`render_tick` 改为 `screen.append_committed(pending)` + `screen.render(tail, cursor)`；删 `build_frame`/`preview_cap_for` 的预留逻辑与 `set_viewport_height` 调用；`needs_full_redraw` 路径改走 `redraw_all`；Resize → 触发全量重绘（宽度变化需重折行）；键盘线程删 `reader_checkpoint`。
- **state.rs**：`pending`/`transcript`/`commit_many` 模型不变；`reset_for_new_session` 配合 `redraw_all` 清屏幕侧 committed 缓存。
- **handlers.rs**：提交/流式/工具配对逻辑不变（MessageStart 空行、flush_stream、pending_tools 配对均保持）。

## 三、tool_card.rs

`tool_box_top`：参数段去掉"收起时单行截断"分支，始终 `wrap_to_width` 完整折行（续行对齐参数起始列，现展开分支逻辑）；`expanded` 参数因此无用，从签名移除并更新调用方与单测。`bash_box` 命令已全显，不动。输出折叠逻辑不动。

## 四、测试与验收

- `cargo test --workspace` + `cargo clippy --workspace --all-targets` 零警告（更新 app.rs/view.rs/tool_card.rs/interactive/tests.rs 中受影响用例：`viewport_layout_shape`、`viewport_reserves_full_preview_height`、`tool_box_top_expands_args_multiline` 等）。
- E2E（`cargo build --bin rpi && cd tests/e2e && pytest`）：`test_output_display.py` 补断言（输出与 `❯` 间恰两空行、流式预览 ≤4 行、长命令工具卡片多行全显），回归 `test_ctrl_o.py`、`test_output_display.py::test_cjk_committed_without_injected_spaces`、`test_plan_mode`、`test_slash_commands` 等全部场景。
- AGENTS.md 同步更新 rpi-tui `app.rs`、interactive `view.rs` 两行架构描述（Inline 视口 → 全帧差分）。