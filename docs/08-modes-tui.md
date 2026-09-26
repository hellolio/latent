# 08 — 运行模式、TUI 与周边包

> **一句话**:四种运行模式(interactive/print/json/rpc)只是同一业务核的 I/O 壳;编辑器集成走 stdio 上的 JSONL RPC;TUI 是完全独立的零依赖库(差分渲染 + 同步输出),经 `ExtensionUIContext` 受限代理接入;另有实验性的 CBOR RPC 栈与持久化 harness(coding-agent 主线未用)。

## 1. 四种运行模式(`packages/coding-agent/src/modes/`)

| 模式 | 文件 | 行为 |
|---|---|---|
| **interactive** | `modes/interactive/interactive-mode.ts`(**6852 行**) | TUI 渲染与输入;组件在 `components/`(约 45 个文件:assistant-message 202、tool-execution 433、diff 147、model-selector 421、session-selector 1045、settings-selector 960、footer 247…);主题系统 `theme/`(theme.ts 1174 + theme-controller 213 + JSON schema);编辑器扩展 `custom-editor.ts`;外部编辑器 `external-editor.ts` |
| **print** | `modes/print-mode.ts`(169) | 跑一个 prompt 输出最终回复;非 TTY 自动进入 |
| **json** | `modes/json-event.ts`(61) | AgentSession 事件序列化为 JSONL stdout(`JsonAgentSessionEvent`,剥离流式 `partial`,toolcall_start 附 id/toolName) |
| **rpc** | `modes/rpc/`(rpc-mode.ts 821 + rpc-types.ts 297 + rpc-client.ts 609 + jsonl.ts 58) | stdio 上的 JSONL RPC,见下节 |

模式导出面:`modes/index.ts`(16 行)。

## 2. RPC 协议(stdio JSONL,编辑器集成的正式接口)

- **stdin**:每行一个 `RpcCommand`(`RpcCommand` 联合 @ `modes/rpc/rpc-types.ts:20-74`),约 30 个命令:`prompt / steer / follow_up / abort / get_state / set_model / set_thinking_level / compact / bash / fork / new_session / navigate_tree / get_tree / get_entries / get_messages / get_commands / get_flags / set_flag / attach / detach / …`
- **stdout**:`RpcResponse`(@116-239)+ `AgentSessionEvent` 流(与 TUI 消费的同源事件);
- **扩展 UI 反向通道**:扩展要弹选择框时,rpc 模式把 `ExtensionUIContext` 调用编码为 `extension_ui_request` 发给客户端,等待 `extension_ui_response`(@246-291)—— 扩展代码完全无感;
- `rpc-client.ts` 是官方 TypeScript 客户端(供 SDK/编辑器侧使用);`rpc-entry.ts`(13 行)是 bundle 的第二入口。

## 3. TUI 库(`packages/tui/`,~22.7k 行)

- **零依赖**:不知道 agent/coding-agent 的存在;coding-agent 的 `modes/interactive` 用它搭界面,主题是 coding-agent 层概念注入。
- **渲染模型**:差分渲染(只重画变化行)+ **CSI 2026 同步输出**(防闪烁)+ 括号粘贴模式;双实现同一 `TUI` 接口(@ `tui/src/tui.ts:425`):
  - `TuiMainScreen`(`tui-main-screen.ts`,655)—— 主缓冲,保留终端 scrollback;
  - `TuiAltScreen`(`tui-alt-screen.ts`,1745)—— 备用缓冲固定高度视口,应用自管滚动,退出时把最终文档打回主缓冲。
- `Component` 接口极简(@ tui.ts:111,核心是 `render(width): string[]`);overlay 系统(@175-270);`ViewportTUI`(@457)扩展视口能力。
- **组件库**(`components/`):Text(107)、TruncatedText(65)、Input(494)、**Editor(2472,行编辑核心:undo-stack、kill-ring、word-navigation)**、Markdown(1015)、Loader、SelectList(273)、SettingsList(328)、ScrollView(224)、Box/HStack/VStack/Stack、Image(127,Kitty/iTerm2 内联图片)、MouseRegion(33)、Autocomplete。
- **键位**:`keys.ts`(1401,终端转义解析 + `matchesKey`)+ `keybindings.ts`(320,`DEFAULT_EDITOR_KEYBINDINGS`/`DEFAULT_APP_KEYBINDINGS`;AGENTS.md 规定禁止硬编码键检查,一律走可配置默认表)。
- **原生模块**:`native/darwin|linux|win32`(C 实现剪贴板等),发布预编译 `.node` prebuilds;运行时能力探测 `getCapabilities()`。
- 其余:`terminal.ts`(547,终端抽象)、`stdin-buffer.ts`(444)、`layout.ts`(449)、`fuzzy.ts`(138)、`latex.ts`(1506,公式渲染)。

