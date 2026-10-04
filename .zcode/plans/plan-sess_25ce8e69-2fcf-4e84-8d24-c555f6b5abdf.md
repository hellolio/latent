# TUI 界面六项改进方案

## 背景分析(为什么现状是这样)

探索确认的根因:

- **主题雷同(问题1)**:15 个外部主题全部走同一套映射规则(`crates/latent-tui/src/theme/external.rs:17-48`)——`assistant_text = user_text = p.fg`、`thinking = p.muted`、dim/border/user_bg 全部用同一个 `blend()` 公式从 bg/fg 推导,只有 5 个主题有精选覆盖。所以换主题只是换了 fg/accent 色相,整体观感几乎一样。
- **颜色种类少(问题6)**:footer 全部用 `dim` 一色(`footer.rs:36-91`)、弹窗边框用 `dim`、提示行用 `dim`——多个区域共用同一角色。
- **输入框闪烁(问题4)**:流式期间预览区行数在 1~8 行间变化(thinking 3行 → 正文涨到8行 → 工具参数1行),每次高度变化走 `set_viewport_height`(`app.rs:223-238`)→ `insert_before` + `Clear(FromCursorDown)` + Terminal 重建,整个视口(含输入框)被清屏重画。输入框内容本身没变,是高度抖动把它一起刷掉了。
- **两次回车(问题3)**:`handlers.rs:64-70`,弹窗可见时 Enter 只做"补全文本进输入框",不是执行。
- **token 在左下(问题2)**:`footer.rs:50-76` 把 token/cost/ctx% 放在第二行左段。
- **排版松散(问题5)**:assistant 消息定稿后固定追加空行(`handlers.rs:623`),加上 markdown 段间空行,视觉上很稀。

## 改动方案

### 1. Footer 重构(问题2)— `crates/latent-tui/src/footer.rs` + `view.rs`

- `FooterData` 新增 `cache_read: u64`、`cache_write: u64`(来自 `state.usage.total`,view.rs:282-295 填充处同步)。
- `footer::lines()` 改为 **3 行**:
  - 第1行:cwd (branch) • session(不变);
  - 第2行:**右对齐** token 段:`↑ {in} │ ↓ {out} │ cache {命中率}% │ ctx {pct}% ({used}/{win}) │ ${cost}`,命中率 = `cache_read / (input + cache_read)`,分母为 0 时省略该段;ctx% 保留 >70 warning / >90 error 阈值,context_window 未知时省略 ctx 段;
  - 第3行:**右对齐** `{model} · t:{thinking}`(模型名位置不动,与现在一致)。
- 每段一个颜色(见第4点的新主题角色),分隔符 `│` 用 dim。

### 2. 主题差异化 + 颜色区分(问题1+6)— `theme/mod.rs`、`external.rs`、`builtin.rs`

- `Theme` 结构新增角色:`usage_input`、`usage_output`、`usage_cache`、`usage_cost`(footer token 段)、`popup_border`(弹窗边框)。
- `external.rs` 统一映射改为分散使用调色板的不同字段:`usage_input = p.info`、`usage_output = p.success`、`usage_cache = p.secondary`、`usage_cost = p.warning`、`popup_border = blend(bg, fg, 0.35)`。
- `refine()` 从 5 个主题扩展到**全部 15 个主题**:每个主题用各自官方色板覆盖 `user_bg` / `border_idle` / `md_code` / `popup_border` 等低饱和角色,让不同主题观感差异明显(如 Dracula 用 selection 紫、Nord 用 polar night、Gruvbox 用 bg 系)。
- `builtin.rs` 的 `dark_ansi()` 同步补新角色(ANSI 基本色映射)。
- 弹窗边框(`command_popup.rs:132`)从 `dim` 换成 `popup_border`。
- 更新 `external.rs` 的主题映射测试(新角色非退化断言)。

### 3. Enter 直达执行(问题3)— `handlers.rs:54-77`

- 弹窗可见时:**Enter = 补全并直接执行**当前选中项(取 `selected_entry()` 拼成 `/{name}` 调 `submit_input`,不再先填输入框);**Tab 保留为"仅补全"**(想带参数时先 Tab 再继续输入)。
- Esc 关闭弹窗后 Enter 提交原文的行为不变;命令名已完整输入(exact match)走原有提交分支不变。
- 更新 handlers 相关单测。

### 4. 消除流式闪烁(问题4)— `view.rs` 的 `viewport()`

- busy 状态下预览区**固定输出 `MAX_PREVIEW_ROWS`(8)行**:内容不足补空行、thinking 尾部/工具参数预览同样固定占位;idle 时预览区为 0 行。
- 效果:流式全程视口高度恒定,不再触发 `set_viewport_height` 的清屏重建路径,输入框在流式期间零重绘(高度只在 busy↔idle 边界和用户编辑器/弹窗操作时变化)。
- 更新 `viewport_shows_stream_tail` 等 view 测试。

### 5. 输出区排版收紧(问题5)— `handlers.rs` + `markdown.rs`

- assistant 消息定稿后**不再追加空行**(`handlers.rs:623` 删除);每回合 `[tokens]` 用量行保留(按你的决定),紧贴正文显示(pi 风格)。user 消息后的空行保留。
- markdown 排版微调:**标题前插入一个空行**(消息开头除外),让章节层次更清晰;段间空行维持 1 行不变。
- 复核转录各处空行,去掉相邻重复空行(user 块后 + assistant 前等叠加场景)。

### 6. 测试与验证

- 更新受影响的单测:`footer.rs`(3 行布局/新字段)、`view.rs`(预览固定行数)、`handlers.rs`(Enter 执行)、`external.rs`(新角色)。
- `cargo test` 全量 + `cargo clippy`。
- 手动跑 `cargo run` 验证:流式期间输入框无闪烁、`/mod` + 回车直达 /model、footer 三行右对齐彩色显示。

## 不改动

- 每回合 `[tokens]` 用量行保留现状(仅显示当前轮,footer 显示 session 累计,信息不重复)。
- 模型名保持在右下角原位置。
- Inline 视口渲染架构、commit_lines 机制不动,只消除高度抖动。