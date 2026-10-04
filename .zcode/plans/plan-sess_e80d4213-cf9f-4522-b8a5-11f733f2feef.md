## 修复"回合结束后输入框上方大片空白"问题

### 根因
回合结束的事件循环顺序是:① flush 定稿内容(此时视口仍是流式高度的 18 行,内容填在旧位置)→ ② `draw()` 把视口缩回 10 行 → 收缩时 `rebuild_terminal` 清掉释放的 8 行,成为输入框上方的空白带,要等下一次提交才能回填。ctrl+o 全文重绘把转录重打一遍,所以"切一次就好"。另外思考阶段预览区固定占 10 行但内容只有 4+1 行,视口内部还有 ~5 行补位空白,放大了观感。

### 改动

**1. 调整每帧渲染顺序(核心,`crates/latent-cli/src/modes/interactive/mod.rs`)**

把 `draw()` 拆为"构建帧 + 同步高度"与"绘制"两步,flush 挪到中间:

```
render_tick(state, app, partial):
  ① 计算帧(现有收缩循环逻辑原样保留)
  ② app.set_viewport_height(frame.height)   ← 先改高,收缩产生空带
  ③ flush state.pending → app.commit_lines  ← 定稿内容 insert_before 从
                                               视口上方往下填,正好回填空带
  ④ app.draw_viewport(&frame.lines, frame.cursor)
```

- `event_loop` 主路径与循环前的首次绘制改调 `render_tick`;
- `needs_full_redraw` 分支保持现状(分支内先 flush pending 再 `redraw_full`,与今天一致);
- flush 不影响帧内容(pending 是转录行,不进视口),帧先算安全;
- 不新增 resize 次数:高度切换仍只在空闲↔思考↔正文↔回合结束的边界发生(每回合 2~3 次),流式增量全程不触发 resize,无新闪烁源。

**2. 思考阶段预览高度精确化(`view.rs` + `mod.rs`)**

- `view.rs` 新增 `pub const THINKING_PREVIEW_ROWS: usize = 5;`(4 行 + 提示恰好占满);
- `preview_cap_for`:思考中 → 5(替换现在的 10),流式正文 → 10(不变),空闲 → 2(不变)。思考阶段视口内部不再有补位空白;思考→正文交接时的增长空带会被紧随其后的思维链块落盘立即回填。

**3. 测试**

- `latent-tui/src/app.rs` 加 L2 测试(TestBackend):提交内容 → 绘制视口 → 收缩视口 → 再提交内容,断言收缩空出的行被新内容回填(验证第 1 条依赖的 insert_before 机制);
- `latent-cli` 更新 `preview_cap_is_constant_during_streaming`:思考中断言为 5;
- 回归:`cargo test -p latent-tui -p latent-cli` 全绿 + `cargo build --workspace` 零警告。

### 预期效果
回合结束时模型输出/用量框紧贴输入区上方,无需 ctrl+o;流式期间空带从 ~13 行降到 3~5 行。

### 已知限制(不恶化现状)
- 纯正文输出的回合,流式期间存在 ~8 行增长空带,回合结束时被正文回填(Inline 视口架构的固有限制);
- 空带部分回填时少量空行会进 scrollback 回看区。