修复沙箱三平台问题（macOS 主因 + Linux 隐患 + Windows 降级矩阵补齐）

## 已确认的问题清单

**macOS（本机实测失效）**：`seatbelt_probe_executes()`（latent-sandbox/src/lib.rs:85-91）用已被新 macOS 移除的 `/bin/true` 探测 + profile 缺 `(allow file-read*)`（dyld 被拒 exit 134）→ 探测恒失败 → `detect_availability()=None` → 沙箱钩子原样放行（assembly.rs:772-774,797-799）→ 全部命令裸跑。sandbox-exec 本身可用（修正 profile 后实测 exit 0）。

**Linux**：① bwrap 只做 PATH 存在性检查（bwrap.rs:19-26），userns 禁用且非 setuid 的机器上每条沙箱命令运行期必败；② `wrap_command` 可写根路径未过 `shell_quote`（bwrap.rs:43-56），含空格路径炸命令；③ Landlock 靠读 `/sys/kernel/security/lsm`（landlock.rs:28-32），容器内 securityfs 未挂载 → 漏报。

**Windows（降级矩阵本身是坏的）**：无沙箱后端；且 ① 判定器零 PowerShell 规则（shell.rs），`Get-Content` 等只读命令全判 Unknown → 无沙箱 Plan 下全 Deny；② Plan 基线给 `bash`（types.rs:245-251），bash 工具 spawn `sh`（bash.rs:167）——原生 Windows 无 `sh`，`powershell` 反而不在基线里。

**通用**：create_sandbox Err 被 `.ok()` 吞成 None 并缓存 → 该模式永久静默裸跑（assembly.rs:785-788，违背 lib.rs:100-102"绝不静默裸跑"）；detect 双探测 TOCTOU；commandPrefix 在沙箱外执行且不经扩展钩子审计（bash.rs:659-662）；SANDBOX_DENIAL_NOTICE 文案过期（bash.rs:710-711 "no network"，Plan 实际放网）；assembly.rs:901 注释与 types.rs:240-242 实际降级行为不符；零测试覆盖。

用户已确认：Windows 本次只补降级矩阵（OS 后端单独立项）；commandPrefix 移到沙箱包装之前。

## 改动清单

### A. latent-sandbox
1. **lib.rs `seatbelt_probe_executes`**：目标改 `/bin/sh -c true`；profile 改 `(version 1)(deny default)(allow file-read*)(allow process-exec* (subpath "/bin"))` 并补 `(allow sysctl-read)(allow mach-lookup)(allow process-fork)`（对齐真实 profile 基线，防其他 macOS 版本 dyld 变体）；更新 doc 注释。
2. **lib.rs `create_sandbox` 签名**：新增首参 `availability: SandboxAvailability`（装配期探测一次传入），删除内部二次 `detect_availability()` —— 消除 TOCTOU 与每模式重复探测；同步更新调用方与文档注释。
3. **bwrap.rs 探测升级**：PATH 存在后再做真实执行探测 `bwrap --unshare-all --ro-bind / / --dev /dev --proc /proc -- /bin/sh -c 'true'`（OnceLock 缓存保持）；失败 → 返回 false → 自动落到 Landlock 备选。
4. **bwrap.rs `wrap_command` 路径引号**：可写根与 `.git/.latent/.env` 只读覆盖路径全部过 `shell_quote`。
5. **landlock.rs `kernel_support_detected`**：改为 landlock crate 真实 ABI 探测（`ABI::new_enforceable()`，即 landlock_create_ruleset syscall，无 unsafe；helper 已用同一 API），securityfs 未挂载也能正确探测。
6. **lib.rs `system_tmp_dirs`**：改用 `std::env::temp_dir()`（跨平台 TMPDIR/TEMP/TMP）+ unix 保留 `/tmp` 兜底；修 Windows 下 WorkspaceWrite 可写根缺临时目录（macOS 行为不变）。

