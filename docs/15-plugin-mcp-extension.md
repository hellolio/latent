# 15 — 插件路线 B:进程外 MCP 扩展(独立项目)

> **一句话**:插件是独立进程的 MCP stdio server,在 `.rpi/settings.json` 声明后由 rpi 启动/回收;工具经标准 MCP 方法注册,事件干预经自定义 `rpi/register`/`rpi/event`,UI 反调走 elicitation;错误语义是"跳过 + 收集诊断,绝不击穿宿主"。

进程内编译期形态见姊妹篇 `docs/14-plugin-in-process.md`(路线 A);两条路线的对比见该文 §1。

---

## 1. 何时选路线 B

- 插件用**任意语言**实现、**独立安装分发**(拷贝可执行文件 + 改 settings);
- 只需要"注册个工具 / 观察或干预事件 / 弹个 UI 原语",不需要跑 agent loop、不需要宿主权限引擎的进程内集成;
- 崩溃隔离重要(扩展崩溃不影响宿主)。

**不适合**的场景:引擎级能力(如 subagent 需要跑 agent loop)——见 §5 的代价分析,应选路线 A。协议与埋点的完整设计权威是 `docs/07-extensions.md` §8,本文只讲"怎么写",开发前必读 07 §8.4–§8.8。

## 2. 开发 how-to

1. 用任意语言写 MCP stdio server(Rust 用 `rmcp`,与宿主同版本号 pin);
2. 用户在 `.rpi/settings.json` 声明:

   ```json
   { "mcpServers": [{ "name": "my-ext", "command": "my-ext", "args": [] }] }
   ```

3. **最小可用 = 只实现 `tools/list` + `tools/call`**:rpi 把工具桥成 `McpTool`,名字自动加 `my-ext__` 前缀(扩展名清洗为 `[a-zA-Z0-9_-]`,重名扩展装配期记诊断跳过);
4. **需要干预事件** → 实现自定义 `rpi/register`(声明订阅事件表,每事件 `timeoutMs` + `failClosed`)+ `rpi/event` 处理器。埋点全集与宿主接缝的映射见 07 §8.3:决策类(tool_call 可 block/改参、context、before_request 等)按注册顺序串行 await,观察类(agent 生命周期、tool_execution_*)单向广播;
5. **UI 反调** → elicitation 映射到 `ExtensionUi` 四原语:notify / confirm(boolean)/ select(enum)/ input(string),属性名必须是 `value`;
6. 工具进度与取消:`notifications/progress` → `ToolUpdater`;`notifications/cancelled` ↔ `CancellationToken`。

## 3. 协议红线(违反即静默失败,07 §8.8)

- **响应信封**:对 `rpi/register` / `rpi/event` 的响应必须包 `{"rpiResult": <载荷>}`。裸载荷(如 `{"isError":true}`)会被 rmcp 的 untagged `ServerResult` 贪婪解析成 `CallToolResult` 而丢失;
- **事件 wire 名一律 snake_case**;埋点载荷以 Rust 类型的 serde 形态为权威(如 `ToolCallCtx` 的字段是 `name` 不是 `toolName`);
- **高频事件双声明**:MessageDelta/MessageUpdate/ToolExecutionUpdate 默认不上线缆,需在 `events` 表声明订阅 **且** 在 `highFrequency` 数组声明解除限制;
- **decision 链式传递**:tool_call.args、context.messages、before_request.model/thinkingLevel 等均回写 payload,前一扩展输出 = 后一扩展输入;
- `prepare_request` 的 thinkingLevel 只能设值、不能显式关闭(JSON 无法区分 null 与缺省)。

## 4. 行为红线与错误语义(07 §8.5)

- handler 失败/超时/断连 → 本扩展该次分发跳过 + 诊断,**绝不击穿宿主**;等待中的决策请求由宿主 pending 表作废;
- tool_call 可声明 fail-closed(守卫类扩展应声明):断连后死掉的守卫不静默放行;其余决策事件 fail-closed 退化为跳过 + 诊断;
- 观察类通知发送有 5s 超时,超时 mark_stale + 诊断(防 SIGSTOP 的扩展塞满管道卡死宿主);
- **"不许 panic"是约定**;不得假设能长期持有宿主资源(扩展进程随 rpi 启动 spawn、退出回收)。

## 5. 为什么路线 B 不适合做 subagent 引擎

扩展进程拿不到 rpi 的 agent loop,只能 spawn `rpi --print` 子进程模拟:

| 代价 | 说明 |
|---|---|
| 进度回传粗 | 只能转发子进程 stdout 为 progress 通知,无流式事件 |
| 审批语义靠子进程 | 子进程走自己的 headless 审批,与宿主权限引擎无进程内集成 |
| 每子 agent 全量启动 | 进程创建 + provider 初始化 + 握手,无法像嵌套 Agent 一样廉价 |
| 递归防护弱 | 只能靠环境变量约定,子进程需主动配合 |

结论:subagent 走路线 A(`docs/14-plugin-in-process.md` §4);仅当"第三方独立分发 subagent 类型"成为需求时再考虑 B。

## 6. 本路线已知问题登记

- **wire 协议无版本协商**:`rpi/register` 无 protocolVersion 字段,扩展与宿主协议演进只能靠 pin rmcp 版本;做第三方分发生态前需补版本字段 + 降级策略。

---

## 源码文件索引(rpi 宿主侧)

| 文件 | 关键符号 | 优先级 |
|---|---|---|
| `docs/07-extensions.md` §8 | B 方案设计权威:路线对比、埋点全集、协议映射、踩坑记录 | P0 |
| `crates/rpi-core/src/extensions/mcp_host.rs` | `McpConnection`、`connect_stdio`、`bridge_elicitation` | P0 |
| `crates/rpi-core/src/extensions/event_bus.rs` | `ExtensionEventBus`、`ExtensionHooks`、`ExtensionRegistration` | P0 |
| `crates/rpi-core/src/extensions/mcp_tool.rs` | `McpTool`(工具桥,前缀/进度/取消) | P0 |
| `crates/rpi-core/src/extensions/mod.rs` | `create_extension_event_bus`(装配工厂)、`ExtensionUi` | P1 |
| `.rpi/settings.json` | `mcpServers` 声明格式 | P1 |
| 参照 | `crates/rpi-cli/src/mcp_mock.rs`(`--mcp-mock-server` 自检 mock 服务端) | P1 |

阅读顺序:本文 → 07 §8(协议细节与踩坑)→ mcp_mock.rs(mock server 是最好的起点模板)。
