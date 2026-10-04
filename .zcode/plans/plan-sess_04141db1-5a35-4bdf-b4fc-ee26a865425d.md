# 复制快捷键固定 Ctrl+X + "Copied!" 右上角提示框

## 需求
1. **复制快捷键固定为 Ctrl+X（有选区时）**，不再可切换——/setting 里原"复制快捷键"循环项改为"**Ctrl+X 复制：开/关**"开关（默认开）。关闭后 Ctrl+X 无复制行为。选 Ctrl+X 是因为 Ctrl+X 目前无任何绑定，与 Ctrl+C 的中断/双击退出、Esc 中止零冲突，且 Ctrl 组合键任何终端都转发（避开 WezTerm 截留 cmd 键的问题）。
2. **任何成功复制**（Ctrl+X 复制、或"选中后自动复制"触发）都在**屏幕右上角短暂显示提示框 `Copied!`**（反色小标签，约 1.5 秒自动消失），**取代**目前落在转录区的"已复制选区到剪贴板"提示行。

## 改动

### 1. 移除可配置快捷键（latent-tui / latent-cli 简化）
- `key.rs`：删除 `CopyShortcut` 枚举与 `from_event_with`（恢复 `from_event` 单入口）；删除 `Key::CopySelection` 变体及 cmd+X/cmd+C/Ctrl+Shift+C 映射（cmd 组合键在 WezTerm 等终端不可靠）。
- `state.rs`：删 `copy_shortcut` 字段；新增 `ctrl_x_copy: bool`（默认 true）。
- `mod.rs`：删 `Arc<RwLock<CopyShortcut>>` 与键盘线程注入；`InteractiveCtx` 删 `copy_shortcut` 字段（tests.rs 同步）；启动读取新设置。
- `handlers.rs` + `state.rs`：`/setting` 三项变为：全屏模式开关 / **Ctrl+X 复制开关** / 选中后自动复制开关；持久化键 `tuiMode` / `ctrlXCopy`（新，默认 true）/ `copyOnSelect`（旧 `copyShortcut` 不再读写）。
- `latent-core/config.rs`：`load_copy_shortcut_setting` → `load_ctrl_x_copy_setting`；测试更新。

### 2. Ctrl+X 复制接入（mod.rs 事件循环）
新增拦截：`Key::Ctrl('x')` 且 `app.has_selection()` 且 `state.ctrl_x_copy` 开 → 复制 + 清选区 + toast；否则原样落编辑器（当前即为无操作）。Ctrl+C 恢复纯中断/双击退出（移除上一版的有选区复制分支）。

### 3. "Copied!" 右上角 toast（latent-tui/app.rs）
- `TuiApp` 新增 `toast: Option<(String, Instant)>`（默认 1.5s）+ `show_toast(&str)`；`copy_selection` 成功时自动触发——Ctrl+X 与自动复制两条路径都经过它，天然覆盖。
- `render_fullscreen`：toast 激活时把屏幕第 0 行右端覆盖为反色 ` Copied! ` 标签（原行按显示宽度截断拼接），走既有整屏逐行差分，出现/消失各重写一行。
- 过期驱动：事件循环 `tokio::select!` 增加 toast 到期分支（有 toast 时 sleep 到期触发重绘），空闲状态也能按时消失。
- 仅 fullscreen 渲染路径有 toast；regular 模式无选区/复制，不涉及。

### 4. 测试
- `app.rs`：toast 显示（第 0 行右端反色 `Copied!`）、过期消失、`copy_selection` 成功即置 toast。
- `config.rs`：`ctrlXCopy` 读写测试。
- key.rs：删除 CopyShortcut 相关测试，保留鼠标/扩展手势测试。
- E2E 全套回归。

### 5. 文档
README / AGENTS.md：复制快捷键固定 Ctrl+X（有选区时复制；无选区无操作）、Ctrl+C 保持中断/退出、`/setting` 三项开关、`ctrlXCopy` 设置键、Copied! toast；删除 cmd+X/复制快捷键循环描述。

## 验收
cargo test --workspace 全绿、clippy 零警告、E2E 全套通过。