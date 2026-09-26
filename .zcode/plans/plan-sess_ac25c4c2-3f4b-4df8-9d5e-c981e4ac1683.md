# TUI 主题系统重构：接入 ratatui-themes + 简约视觉升级

## 现状结论

项目已有质量很高的语义化主题系统（`crates/rpi-tui/src/theme.rs`，24 个语义角色字段，调用方全部经 `theme.xxx` 取色），硬编码颜色仅剩 3 处（`replay.rs`）。本次工作的核心是：**把主题源从「两套内置调色板」升级为「ratatui-themes 15 主题 + 统一映射 + 精选微调」**，并做轻量视觉重构。

已确认的决策：精选 + 统一映射；settings.json + `/theme` 命令 + `--theme` 参数三层切换；轻量视觉重构；默认主题用 crate 的 Tokyo Night。

## 1. 主题模块重构（逻辑与主题分离）

`crates/rpi-tui/src/theme.rs` 拆为 `theme/` 模块，按你的要求把「角色定义/切换逻辑」与「调色板数据」分开：

- **`theme/mod.rs`** — 语义 `Theme` 结构体（角色接口，即"逻辑"）：现有 24 个角色字段保留，新增 `is_dark: bool`；解析逻辑 `resolve(name: &str, truecolor: bool) -> Theme`（未知名字 → 回退默认）；保留 `dark()` / `dark_ansi()` 构造器（现有单测与 16 色终端兜底依赖它们）。
- **`theme/builtin.rs`** — 现有 `dark()` / `dark_ansi()` 调色板数据迁入。
- **`theme/external.rs`** — ratatui-themes 映射层：`fn from_external(name: ThemeName) -> Theme`。
  - **统一映射规则**：从 `ThemePalette`（accent/error/warning/muted/bg 等）映射同名角色；`dim`/`border_*` 等缺失角色由 `muted`/`bg` 按 is_dark 明暗推导；`tool_success`/`md_*` 等由语义近似色推导。
  - **精选微调表**：对 Tokyo Night、Catppuccin Mocha、Dracula、Nord、Rosé Pine 5 个主题手工覆盖个别角色（user_bg、tool_output 等），其余 10 个主题走统一规则。
  - 所有 15 个主题都暴露，名称与 crate 的 serde kebab-case 一致（`"tokyo-night"` 等）。
- **依赖落点**：`ratatui-themes = "0.3"` 加到 `crates/rpi-tui/Cargo.toml`（rpi-tui 本就依赖 ratatui，是纯 UI crate，方向正确）。⚠️ 需先验证它与 ratatui 0.30.2 的兼容性（`cargo tree`）；若其锁的是 ratatui 0.29 导致 `Color` 类型不一致，就在映射边界提取 RGB 分量重建 `Color::Rgb` 隔离差异。

## 2. 高亮跟随主题（highlight.rs）

`Highlighter::shared()` 目前固定 base16-ocean.dark。改为按 `theme.is_dark` 选择 syntect 主题：深色 → base16-ocean.dark，浅色（Catppuccin Latte 等）→ InspiredGitHub。保持惰性单例（两个实例）。

## 3. 主题选择链路（三层优先级：/theme > --theme > settings.json）

- **`rpi-core/src/config.rs`**：settings.json 新增可选 `"theme": "<kebab-case>"` 字段，只存字符串（rpi-core 不依赖 rpi-tui，保持分层）。
- **`rpi-cli` 参数解析**（main.rs 现有手写风格）：加 `--theme <name>`，装配时传入。
- **`interactive/mod.rs`**：解析顺序 `--theme` → settings.json → 自动探测（16 色终端仍降级 `dark_ansi()`，真彩色默认 Tokyo Night）。
- **`interactive/handlers.rs`**：新增 `/theme` 命令——无参数列出全部主题并标注当前项；带参数切换（更新 `state.theme` 后触发 `redraw_full()` 重绘转录）。会话内生效，持久化写 settings.json。

## 4. 清理硬编码

- `interactive/replay.rs` 3 处 `Color::DarkGray`/`Color::Red` → 改用 theme 角色。
- `highlight.rs` 的 syntect→ratatui 转换保留（结构性必要）。

## 5. 轻量视觉重构（简约高端）

改动集中在 `view.rs` / `header.rs` / `footer.rs` / `markdown.rs`，不动信息架构：

- **圆角统一**：编辑器框 `┌─`/`└` → `╭─`/`╰`（markdown 代码块已是圆角风格，编辑器对齐）。
- **弱化非焦点元素**：分隔线、footer、键位提示统一用更克制的 dim 层级，减少 BOLD 滥用；横幅 "rpi" 保留 accent。
- **user 消息块更含蓄**：bg 从硬编码灰改为由主题 bg 推导的低饱和底色。
- **留白节奏**：转录条目间空行规则统一。
- **约束**：`❯`、`⏺`、`✻` 等符号不变；动手前先 grep `tests/e2e/*.py` 是否断言了边框字符（┌/└），若有则同步更新断言。

## 6. 测试与验收

- **单测**：15 个主题映射测试（每个角色非退化、is_dark 正确）；is_dark 驱动 syntect 主题选择；ANSI 兜底路径。现有用 `Theme::dark_ansi()` 的测试不动。
- **e2e**（按 `tests/e2e/README.md` 流程）：新增 `test_theme.py`——`/theme` 列出主题、`/theme nord` 切换后 UI 正常工作（用 pyte 屏幕属性断言颜色变化）；跑全量 `pytest -v` 验证既有场景不回归。
- **验收命令**：`cargo test --workspace` + `cargo build --bin rpi` + `cd tests/e2e && pytest -v`。

## 实施顺序

1. 验证 ratatui-themes 兼容性 → 2. theme/ 模块拆分 + 映射层 → 3. highlight/replay 跟随主题 → 4. 配置与三层切换链路 + /theme 命令 → 5. 视觉重构 → 6. 测试补全与全量回归。