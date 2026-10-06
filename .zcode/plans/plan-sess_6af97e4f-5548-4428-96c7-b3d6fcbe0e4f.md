# TUI 配色语义重设计：绿主色 / 统一灰 / 黄 git 分支 / 蓝紫粉分类（信息类多色阶）

## 目标语义方案

所有主题统一"哪个角色用哪种色相"，色值取各主题色板（`ThemePalette`）对应槽位。以默认 TokyoNight 为例：

| 色系 | 来源 | 覆盖角色 |
|---|---|---|
| **绿（主色）** | `p.success` `#9ece6a` | `accent`（弹窗选中项 ❯、列表 bullet、logo、审批问题行）、`footer_cwd`、`md_code`（行内代码）；`tool_success`/`success`/`usage_output`/`border_bash` 已是绿不动 |
| **灰（次要，统一）** | 中性灰 `#8c8c8c`（浅色主题 `#767676`，即现 thinking 灰） | `thinking`、`tool_output`（已是）、`muted`、`dim` —— 文字灰全部统一；边框/卡片背景等结构性暗色保持现状 |
| **黄** | `p.warning` `#e0af68` | **git 分支**（调用点改 `warning`）；`warning`/`tool_pending`/`md_heading`/`usage_cost` 已是黄 |
| **蓝（信息主色）** | `p.info` `#7dcfff` | `tool_title`、`usage_input`（已是）、`md_link`（现 `p.accent`→改） |
| **蓝紫（信息第二色阶）** | `blend(p.info, p.secondary, 0.5)` `#9cb5fb` | `usage_ctx`、`usage_reasoning`（现 `p.accent`→改）—— 与 ↑prompt 同属冷色信息家族但肉眼可区分 |
| **紫（扩展/活跃态）** | `p.secondary` `#bb9af7` | `subagent`/`spinner`/`usage_cache`/`border_busy`/`plan_bg` 已全是紫不动 |
| **粉（会话模式）** | 逐主题官方粉 | `mode_plan` 保持逐主题 refine 覆盖不动 |

## Footer 最终效果（TokyoNight）

```
~/work (main)                        ↑ 12k (U 1k/R 11k·92%) │ ↓ 3.4k │ ctx 34% │ $0.42
 绿      黄                            蓝青                  绿      蓝紫     黄
agent:main │ plan · ⏷2 subagent                  gpt-x · thinking:off
 灰           粉       黄                          绿          灰
```

token 段四指标 蓝青/绿/蓝紫/黄 两两可区分；ctx% 阈值变色（>70% 黄、>90% 红）逻辑不变；状态行渐变锚点自动变为 蓝→紫→绿。单回合用量行：TPS 绿（同 ↓）、TTFT 蓝（同 ↑）、reasoning 蓝紫。

## 改动清单

### 1. `crates/latent-tui/src/theme/external.rs` — 统一推导（核心）

`from_name()`（:38-96）改 8 个字段：
- `accent: p.accent` → `p.success`；`footer_cwd: p.accent` → `p.success`
- `md_code: blend(p.warning, p.error, 0.45)` → `p.success`
- `md_link: p.accent` → `p.info`
- `muted: p.muted` → `thinking`（复用 ：33-37 中性灰局部变量）；`dim: blend(...)` → `thinking`
- `usage_ctx: p.accent` → `blend(p.info, p.secondary, 0.5)`；`usage_reasoning: p.accent` → 同值
- `usage_input`/`usage_output`/`usage_cache`/`usage_cost`/`tool_title`/`subagent`/`spinner` 保持现状

`refine()`（:103-266）精简：
- **删除**：TokyoNight 的 `accent` 橙、`footer_cwd`、`md_code` 橙、`dim #7e8597`；全部主题的 `md_code`、`usage_input`、`usage_cache` 覆盖（收回统一推导）
- **保留**：各主题 `user_bg`、`border_idle`、`md_code_block_border=border_idle`、`popup_border`（结构性）、`mode_plan` 官方粉、TokyoNight `subagent` 紫
- 更新 ：1-9 过时文件头注释（"五个主题微调"→新分层描述）

### 2. `crates/latent-tui/src/theme/builtin.rs` — 两套兜底对齐

`dark()`（真彩色兜底，镜像 TokyoNight 推导值）：
- `accent`/`footer_cwd`/`md_code`：`#ff9e64` → `#9ece6a` 绿
- `md_link`：`#7aa2f7` → `#7dcfff`（=p.info，与 tool_title/usage_input 同为信息主色）
- `usage_ctx`/`usage_reasoning`：`#7aa2f7` → `#9cb5fb` 蓝紫
- `muted` `#969eb6` → `#8c8c8c`；`dim` `#7e8597` → `#8c8c8c`
- 其余不变（`usage_input #7dcfff`/`tool_title #7dcfff`/`usage_cache #bb9af7` 等已是目标值）

`dark_ansi()`（16 色降级）：
- `accent`/`footer_cwd`：Yellow → Green；`md_code`：Magenta → Green
- `muted`/`dim`：Gray → DarkGray（与 `thinking` 统一）
- `usage_input` Cyan / `usage_ctx` Blue / `md_link` Blue / `tool_title` Cyan / `subagent` Cyan / `mode_plan` Magenta 保留（16 色下已是"input 与 ctx 可区分"的合理降级）；注释同步

### 3. `crates/latent-tui/src/footer.rs` — git 分支改黄（唯一调用点改动）

- :56 `Style::new().fg(theme.success)` → `theme.warning`；:380 注释、:386 单测断言 `success` → `warning`

### 4. 测试更新与新增（external.rs `#[cfg(test)]`）

- `curated_overrides_apply`：`md_code` 断言 橙 `#ff9e64` → 绿 `#9ece6a`（border_idle/user_bg 断言保留）
- `themes_are_visually_distinct`：md_code 现=p.success，user_bg 逐主题互异仍保证通过，跑一遍确认
- **新增** `semantic_scheme_is_unified`：遍历全部主题钉死语义映射——绿系 `accent==success==footer_cwd==md_code`；灰系 `muted==dim==thinking==tool_output`；蓝系 `md_link==tool_title==usage_input`；紫系 `subagent==spinner==usage_cache`；`usage_ctx==usage_reasoning` 且 `!= usage_input`；并断言 token 段渲染四色 `usage_input/usage_output/usage_ctx/usage_cost` 两两互异 + `mode_plan != subagent`（粉紫区分），防退化调色板与后续微调污染

### 5. 验证（仓库硬门槛）

- `cargo test --workspace` 全绿 + `cargo clippy --workspace --all-targets` 零警告
- 手动目测 `cargo run -p latent-cli -- --mock "你好"`：footer cwd 绿/分支黄/`agent:main`·`thinking:off`·键位提示统一灰/行内代码绿/弹窗选中项绿/token 段四色
- 可选冒烟：`cd tests/e2e && pytest test_theme.py`（E2E 无色彩断言，预计不受影响）

## 不改的部分

- 结构性颜色（`user_bg`、工具卡片三背景、`plan_bg`、`popup_border`、`md_code_block_border`、`border_idle`）与 `user_text`/`assistant_text` 亮灰保持现状
- toast/选区 `REVERSED` 反色机制不动
- 蓝/紫/粉全部复用现有 `Theme` 字段，**不新增结构体字段**；UI 调用点除 footer git 分支一行外零改动
