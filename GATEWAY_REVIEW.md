# latent-gateway / latent-channel 审查报告与修复计划

> 生成日期：2026-10-07。审查对象：git staged 的 `crates/latent-channel`（全量新增）、`crates/latent-gateway`（全量新增）及 `latent-runtime`/`latent-cli`/根 Cargo.toml/文档改动；对照 `GATEWAY_PLAN.md`（§2/§4/§5/§6/§8/§10）与 `AGENTS.md` 工程规范。
> 审查方式：4 路分域独立审查（安全专项 / channel 协议层 / gateway 引擎并发 / 规范与计划符合度），**P0 与全部 P1 已逐行人工复核**；P2/P3 来自审查代理（含行号），修复时以实际代码为准。
> 用法：每条含【位置 / 问题 / 证据 / 修复建议 / 验收】。修完勾掉对应 `- [ ]`。每批修完跑：`cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets`（全绿零警告）+ §1 隔离断言。

---

## 0. 总评与修复批次

架构与实施纪律整体良好：GATEWAY_PLAN §10 实施坑清单所列各坑（B1 出站消费循环、B4 same_channel、A1-A4 serde、E1-E6 装配语义）全部有落实与测试；防抖/chunk/typing/pairing 常量与上游语义逐项一致；控制面 11 个方法齐全；unsafe 零出现、外部依赖精确 pin、测试全离线、§8 偏离记录大体诚实。**但存在 2 个 P0（其一为权限绕过安全洞）与 14 个 P1，建议全部修复后再合入。**

| 批次 | 内容 | 主题 |
|---|---|---|
| 1 | P0-1 + P1-8 | 聊天暴露面的权限底线 |
| 2 | P0-2 + P1-6 | 推荐配置下渠道基本不可用 |
| 3 | P1-5 | 中文场景核心功能失效 |
| 4 | P1-1 / P1-2 / P1-3 | §5.3 资源上限与节流承诺 |
| 5 | P1-9 / P1-10 / P1-11 / P1-12 | "消息不丢、转录即真相"契约 |
| 6 | P1-7 / P1-13 / P1-14 + P2 批次 | 其余 |

---

## 1. P0（合入前必须修）

### - [ ] P0-1【安全】full-access 权限天花板可被参数别名绕过

- **位置**：`crates/latent-gateway/src/auto_reply/mod.rs:585-586`；对照 `crates/latent-core/src/permission/types.rs:29-36`
- **问题**：权限门对 `/mode` 参数做**字面串精确比对**，而实际切档走 `SessionMode::parse`（接受 `full-access | fullaccess | full_access` 且大小写不敏感）。群聊被 `owner_only_in_group()` 兜住不受影响；但**私聊中任何已过 dmPolicy 的非 owner**（配对用户 / allowlist / `dmPolicy: open` 下的任何人）发 `/mode full_access` 或 `/mode FULL-ACCESS` 即可绕过门禁，把会话切到 FullAccess——之后其消息驱动一个无审批、无沙箱的 agent，等效拿走机器控制权。直接击穿 GATEWAY_PLAN §5.1。
- **证据**：
  ```rust
  // auto_reply/mod.rs:585-586
  let full_access_requested =
      matches!(&command, ChatCommand::Mode { arg: Some(arg) } if arg.trim() == "full-access");
  // latent-core/src/permission/types.rs:33（parse 先 to_ascii_lowercase()）
  "full-access" | "fullaccess" | "full_access" => Some(SessionMode::FullAccess),
  ```
- **修复建议**：门改用与执行层同一个解析函数做语义判定：
  `matches!(&command, ChatCommand::Mode { arg: Some(arg) } if SessionMode::parse(arg) == Some(SessionMode::FullAccess))`；
  更稳妥：把 full-access 的 owner 判定下沉到 `execute_session_command` 的 `Mode` 分支内（解析后判定），门与执行不可能再漂移。
- **验收**：新增回归测试——非 owner 私聊发 `/mode full_access`、`/mode FULL-ACCESS`、`/mode fullaccess` 均被拒；owner 三种拼写均成功。

### - [ ] P0-2【功能】raw 配置覆盖把已解析凭据打回 `$ENV` 原始串，推荐配置下三渠道全废

