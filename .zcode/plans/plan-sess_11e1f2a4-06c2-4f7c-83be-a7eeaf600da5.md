## 改动一：删除 web_access 动态激活，4 个 web 工具从一开始就注册（保缓存）

目标：tools 数组从会话第一次请求起就恒定（read/bash/edit/write + 4 个 web 工具 + load_skill + subagent），消灭会话中途 tools 变化导致的缓存全量失效。会话级 `set_active_tools_by_name` 机制本身保留（settings `tools:` 配置和 resume 恢复还在用），只删 web_access 这一特化链路。plan mode 注入逻辑不动。

### 1. latent-web crate
- **删除整个 `crates/latent-web/src/tools/web_access.rs`**（WebEnableTool 及其测试）。
- `tools/mod.rs`：删 `pub mod web_access;`（L10）；删 `ToolSetActivator` trait（L20-24）；删 `WebContext.activator` 字段（L39-40）；`create_web_tools` 移除 web_access 条目，只返回 4 个工具并更新文档注释（L67-76）；`names` 中删 `WEB_ACCESS`、`ACTIVATABLE`、`activatable_strings()`（L84-96）；更新模块文档（L1-5）。
- `prompts.rs`：删 `WEB_ACCESS_DESCRIPTION`（L172-173）和 `web_access_prompt_snippet()`（L175-177）。
- `lib.rs`：L24 re-export 去掉 `ToolSetActivator`，L2 注释更新。
- 测试/示例夹具（删字段引用）：`tools/get_search_content.rs:668-672,685`（NullActivator）、`summary.rs:104,140`、`tools/mod.rs:243`、`examples/e2e_manual.rs:18-25,36`。

### 2. latent-cli crate（assembly.rs）
- 删 `SessionToolSetActivator`（L668-691）。**保留** `AgentFollowUpNotifier`（L693-711）和 `web_agent_cell`——web_search 后台抓取完成通知仍在用，与激活无关。
- 删 `web_session_cell`（L929）及其回填（L1133-1134），注释改为"后台完成通知"。
- 默认激活逻辑（L1049-1073）：删 `has_web_access` 计算和 `(None, None) if has_web_access` 分支，默认回落到 `None`（= 全量候选工具激活）。更新 L906-910 注释。

### 3. latent-core
- `permission/types.rs:84-85`：`classify_tool` 的 ReadOnly 分支去掉 `"web_access"`（其余 4 个 web 工具名保留）。

### 4. 测试
- `crates/latent-cli/tests/modes.rs:1190-1243`：删 `web_access_activates_web_tools_via_session_bridge` 和仅它使用的 `NoopToolUpdater`；新增替代测试：默认配置下 `active_tool_names()` 包含 `web_search`/`source_check`/`fetch_content`/`get_search_content`（即不再需要激活步骤）。
- 保留不动：`latent-core/tests/session_integration.rs` 的 active_tools 测试、`modes.rs:794-877` 的 settings tools 测试（机制仍在）。

### 5. 文档
- `docs/16-web-access.md`：改为"4 个工具、随会话常驻、无懒激活"，更新工具表和实现索引；`docs/web-search-principles-and-prompts.md` 加一行注记说明 latent 未移植激活工具。

## 改动二：缓存命中率计算的修正

审计结论（已逐环节核实）：解析（`openai_completions.rs:587` `parse_chunk_usage`，智谱官方确认 `prompt_tokens` 含 `cached_tokens`）、每请求 TurnEnd 上报（`loop_.rs` 工具循环每次请求都发事件，无遗漏）、累计（`usage.rs accumulate` 全量求和）、公式（`footer.rs:161`，与智谱 `cached_tokens/prompt_tokens` 口径一致）——**链路正确，38% 是真实读数**，主因就是 tools 动态切换等前缀失效（本次改动一即根治）。审计中发现两处真实缺陷，一并修掉：

- `crates/latent-cli/src/modes/interactive/overflow.rs:89,97`：静默溢出检测的上下文规模用 `input + cache_read`，**漏了 `cache_write`**，与 `compaction.rs:72-78`、`usage.rs:46-48` 不一致（Anthropic 的 cache_write 计入窗口，重缓存会话会漏报溢出）→ 改为三项相加。
- `crates/latent-tui/src/footer.rs:161`：`(cache_read * 100) / prompt_total` 整除截断（4.9% 显示 4%）→ 改为四舍五入。

## 验证

`cargo build` + `cargo test`（重点：latent-web、latent-cli/tests/modes.rs、latent-core）+ `cargo clippy`。