### B. latent-core（权限判定）
7. **types.rs `mode_baseline_tools` 平台感知**：Windows Plan 基线用 `powershell` 替代 `bash`（`#[cfg(windows)]`）；同步修 assembly.rs:901 过期注释为实际语义（无沙箱平台 Plan 保留 shell 工具、由三态判定兜底）。
8. **shell.rs PowerShell 判定表**（保守最小集，fail-closed）：
   - engine.rs `ToolRiskClass::Shell` 分支按 `ctx.name` 分派（ToolCallCtx 已有 name 字段，无需改结构）：`bash` → 现有表，`powershell` → 新表；
   - 新表：只读 cmdlet 前缀（Get-Content/Get-ChildItem/Get-Item/Get-Location/Get-Process/Get-Service/Get-Command/Get-Help/Get-Member/Test-Path/Select-String/Select-Object/Sort-Object/Where-Object/Measure-Object/Compare-Object/Format-*/Out-String/Out-Host/Set-Location）+ 常用别名（ls/cat/cd/pwd/gci/gc/gwmi 等）；明确写前缀（Set-Content/Add-Content/Out-File/New-Item/Remove-Item/Copy-Item/Move-Item/Rename-Item/Clear-Content/Set-Item/Set-ItemProperty/Start-Process/Stop-Process/Stop-Service/Restart-*/New-PSDrive）；联网查询（Invoke-WebRequest/Invoke-RestMethod/Resolve-DnsName/Test-NetConnection）；其余 Unknown；`>` 重定向检测复用现有逻辑；
   - 表驱动纯函数 + 单测（跨平台可跑）。

### C. latent-cli（装配层）
9. **assembly.rs `SandboxSpawnHook` fail-closed**：`create_sandbox` Err 不再 `.ok()` 吞掉 → `rewrite()` 返回 `Err("沙箱构造失败: …")` → bash 工具拒绝执行（bash.rs:651-656 既有语义）；失败不写缓存；装配期探测的 availability 存入钩子；沙箱工厂改为可注入字段（默认 `create_sandbox`）便于单测。
10. **bash.rs commandPrefix 前移**：prefix 拼接从 hook 之后（659-662）移到 hook 之前 —— prefix 随整条命令被沙箱包装、被扩展钩子审计；每条命令的执行体全部落进沙箱。
11. **bash.rs 文案**：`SANDBOX_DENIAL_NOTICE` 的 "plan mode:read-only, no network" 修正为实际策略（文件系统只读、网络按策略放行）。

### D. 测试
12. latent-sandbox：`#[cfg(target_os = "macos")]` 断言 `detect_availability()==MacosSeatbelt`（本回归的直接回归测试）；bwrap 探测 argv 纯函数测试 + Linux-gated 条件真探测（后端缺失跳过）；bwrap 空格路径引号测试；tmp 目录跨平台测试。
13. latent-core：PowerShell 判定表测试（只读/写/联网/Unknown/别名/重定向各路径）。
14. latent-cli assembly 单测：注入失败工厂 → rewrite 返回 Err（fail-closed）；注入假后端 → Plan 模式 rewrite 输出含包装前缀且 prefix 已并入被包装命令；macOS-gated 真后端断言输出含 `sandbox-exec`。

### E. 验收
- `cargo build --workspace` + `cargo test --workspace` + `cargo clippy --workspace --all-targets` 全绿零警告（硬门槛）。
- 本机手工验收：`cargo run -p latent-cli -- --mock` Plan 模式跑 `ls` 类只读命令确认 sandbox-exec 包装生效（不再有启动时"未检测到可用沙箱"stderr）、写命令仍 Deny、未知命令沙箱内放行。

### F. 范围外（记录不修）
- Windows OS 级沙箱后端（AppContainer/受限令牌）：单独立项，需真 Windows 机器验收。
- `!`/`!!` 透传不经权限与沙箱：对齐 pi 的有意设计。
- resume 时会话模式由会话文件 ModeChange entry 驱动：行为正确。