## 4. 实验性多进程栈(protocol/server/client + chord)

与 modes/rpc **完全独立**的一套 CBOR 二进制 RPC(coding-agent 的 `src/experimental/` 是唯一消费者):

- **protocol**(`protocol/src/protocol.ts`,110 行):`PROTOCOL_VERSION = 8`;信封极小 —— C→S:`hello | request{id,target,call} | cancel{id,target}`;S→C:`hello{version,serverId} | hello_error | response{id,ok,result|error} | service_update | attachment`。路由目标 `ServerTarget{serverId}` 或 `SessionTarget{serverId, sessionId, attachmentId}`(attachmentId 服务端生成,防串话)。线格式(`framing.ts` 151 + `cbor/` 436):4 字节大端长度 + 一个 definite-length CBOR item;payload 必须严格 JSON;限制 16MiB/帧、100 万元素、64 层嵌套。**payload 语义归 chord 所有**(`{serviceId, instance?, member, args}`)。
- **server**(`server/src/server.ts`,576 + session-router.ts 312):透明路由与 attachment 生命周期;Unix domain socket 传输(`transports/unix/listener.ts` 421)。
- **client**(`client/src/client.ts`,479):`connect({serverId, transportFactory})` + `request`/`subscribeService` 原语(先快照后缓冲更新);**不自动重连/重放**。
- **chord**:应用组合运行时(facets/plugins/services/复制状态 Delta/RPC 边界),不依赖任何 pi 包。
- 重写结论:此栈可整个跳过,保留 stdio JSONL RPC 即可;若未来要多进程,protocol 的信封设计(长度前缀 + 自描述负载 + attachment 令牌)值得参考。

## 5. harness 附注(packages/agent/src/harness/,~25k 行,实验性)

"agent 对话的持久化运行时":进程中断后工作可恢复、已定案效果不重复执行。核心概念(重写主线用不到,但以下设计可迁移):

- **持久 entry 树**(`session/types.ts`,602):`MessageEntry/CompactionEntry/BranchSummaryEntry/CustomEntry`,公共头 `{id, parentId, seq, timestamp}`;`Storage` 接口(commit 原子写 + scan);memory 与 JSONL 实现 + **conformance 测试套件**(可测任意 Storage 后端)。
- **操作状态机**:`OperationState` 恰好 **13 个扁平叶**(`starting / checkpoint / assistant.ready / assistant.effect_pending / assistant.retry_wait / tools / deferred.suspended / deferred.effect_pending / summary.deciding / summary.ready / summary.effect_pending / summary.retry_wait / navigation.ready_to_commit`);调度器 `runtime/drive.ts`(106 行)按状态分派,状态未变且未请求取消 → 抛 `SessionInvariantError` 强制进展。
- **意图-效果-结算三段式**:provider 请求前先提交 intent(预留 response/usage 两个 UUIDv7);流式帧 append 进 pending 列表不断提交;结算事务删帧 + 写 entry + usage + tip + 下一状态。
- **孤儿恢复**:重启后对 `assistant.effect_pending` 读已提交帧前缀,合成 `stopReason:"error"` 的"被打断"消息 settle —— 不重复外发效果、不丢已产出部分(`runtime/drive/recovery.ts`)。
- **响应结算分类器**(`runtime/drive/response.ts`,485):cancel/overflow/deferred/error(可重试→持久 retry_wait)/正常(tools 批计划)逐分支原子决定;overflow 每 run 最多一次"压缩后重试"。
- **工具差异**:harness 工具 execute 签名**无 AbortSignal**(由 Gate 注入,`execution/effect-gate.ts`),`replay: "never"|"safe"` 标注重放策略;内置工具基于 `ExecutionEnv{FileSystem + Shell}` 抽象(全 Result 不抛)。
- **上下文过滤差异**:harness 的 `buildContextEntries` **过滤 stopReason 为 error/aborted/deferred 的 assistant 消息**(失败的回复不进模型上下文)—— 与 coding-agent 不同,重写时需二选一。
- compaction 世界(`harness/compaction/compaction.ts`,865):算法与 core 版同构(切点不在 toolResult、固定摘要模板)。

## 6. 其余支撑包