- **位置**：`crates/latent-gateway/src/channels.rs:98-106`；关联 `crates/latent-gateway/src/main.rs:199/243/250`
- **问题**：`main.rs:199` 先经 `prepare_channel_credentials` 把 `$ENV`/`!shell` 解析成明文写回 typed 配置；`main.rs:243` 却另取**未解析**的 raw 配置，`start_enabled` 中 raw 逐键无条件覆盖 typed。按 §4.2 推荐写 `accessToken: "$NAPCAT_TOKEN"` 的用户：QQ 实际发送 `Bearer $NAPCAT_TOKEN` 字面量（NapCat 永远 403）、telegram `getMe` 401、wecom 订阅被拒 → 三渠道全部 Failed；**反而写明文 token 能用**，而危险配置警告恰恰在把用户往 `$ENV` 推。e2e 测试全用 mock 渠道直连 `attach`，没走 `start_enabled` 凭据路径，故未暴露。
- **证据**：
  ```rust
  // channels.rs:98-106
  for (id, chunk_limit, mut typed) in enabled {
      // raw merged config 优先(保留未类型化键,原样转发给渠道)
      if let Some(raw) = raw_channels.get(id) {
          if let (Some(base), Some(raw)) = (typed.as_object_mut(), raw.as_object()) {
              for (key, value) in raw { base.insert(key.clone(), value.clone()); }
          }
      }
  ```
- **修复建议**：`load_config` 的 `deny_unknown_fields` 校验在前，raw 合并无正向价值——**直接删除 raw 覆盖逻辑**。若保留，必须跳过已被 `prepare_channel_credentials` 解析过的键（accessToken/secret/botToken）或对 raw 值重跑同一解析。
- **验收**：端到端测试——`$ENV` 凭据 + `start_enabled` → 假 NapCat/假 Bot API 认证通过。

---

## 2. P1（应当修，按域分组）

### 安全面

#### - [ ] P1-1 QQ 接入冷却按含临时端口的 SocketAddr 计键，节流失效 + 失败表无界

- **位置**：`crates/latent-channel/src/qq/mod.rs:111`（键类型）、`402-428`（判定）；对照控制面正确实现 `crates/latent-gateway/src/control/auth.rs:29`（用 `IpAddr`）
- **问题**：每次 TCP 重连源端口都变 → `record_failure`/`is_cooled_down` 永不命中同一条目，"连续 5 次失败 → 60s 冷却"（§5.3）对真实攻击者形同虚设；且失败条目仅成功时移除，公网 bind 下无界增长。
- **修复**：键改 `peer.ip()`；失败表加容量上限/过期清理（控制面 `auth.rs` 的 `failures` 表同病，一并处理）。
- **验收**：同 IP 换端口连续 5 次错 token → 第 6 次连接被冷却拒绝。

#### - [ ] P1-2 1MiB 帧限在整帧缓冲之后检查，实际可缓冲 ~64MiB

- **位置**：`crates/latent-gateway/src/control/server.rs:187`、`crates/latent-channel/src/qq/mod.rs:462-473`（同型）
- **问题**：应用层拿到 `Message::Text` 时整帧已被 axum/tungstenite 缓冲完（tungstenite 默认消息上限约 64MiB，待核实精确默认值）。8 并发 × 64MiB ≈ 512MiB 可被打满，"单帧 1 MiB（超限断连）"（§5.3）实际是"缓冲到 64MiB 再断"。
- **修复**：`WebSocketUpgrade`（axum）链式 `.max_message_size(MAX_FRAME_BYTES).max_frame_size(...)`；qq 侧 tungstenite 同理；应用层检查保留作双保险。

#### - [ ] P1-3 配对码兜底熵源退化为恒定输出

- **位置**：`crates/latent-gateway/src/pairing.rs:186-195`
- **问题**：同一 `RandomState` 对同一常量反复 SipHash → 8 字节全相同。Windows 上（唯一走兜底的平台）配对码恒为同一字符 ×8，码空间从 32⁸ 塌缩到 32。Unix 正常走 `/dev/urandom` 不受影响。
- **修复**：循环内混合下标（`hasher.write_u64(i as u64)`）；或兜底直接返回 Err 拒绝启动（fail-closed）；或登记 `getrandom` crate（需按仓库规范先在根 Cargo.toml 登记）。三选一。
- **验收**：单测——连续两次 `generate_code()` 不同；mock 一次 `/dev/urandom` 失败路径不再产生重复字符码。

#### - [ ] P1-4【需拍板】项目级 gateway.json 可覆盖 auth.token / ownerAllowFrom / sessionMode=full-access

- **位置**：`crates/latent-gateway/src/config.rs`（项目逐字段深合并）；关联 `main.rs:135-153`
- **问题**：`!shell` 项目级拒绝的理由是"恶意 repo 可借 gateway 启动"（config.rs:8/454-456），但恶意 repo 的 `.latent/gateway.json` 可以：把 `gateway.auth.token` 覆盖为攻击者已知的**明文**值（不触发 `!shell` 拒绝）、覆盖 `commands.ownerAllowFrom` 为攻击者身份、`dmPolicy/groupPolicy: open`，再经项目 `.latent/settings.json` 覆盖 `sessionMode: full-access`——受害者在本 repo 内启动 gateway 后，攻击者以 owner 身份聊天即可驱动 full-access agent。效果与被禁的 `!shell` 等价。**计划层只拍板了 `!shell` 一项，此项属威胁模型决策。**
- **修复（按拍板结果选）**：安全敏感字段（`gateway.auth`、`commands.ownerAllowFrom`、`messages.groupChat`、settings 的 `sessionMode`）全局只读 / 项目级出现即拒绝启动 / 至少显式 stderr 警告列出被覆盖字段。

