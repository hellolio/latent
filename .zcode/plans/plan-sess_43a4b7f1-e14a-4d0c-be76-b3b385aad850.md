## 方案（三部分）

### 1. 模型可见提示词英文化

Deny reason → ToolBlock → 错误 tool result 文本，全部进模型上下文。以下改英文：

- engine.rs:127 → `Plan mode is read-only. Finish the plan and exit Plan mode before modifying files`
- engine.rs:167 → `Plan mode allows read-only commands only: {command}`
- engine.rs:196 → `Command matched a deny rule: {command}`
- engine.rs:211 → `Extension tools are not allowed in Plan mode`
- engine.rs:265 → `(missing path)`
- hooks.rs:216 → `User denied the operation: {detail}`
- hooks.rs:221 → `用户中止` → `Aborted by user`
- loop_.rs:1511 → `已中止` → `Aborted`
- latent-sandbox lib.rs:123 → `No sandbox available on this platform`；landlock.rs 全部 11 条 Err 文本英文化（`landlock sandbox supports Linux only`、`failed to add read rule`、`failed to apply seccomp filter` 等）
- subagent/registry.rs:189/198/201/214/219 五条英文化（如 `run id \`{id}\` not found; existing runs: {ids:?}`）

纯 UI 中文保持不动：ApprovalReason::message()（types.rs:134-149）、审批 overlay（handlers.rs:921-940）、stderr 日志。

### 2. 新增：沙箱拦截后的事后提示（bash.rs:429-474）

- `execute()` 中 hook 改写后检测 `effective` 是否含沙箱包装标记（`sandbox-exec`/`bwrap`/landlock helper 特征），记 `sandboxed: bool`；
- `code != 0` 且 `sandboxed` 且输出含 `Operation not permitted`（EPERM 是 seatbelt/landlock/seccomp 拦截的统一表现）时，在错误消息末尾追加英文提示：

  `[latent] sandbox notice: this command ran inside the OS sandbox (plan/confirm mode); file writes outside temp dirs and network access are blocked by the kernel. The failure above is likely caused by the sandbox, not the command itself.`

- 措辞用 "likely"：EPERM 也可能来自普通文件权限，事后判断是概率性的，不改变判定、只补信息；
- 覆盖两个场景：Plan 模式下白名单命令的边缘拦截、Confirm 模式 WorkspaceWrite 沙箱（如批准的 `npm install` 被网络关掐死）。

### 3. 判定策略（已确认，无代码改动）

Confirm 保持现有保守流（只读放行 / deny 规则拒绝 / 其余 Ask 审批）；Plan 保留只读校验 + 沙箱兜底 + 新增事后提示，三层互补。

## 测试

- 按授权同步 4 处过时断言：engine.rs:345、hooks.rs:338、permission_hooks.rs:200、e2e/test_plan_mode.py:48（期望改为英文文案）；
- 追加（不改现有 case）：
  - bash.rs 测试：伪 spawn hook 返回含 `sandbox-exec` 的改写命令 + 命令失败输出含 `Operation not permitted` → 断言错误消息含 sandbox notice；非沙箱命令同样输出 → 不追加；
  - 文案英文化的正向断言随 4 处同步覆盖。

## 验证

`cargo test -p latent-core -p latent-tools -p latent-agent -p latent-sandbox`、`cargo build --workspace`、`cargo test --workspace` 全绿；grep 确认模型可见路径无残留中文（UI 路径保留中文）。