| 包 | 说明 |
|---|---|
| `packages/durable` | Pico runtime:持久化 conversation/task/document 记录契约 + memory/JSONL/SQLite 三种存储 |
| `packages/session-backends/sqlite-node` | 给 agent 包 Session 提供 node:sqlite 后端(一 session 一文件或共享容器) |
| `packages/telemetry` | 显式回调式遥测契约(`TelemetryContext`/`TelemetrySpan`),NOOP 与 InMemory 参考实现;无 exporter、无全局状态 |
| `packages/evals`(private) | 基于 vitest-evals 的行为评测(host evals + Docker 双臂 docs-lift 对比) |

## 7. 源码文件索引

### modes/

| 文件 | 行数 | 职责 | 关键符号 | 优先级 |
|---|---|---|---|---|
| `packages/coding-agent/src/modes/rpc/rpc-types.ts` | 297 | RPC 协议类型 | `RpcCommand`:20, `RpcResponse`:116, `extension_ui_request/response`:246-291 | P0 |
| `packages/coding-agent/src/modes/rpc/rpc-mode.ts` | 821 | rpc 模式实现 | 命令分派 | P0 |
| `packages/coding-agent/src/modes/rpc/rpc-client.ts` | 609 | 官方 TS 客户端 | — | P1 |
| `packages/coding-agent/src/modes/rpc/jsonl.ts` | 58 | JSONL 读写 | — | P1 |
| `packages/coding-agent/src/modes/print-mode.ts` | 169 | print 模式 | `runPrintMode` | P0 |
| `packages/coding-agent/src/modes/json-event.ts` | 61 | json 模式 | `JsonAgentSessionEvent` | P1 |
| `packages/coding-agent/src/modes/index.ts` | 16 | 模式导出 | — | P1 |
| `packages/coding-agent/src/modes/interactive/interactive-mode.ts` | 6852 | TUI 模式巨类 | — | P1(重写 TUI 时) |
| `packages/coding-agent/src/modes/interactive/components/tool-execution.ts` | 433 | 工具执行渲染 | — | P2 |
| `packages/coding-agent/src/modes/interactive/theme/theme.ts` | 1174 | 主题系统 | — | P2 |
| `packages/coding-agent/src/modes/interactive/components/session-selector.ts` | 1045 | 会话选择器(树 UI) | — | P2 |

### tui 包

| 文件 | 行数 | 职责 | 关键符号 | 优先级 |
|---|---|---|---|---|
| `packages/tui/src/tui.ts` | 1468 | TUI 接口与调度 | `Component`:111, overlay:175-270, `TUI`:425, `ViewportTUI`:457 | P0 |
| `packages/tui/src/tui-alt-screen.ts` | 1745 | 备用缓冲视口实现 | — | P1 |
| `packages/tui/src/tui-main-screen.ts` | 655 | 主缓冲实现 | — | P1 |
| `packages/tui/src/terminal.ts` | 547 | 终端抽象 | — | P1 |
| `packages/tui/src/components/editor.ts` | 2472 | 行编辑器 | undo/kill-ring | P1 |
| `packages/tui/src/keys.ts` | 1401 | 按键解析 | `matchesKey` | P1 |
| `packages/tui/src/keybindings.ts` | 320 | 可配置键位表 | `DEFAULT_EDITOR_KEYBINDINGS` | P1 |
| `packages/tui/src/components/markdown.ts` | 1015 | Markdown 渲染 | — | P2 |
| `packages/tui/src/components/*.ts`(其余) | 33-494 | 组件库 | select-list/settings-list/scroll-view/image | P2 |

### 实验栈与支撑

| 文件 | 行数 | 职责 | 关键符号 | 优先级 |
|---|---|---|---|---|
| `packages/protocol/src/protocol.ts` | 110 | RPC 信封 | `PROTOCOL_VERSION`:— | P2 |
| `packages/protocol/src/framing.ts` | 151 | 帧格式 | — | P2 |
| `packages/server/src/server.ts` | 576 | 本地 server | — | P2 |
| `packages/client/src/client.ts` | 479 | RPC client | — | P2 |
| `packages/agent/src/harness/agent-harness.ts` | 622 | harness 公开面类型 | `OperationRequest`:111, `HarnessEventPayload`:255, `HookMap`:430, `AgentLane`:538 | P2(附注) |
| `packages/agent/src/harness/session/types.ts` | 602 | 持久 entry 树与 Storage | `Entry`:64, `OperationState`:316, `Storage`:455 | P2(附注) |
| `packages/agent/src/harness/runtime/drive.ts` | 106 | 状态机调度器 | `for(;;)` 分派 | P2(附注) |
| `packages/agent/src/harness/runtime/drive/response.ts` | 485 | 响应结算分类器 | — | P2(附注) |
| `packages/agent/src/harness/runtime/drive/recovery.ts` | 126 | 孤儿恢复 | `recoverAssistantGeneration`:44 | P2(附注) |
| `packages/agent/src/harness/runtime/lane.ts` | 2012 | Lane(串行 mutation line) | — | P2(附注) |