### 协议正确性（latent-channel）

#### - [ ] P1-5 Telegram @实体偏移按 UTF-8 字节切，Bot API 单位是 UTF-16 code units → CJK 群 @ 判定系统性失效

- **位置**：`crates/latent-channel/src/telegram/mod.rs:363-365`
- **问题**：mention 前出现任何中文/emoji 时，实体 offset/length（UTF-16 单位）与 `str::get`（字节偏移）错位：切片落进多字节字符中间返回 None → 静默判为未 @ → `requireMention` 群里中文用户 @ 机器人不响应（偶发对齐时产生错误切片）。现有测试只覆盖 offset=0 纯 ASCII。
- **证据**：
  ```rust
  let offset = entity.get("offset").and_then(serde_json::Value::as_u64)? as usize;
  let length = entity.get("length").and_then(serde_json::Value::as_u64)? as usize;
  let mention = text.get(offset..offset + length).unwrap_or("");
  ```
- **修复**：text 转 `Vec<u16>` 求 UTF-16 偏移，或遍历 char 累计 `utf16_length` 映射回字节区间；**同时把 363-364 行的 `?` 改 `continue`**（当前单个畸形实体会让整条消息归一化返回 None 被丢弃）。
- **验收**：测试——`"你好 @bot 帮忙"`（CJK 前缀）能正确判 to_me；畸形实体不丢整条消息。

#### - [ ] P1-6 wecom 把一切握手期异常映射为 `Startup`，connect_loop 视所有 `Startup` 为凭据失效 → 瞬时故障永久 Failed

- **位置**：`crates/latent-channel/src/wecom/mod.rs:241-245`（connect_loop 判定）+ `258-299`（establish 全部错误 → `Startup`）
- **问题**：connect_async 失败（DNS/TLS/服务端重启）、订阅发送失败、应答 15s 超时、ack 读/解析失败——全是瞬时类故障，却与"errcode≠0 凭据被拒"共用 `Startup` 分类，首次遇到即 `Failed` **永久退出不再重连**，只能重启 daemon。与模块"断线自动重连"自述及 connect_loop 注释（"订阅被拒 = Failed"）的意图矛盾。
- **修复**：握手/传输期错误改 `DeliveryFailed`（或新增专用瞬时分类），仅 `errcode != 0` 保留 `Startup`（Fatal）。
- **验收**：新测试——假服务器不可达 → 状态 `Disconnected` → 退避重连恢复；errcode≠0 → `Failed` 不重试。

#### - [ ] P1-7 qq `path` 配置未校验直接喂 axum，非法值启动即 panic

- **位置**：`crates/latent-channel/src/qq/mod.rs:274-276`
- **问题**：`.route(&path, ...)` 对不以 `/` 开头的 path panic；`path=="/"` 时与兜底 `.route("/")` 重复注册 GET → "Overlapping method route" panic。违反"trait 方法不 panic"与"配置非法走 `Config` → 退出码 78"两条硬规则（注释自称"同时接受 /"，恰好该值必炸）。
- **修复**：apply_config/start 时校验（必须以 `/` 开头、非 `/`、与兜底路由去重），非法返回 `ChannelError::Config`。

### 引擎与并发（latent-gateway）

#### - [ ] P1-8 裸词命中命令表，自然语言被劫持为命令

- **位置**：`crates/latent-gateway/src/auto_reply/commands.rs:53-61`；钉死错误行为的单测在 `commands.rs:164` 附近
- **问题**：`strip_prefix('/').unwrap_or(trimmed)` 让**所有**命令斜杠可选——"stop it" 杀 run、"new plan for tomorrow" 清空群共享会话、"Help me fix this" 吞消息回命令清单、"status update..." 回状态不进模型。实现自己引用的上游正则明确**只有 `/approve` 可裸词**（`/^\/?approve/`），其余必须 `/`（`/^\/(new|reset)(?:\s|$)/i`）。
- **修复**：仅 `/approve` 保留裸词（对齐上游），其余命令要求行首 `/`；同步修改单测 `parses_with_and_without_slash_and_case_insensitive`。若判定为有意设计，须在 GATEWAY_PLAN §8 登记偏离。
- **验收**：单测——"stop it"/"new plan"/"Help me" 不命中命令、进模型管线；"/stop"/"approve 3 deny"（裸词 approve）正常。

#### - [ ] P1-9 忙/闲判定与 steer/follow_up 注入之间的竞态窗口会静默滞留消息

- **位置**：`crates/latent-gateway/src/auto_reply/mod.rs:349-358`（run_turn try_lock 判忙）、`434/442`（queue_disposition）、`807`（inject_operator_message 忙分支同病）
- **问题**：core 侧注入通道只在 run 活跃期间被消费，最后一次收队在 `step_settling`（`latent-agent/src/loop_.rs:904-917`）。gateway 从"try_lock 失败"到"消息入队"之间若正在收尾的 run 恰好跨过最后一次收队，steer 的消息落在通道里**没有任何活跃 run 去消费**，滞留到该会话下一条消息触发新 prompt 才被吞入——对安静会话等于无回执丢失。（窗口宽度待复现验证，但结构性缺口成立；对照 subagent supervisor 有 `wait_idle → follow_up → continue_run` 兜底，gateway 忙路径没有。）
- **修复**：`execute_turn` 在 `wait_idle` 与 collect 冲刷之后复查 `queue_depths() != (0,0)`，非空则续跑——需把 latent-agent 已有的 `continue_run`（`agent.rs:347-360`）暴露到 `AgentSession`（session.rs:765 内部已在用）；或忙路径改走 `session.prompt()`（core 已把 AlreadyRunning 转 steer）。

#### - [ ] P1-10 防抖冲刷循环内联执行完整 turn：跨会话队头阻塞

- **位置**：`crates/latent-gateway/src/auto_reply/mod.rs:273-300`（`run_debounce_loop`，冲刷体在 283-289）
- **问题**：daemon 全局唯一冲刷任务里 `run_message(...).await` 同步执行（prompt + wait_idle 分钟级），一个用户的长 run 期间**所有其他会话**到期的防抖缓冲全部无法冲刷，回复延迟无上界（缓冲不丢但延迟任意长）。
- **修复**：冲刷出的每条消息 `tokio::spawn` 后继续循环（顺序性由各会话 run_lock 自身保证，跨会话本就并行）。
- **验收**：集成测试——会话 A 长任务运行中，会话 B 消息到期后能独立得到回复。

#### - [ ] P1-11 state.json 并发 save 可撕裂/丢失

- **位置**：`crates/latent-gateway/src/state.rs:99-109`
- **问题**：`save()` 锁内只 clone，写固定名 tmp（`path.with_extension("json.tmp")`）与 rename 在锁外。调用方分散在多个并发任务（SessionFactory::build、`/new`、pairing、sessions.reset、停机），其中 `/new` 的 `rebuild_session` 不持 registry 锁，可与另一 session key 的 `get_or_build` 并发 build → 两个 `save()` 交错写同一 tmp → rename 撕裂/失败。state.json 是重启 resume 唯一依据，损坏 = 下次启动按空状态起步（会话映射、已批准配对全丢）。
- **修复**：write+rename 全程持 `inner` 锁（序列化简单可靠），或 tmp 名拼 `pid+计数器` 唯一化。

#### - [ ] P1-12 busy 期间 `/new` 半切换持久化：state.json 与实际会话文件脱钩

- **位置**：`crates/latent-gateway/src/auto_reply/mod.rs:652-674`；core 侧 `crates/latent-runtime/src/assembly.rs:414-419`
- **问题**：`switch_new_session` 文档契约"流式期间调用方须先行拒绝"依赖调用方预检，gateway 的 `/new` 路径**不查 `is_streaming`** 直接调用。core 内部顺序：`create_session_in_dir`（新文件已落盘）→ `holder.set(Some(new_manager))`（**持久化 sink 已指向新文件**）→ `session.agent().reset()`（busy 时 Err）→ 返回 Err。gateway Err 分支只回"新建会话失败"：转录尾段继续 append 进**新**文件、state.json 仍记旧文件（state 写只在 Ok 分支）、磁盘多孤儿文件。重启 resume 到旧文件，静默丢失中途追加内容——违反"转录即真相"。
- **修复**：gateway 侧 `execute_session_command` 的 New/Reset 臂先查 `session.built.session.agent().state_snapshot().is_streaming`，忙则拒绝（`/compact` 同样预检）；core 侧把 `holder.set` 挪到 `reset()` 成功之后（纵深防御）。
- **验收**：集成测试——run 进行中发 `/new` 被拒且回复提示；结束后 `/new` 正常。

#### - [ ] P1-13 会话注册表全局锁跨 `factory.build().await`：任一会话冷启动卡死所有会话