阅读顺序:`rpc-types.ts` → `rpc-mode.ts` → `print-mode.ts`/`json-event.ts` → `tui.ts` 接口部分;实验栈与 harness 按需。

## 踩坑记录

- **2026-09-26 ANSI CSI 序列的终止字节区间包含 `[` 本身**:剥离/复制转义序列时按"扫描到 0x40-0x7E 终止字节"实现,`\x1b[1m` 只剥掉 `\x1b[` 留下 `1m` 混入可见文本,ANSI 宽度计算随之全错 → 解法:先单独消费 `ESC`(和 introducer `[`),再扫描终止字节(`[` 在区间内须排除)。 (关联文件:`crates/rpi-tui/src/components.rs`)
- **2026-09-26 serde struct 不能用 `#[serde(tag = "type")]`**:想给 `RpcResponse` 输出 `"type":"response"`,在 struct 上加 internally-tagged 属性得到的是类型名(`"type":"RpcResponse"`)而非自定义值 → 解法:显式字段 `#[serde(rename = "type")] kind: &'static str`,在构造器里赋 `"response"`。 (关联文件:`crates/rpi-cli/src/modes/rpc.rs`)
- **2026-09-26 主缓冲渲染的两个画面错乱路径**:① 进 interactive 时光标通常停在屏幕中部(shell 提示符后),渲染器默认"视口占底部"于是首帧 `cursor_up(H-1)` 越进 scrollback → 解法:构造渲染器前先输出 `rows-1` 个换行把光标锚定到屏幕底;② commit 的行超宽时终端自行回绕,实际光标比追踪值多走 N 行且 auto-scroll 不补偿 → 解法:commit/视口行都在渲染器内部按保存的终端宽度折行/截断,不信任调用方。 (关联文件:`crates/rpi-tui/src/screen.rs`、`crates/rpi-cli/src/modes/interactive.rs`)
- **2026-09-26 rpc 是异步协议,应答到达顺序 ≠ 命令顺序**:prompt spawn 后 run 进行中 stdin 保持可响应,后续命令的应答先于 prompt 应答上线;客户端必须按 `id` 关联应答,测试不能按下标对位 → 集成测试用 tokio duplex 流模拟真实编辑器(等 `agent_settled` 事件后再发依赖 run 结果的查询)。 (关联文件:`crates/rpi-cli/tests/modes.rs`)
- **2026-09-26 rpc 客户端断连会挂死扩展 UI 调用**:扩展等 `confirm/select` 应答时客户端关 stdin,未决 oneshot 无人 resolve → `wait_idle` 永不返回 → 解法:stdin EOF 后 `RpcUi::close_all()` 清空 pending,让等待方走 Err 分支落默认值(confirm 默认**拒绝**,不 fail-open)。 (关联文件:`crates/rpi-cli/src/modes/rpc.rs`)
- **2026-09-26 retry hooks 与 SessionBridge 需要同一个订阅者列表**:`create_session_retry_hooks` 产生的 AutoRetry 事件要进 mode 可见的事件流,但 core 内部自建列表拿不到 → 解法:`AgentSessionConfig` 加可选 `subscribers` 字段,装配方预建列表同时交给 SessionBridge 与 retry hooks(带默认值的加法变更,不动接缝签名)。 (关联文件:`crates/rpi-core/src/session.rs`)
- **2026-09-26 M5/M6 子集取舍(有意为之,非遗漏)**:rpi-tui 只实现主缓冲单实现 + 4 个组件(Text/Markdown/SelectList/Editor),pi 的 TuiAltScreen 双实现、Image/Kitty 图片、完整键位表(keybindings.ts 可配置默认表)未引入;rpc 只实现业务核已支持的 12 命令,未知命令返回明确错误而非静默。扩充时按 §7 索引的优先级逐项对照。 (关联文件:`crates/rpi-tui/src/lib.rs`、`crates/rpi-cli/src/modes/rpc.rs`)