- **位置**：`crates/latent-gateway/src/agents.rs:180-188`
- **问题**：`get_or_build` 持 registry `AsyncMutex` 跨整个 `factory.build(key).await`（MCP 子进程 × spec 数 spawn、settings 读盘、state.json 同步落盘——秒级起步）。期间**其他所有会话**的 `get_or_build`/`get`（含 /status、控制面 sessions.*、chat.send）全部排队。同 key 双建确实被防住了，但代价是全局串行。
- **修复**：先无锁查缓存 → 未命中放 per-key 构建锁（`HashMap<String, Arc<tokio::sync::Mutex<()>>>`）内 build → 完成后二次查缓存再 insert；registry 锁只护 map 读写。

#### - [ ] P1-14 gateway 直接依赖 latent-ai/agent/core/session 五件套，与计划/文档不一致且未登记

- **位置**：`crates/latent-gateway/Cargo.toml:21-27`
- **问题**：GATEWAY_PLAN §1 与本次 AGENTS.md 均写"latent-gateway 仅依赖 latent-runtime + latent-channel"，实际直接依赖 `latent-ai`、`latent-agent`、`latent-core`、`latent-session`、`latent-runtime`、`latent-channel` 六项。§8/§10 均无此偏离登记——代码与两份文档三方不一致。
- **修复（二选一）**：把所需类型经 latent-runtime re-export（照 latent-cli 兼容层做法，回归"仅依赖 runtime + channel"）；或改 AGENTS.md 依赖图 + GATEWAY_PLAN §1/§8 登记现状。**推荐前者**（保持"装配类型只从 runtime 出"的层纪律）。

---

## 3. P2（应当修，择要）

> 以下条目来自分域审查（含行号），未逐条人工复核，修复时以实际代码为准。

### 安全 / 配置

- [ ] **P2-1 `!shell` 项目级拒绝扫描的是合并后值**（`config.rs:434-465`）：全局合法 `!shell` + 项目文件存在（哪怕 `{}`）→ 误拒启动，打断 §4.2 明确允许的用法。修复：只扫项目文件自身的 Value 子树。
- [ ] **P2-2 渠道扩展键端到端配不了**（`config.rs` 类型化 schema + `channels.rs:99`）：qq `path`、telegram `apiBase`/`pollTimeoutSecs`、wecom `wsEndpoint`/`heartbeatIntervalSecs` 不在类型化字段，`deny_unknown_fields` 直接 78——AGENTS.md 宣称的"apiBase 可指向本地假服务器"用户实际配不出。修复：提升为类型化字段，或 §8 登记"仅测试注入"。放开 `path` 时须接 P1-7 的校验。
- [ ] **P2-3 审批请求 `handle.send().await` 无超时**（`approval.rs:108-129`）：渠道命令队列卡死（mpsc 64 满不消费）时 run 被无限挂住——既非"120s→Deny"也非"发送失败→立即 Deny"。修复：send 套 `tokio::time::timeout`（如 10s），超时走发送失败路径 Deny。
- [ ] **P2-4 审批通道选择不看实际连接状态**（`main.rs:413-437` + `channels.rs:184-190`）：`refresh_approval_transport` 取 ownerAllowFrom 第一个"有 handle"的渠道，`ChannelManager::handle` 只 attach 过就返回 Some（即使 disconnected/failed）→ 首个 owner 渠道断开时审批钉死死渠道。修复：选择时读 `status_snapshot()` 只挑 connected。
- [ ] **P2-5 envelope 头部清洗不滤换行/控制符**（`auto_reply/envelope.rs:14-17`）：只替换方括号；昵称含 `\n` 可把单行信封头拆成多行伪造新头部。修复：`sanitize_envelope_header_part` 追加 `\n\r` 与控制字符过滤（空格替代）。
- [ ] **P2-6 telegram reqwest 错误 Display 携带 URL，bot token 泄入日志**（`telegram/mod.rs:224-226` + `440-444`）：reqwest Display 形如 `error sending request for url (https://api.telegram.org/bot<TOKEN>/getMe)`。修复：错误串清洗 URL 的 token 段（违反 §5.5 日志 redact）。

### 渠道健壮性

- [ ] **P2-7 QQ/wecom 心跳判活缺失**（`qq/mod.rs:504-519` 自认"MVP 忽略"；规格 §2.6/§6 坑 3 明确要求）：NapCat 静默掉线时渠道僵尸在 Connected，出站白等 30s 超时。修复：记录最近心跳时间，超过 N 个周期无帧主动断开清理。wecom 侧 30s ping 同查。
- [ ] **P2-8 shutdown 不传播到长驻任务**（telegram `poll_loop` mod.rs:174、qq axum server mod.rs:279、wecom `connect_loop` mod.rs:200）：`ChannelHandle::shutdown` 只停命令循环，三处长驻任务继续收发。当前仅 Ctrl-C 进程退出掩盖问题；未来渠道热重载会双轮询（§6 坑 4 的 409）。修复：start 内建 `CancellationToken`，shutdown cancel，三任务 select。
- [ ] **P2-9 mentionPatterns 每条群消息重新编译正则**（`mention_gating.rs:108-111`；gateway 每群消息调用 `auto_reply/mod.rs:556`）：Regex::new 重操作且非法 pattern 每条重复诊断。修复：构造期预编译存入 `MentionConfig`。注意同文件 `MentionConfig` derive Default 使 `require_mention=false`（上游默认 true），是 §10 A3 同型脚枪，顺手改手写 Default。
- [ ] **P2-10 `Segment` serde internally-tagged + newtype 变体序列化必炸**（`types.rs:61-81`）：`#[serde(tag="type")]` 遇 `Text(String)` newtype 变体运行时报错。当前全仓无人序列化 Segment（潜伏雷）。修复：改 untagged 或各变体改 struct 变体，并补序列化单测。
- [ ] **P2-11 telegram 入站 message_id 用 `"tg-{update_id}"`**（`telegram/mod.rs:403-406`）：平台 `message.message_id` 未提取，`OutboundMessage.reply_to` 一旦回填必 400（Bot API 要 Integer 且 update_id≠message_id）——"reply_to_message_id 实现 Reply 段"的验收声明不成立（当前无人设 reply_to，潜伏）。修复：normalize 增加平台 message_id 或以之为主键、另设去重键。

### 引擎

- [ ] **P2-12 三处 `let _ = session.prompt(...)` 吞错**（`auto_reply/mod.rs:395/407/802`）：失败无日志无回执（collect 冲刷分支还丢已合并文本）。修复：至少 eprintln 诊断；能回执处发失败提示。
- [ ] **P2-13 `handle_queue_overflow` 用全局 queue_settings 回执 + 两个死参数**（`mod.rs:486-522`）：判溢出用会话级 cap（`/queue` 可覆盖），回执文案却读全局 `self.queue_settings`，数值可能不符；`let _ = session; let _ = text;` 是掩盖签名失配的死参数。修复：传 `session.pending.settings()`，删假参数。
- [ ] **P2-14 typing 间隔配置分叉 + 死旋钮**（`main.rs:230-233`、`config.rs:63-65`）：`typingIntervalSeconds` 只传 Gateway 未传 SessionFactory（泵补开的 typing 用另一份配置）；config 注释"默认 6 = 3000ms"算术错误（6s≠3s）。修复：抽 `build_typing_config(config)` 两处共用，或删字段修注释。
- [ ] **P2-15 NO_REPLY 静默语义缺失**（`reply_dispatcher.rs:92-95`）：只跳过空文本，模型对"收到但无需回应"照常回群（§4.8 承诺未兑现，gateway 也未注入相关约定）。修复：出站识别标记丢弃，或系统提示词注入约定 + 配套过滤。
- [ ] **P2-16 sendChain 消费者可能无限期阻塞**（`reply_dispatcher.rs:100-114` + `agents.rs:300-305`）：渠道发送悬挂（qq/wecom WS 对端黑洞）→ 256 槽满 → pump `AgentSettled` → `send_final().await` 阻塞 → SessionBridge 串行广播阻塞 → agent 循环停摆 → run_lock 永久被持，会话后续消息全部入队直至 cap 丢弃。fail-closed 是计划取舍，但"拖慢"与"永久卡死"之间缺兜底。修复：每段发送加超时（如 60s，超时记诊断放行下一段）。
- [ ] **P2-17 chunk 括号感知断点只认 ASCII 括号**（`chunk.rs:90-110`）：中文全角 `（）【】｛｝` 不计入 depth，聊天场景括号内换行被当断点。修复：depth 匹配表加全角对。
- [ ] **P2-18 e2e 验收缺口**（`tests/e2e.rs`）：①防抖窗口>0 的 gateway 级路径全部用例配 `debounceMs: 0`，`run_debounce_loop` 冲刷未被 e2e 走过；②collect 的 drop=old/summarize 冲刷路径无集成验证；③事件泵背压语义（进度丢/final 不丢）无测试；④双会话并发审批 id 路由无测试；⑤`/compact` 不在 15 例中；⑥`channel_echo_messages_are_dropped_by_channel`（e2e.rs:790-804）名不副实——mock 渠道不丢自消息，断言本体写在注释里指向 tests/qq.rs，建议改名或删除。

---

## 4. P3（择机，低风险）

- [ ] 去重表满载 O(n)=4096 全表扫描逐条淘汰（`auto_reply/mod.rs:140-155`）——改周期 retain 或记录插入序。
- [ ] `flush_due` 文档称"按 key 插入序产出"，实为 HashMap 随机迭代序（`debounce.rs:116-118`）——换有序容器或改注释。
- [ ] `sent_message_ids` 满 4096 整表 `clear()`：清空瞬间 reply_to_me 误判（`qq/mod.rs:175-181`）——改 FIFO 淘汰。
- [ ] typing `stop(self)` 后 Drop 再触发一次 off（`typing.rs:168-184`，off 发两次）+ stop 与 keepalive 极小竞态窗口。
- [ ] telegram 轮询路径 429 被吞、统一 3s 重试，忽略服务端 retry_after（`telegram/mod.rs:246-254/314-318`）。
- [ ] wecom msgid 缺失兜底 `"unknown"`：去重表把所有无 msgid 消息视作同一条丢弃（`wecom/mod.rs:459-463`）——缺失时跳过归一化或生成随机键。
- [ ] chunk 裸 ``` 行作段尾 open fence 时漏补闭合；CRLF 行尾风格混用（`chunk.rs:175-179`，纯外观）。
- [ ] `first_in_batch` 从不复位："首段不延迟"只对史上第一条生效（`reply_dispatcher.rs:71/97-99`）。
- [ ] `/activation` 私聊完全无 owner 校验（`mod.rs:617-637/740-748` 对照）——计划 §4.7 写"仅 owner"，与 `/queue` 双重校验口径不一致；统一即可。
- [ ] 控制面 `AuthThrottle.failures` 仅成功时移除，公网 bind 下慢性泄漏（`control/auth.rs:29`）。
- [ ] 控制面每请求无限 `tokio::spawn`，慢客户端可堆积在途任务（`control/server.rs:199-208`）——加 per-connection 在途信号量。
- [ ] resume 路径每次 build 覆盖 `createdAt`（`agents.rs:96-105`）——语义漂移为"最近 build 时刻"。
- [ ] `run_message` 恒传空 `account_id`，`per-account-channel-peer` 档退化为 `unknown`（`mod.rs:316-322`，MVP 可接受、多账户时是坑）。
- [ ] `session.reset` 接受 daily/idle 但策略未执行（`config.rs:495-500`）——validate 拒绝或文档标注"配了等于没配"。
- [ ] 死代码/死 API：`Shared<T>`（plugin.rs:207）、`WecomStream`（wecom/mod.rs:486）、`server.rs:76 let _ = state.gateway.started_at`；queue/drop 映射在 `main.rs:212-221` 与 `Gateway::new:92-101` 重复两份。
- [ ] 拼写/过时注释：`"wemos 连接失败"`（wecom/mod.rs:260）；credential.rs:8-9 引用不存在的 `resolve_for_config`/`credential_source_allowed`；types.rs:1 "Koichi"（应为 Koishi）；wecom 模块注释"新连接踢旧连接"实际未实现。
- [ ] mock 渠道 `dmPolicy` 不在 `validate()` 枚举校验内（qq/wecom/telegram 有）。

---

## 5. 已核实无问题、无需重审的兑现项

控制面/QQ token 常数时间比较；`/approve` owner 双保险 + pending 表全局单例 + resolve 即 remove + close_all 落 Deny + 发送失败立即 Deny；审批目标不受入站消息左右（仅 ownerAllowFrom 配置驱动）；chat.send sender 服务端固定 operator 且显式拒 params.sender、operator 不经命令解析无法应答审批/切模式；群会话命令天花板 `owner_only_in_group` 覆盖完整（除 /status /help）；pairing 主路径 `/dev/urandom` + 码集无偏 + 剔除 0O1I + TTL 1h + pending≤3 + 批准仅授 DM + CLI 全走控制面薄客户端；state.json 无路径穿越（key 只作 HashMap 键）；config.get redact 覆盖全部凭据键；QQ accessToken 空/缺拒绝启动、Bearer 走 Header、自消息私聊+群聊源头丢弃；`$ENV` 缺失显式 Err、`!shell` 执行有 env 白名单/5s 超时/16KB 上限；事件通道有界（broadcast 256 + per-connection 64，Lagged 有诊断）；raw OneBot JSON 未进 prompt（提示注入面 = envelope 纯文本，与 §5.5 定位一致）；防抖首条固定 ×5 封顶/2048 键上限/合成 id；chunk 4000/三级断点；typing 3000ms/60s TTL/2 次失败/无 gate；mention 三级优先级 + 大小写不敏感 + 坏正则跳过；QQ echo oneshot 三路清理无泄漏、same_channel 防误清（B4）、单帧 1MiB/并发 8 配置项存在；wecom 出站 mpsc 有消费循环（B1）、订阅被拒 → Failed 不重试、delivered 端到端断言；credential 与 latent-web 版语义对齐（仅增 is_shell_source）；全 crate 手写 Default 未被误 derive（A3）；serde A1/A2 落实；测试等待全部有界（D1）。

---

## 6. 文档同步与偏离登记（修代码时一并处理）

**未登记的计划偏离（应补进 GATEWAY_PLAN §8）**：
1. gateway 依赖面扩大（P1-14，随 P1-14 的拍板结果同步）。
2. 裸词命令匹配（P1-8；若有意保留须登记并说明与 §4.7 引用正则的关系）。
3. `channels.mock` 配置节（§4.2 schema 无 mock 渠道；测试基建合理，登记进 §4.2/§8——目前 AGENTS.md 写了、§4.2 没写，各说一半）。
4. `inject_operator_message` 未走"同一 dispatch 管线"（§4.12 字面要求；实现绕过去重/授权/防抖直接进 run——与 §5.4 operator 语义一致，但偏离未登记）。

**文档与实现不一致**：
1. AGENTS.md 聊天栈图"latent-gateway 仅依赖 latent-runtime + latent-channel" vs Cargo.toml 实际六依赖（同 P1-14）。
2. AGENTS.md gateway.json 行把 mock 渠道写成有"凭据/textChunkLimit"——实际 mock 无凭据、chunk_limit 硬编码 4000；该行也未列已有的 `thinkingLevel`/`typingIntervalSeconds`/`bindings`/`reset.atHour/idleMinutes`。
3. AGENTS.md telegram 行"apiBase 可指向本地假服务器"——经 gateway.json 端到端配不了（P2-2）。
4. §7 验收清单两项声称完成但无对应测试（事件泵背压、双会话并发审批路由，见 P2-18）。
5. GATEWAY_PLAN §3.3 `axum = "=0.8.7"` 未同步为实际 `=0.8.9`（§10 C1 已按实记录，软性漂移，顺手改）。

---

## 7. 待确认决策项（修复前需拍板）

1. **P1-4 威胁模型**：是否将"用户在不可信 repo 内手动启动 gateway"视为已防御场景？是 → 敏感字段覆盖降为显式警告；否 → 升级处理（拒绝启动/全局只读）。
2. **P1-8 裸词命令**：是否有意的产品决策？是 → §8 登记 + 评估英文自然语言误拦面；否 → 按上游正则收紧。
3. **wecom 协议帧字节级复核**：实现用 `aibot_subscribe`(bot_id+secret 直连) + 文本帧 `{"cmd":"ping"}`，计划文档表述为"BotId+Secret 换 ticket"；测试断言的是自实现帧格式（自证循环）。按 §10 C6 纪律对照官方文档复核：有无独立 ticket 步骤、心跳是文本帧还是 WS 协议层 ping。
4. **`$ENV` + `apiBase` 外泄面**：项目级 gateway.json 可同时提供 `botToken: "$ENV:XX"` 与 `apiBase` 指向任意外部端点（token 拼进 URL），解析出的 env 值会被发往该端点。需确认是否有意接受，或项目级禁用 `$ENV`/apiBase 覆盖。
5. **DmScope::Main / GroupScope::Main 折叠**（`session_key.rs:74/79`）：两者同时配 main 时群聊与私聊共享 `agent:main:main` 同一会话——需对照上游 `buildAgentPeerSessionKey` 确认是否有意。
6. **附件（图片/文件）不进模型上下文**：envelope 文案"仅附件"与实际丢弃行为需对齐；MVP 是否明确不做视觉。

---

## 8. 回归验证清单

每批修复后：

```bash
cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets
test -z "$(cargo tree -p latent-channel | grep -E 'latent-(core|ai|agent|session|tools|web|sandbox|tui|runtime|gateway)')"
test -z "$(cargo tree -p latent          | grep -E 'latent-(channel|gateway)')"
test -z "$(cargo tree -p latent-gateway  | grep 'latent-tui')"
```

新增测试最低集合（对应 P0/P1 验收条目）：
1. 非 owner `/mode` 变体拼写被拒（P0-1）；
2. `$ENV` 凭据 → start_enabled → 假平台认证通过（P0-2）；
3. 同 IP 换端口 5 次失败触发冷却（P1-1）；
4. CJK 前缀 mention 判 to_me + 畸形实体不丢消息（P1-5）；
5. wecom 连接失败 → Disconnected → 重连；errcode≠0 → Failed（P1-6）；
6. 非法 qq path → Config 错误（P1-7）；
7. "stop it"/"new plan" 不命中命令（P1-8）；
8. run 进行中 `/new` 被拒（P1-12）；
9. 会话 A 长任务期间会话 B 防抖消息独立回复（P1-10）；
10. state.json 并发 save 无撕裂（P1-11，可用多任务并发 save + 内容断言）；
11. 事件泵背压（进度丢/final 不丢）+ 双会话并发审批路由（补 §7 缺口，P2-18）。
