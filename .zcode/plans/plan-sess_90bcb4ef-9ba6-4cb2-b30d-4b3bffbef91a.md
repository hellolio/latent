## 修复计划（权限判定器审查问题，P0 + P1 + P2 全修）

### 1. [P0] `$(( ))` 算术展开拦截命令替换 — `crates/rpi-core/src/permission/shell.rs:348-352`
把盲扫的 `skip_balanced_parens` 换成专用 `skip_arithmetic_body`：跟踪括号配对，遇到反引号或 `$` 后随 `(` 立即返回 false（保守拒绝，嵌套算术也一并拒绝）。挡住 `echo $(( $(rm -rf src) + 1 ))`。新增测试：含命令替换/反引号的算术展开拒绝，`echo $((1 + 2))`、`echo $(( (1+2)*3 ))` 放行。

### 2. [P1] `sed` 尾部选项守卫 — `shell.rs:691-699`
`sed_words_are_safe` 在推入脚本词后推进 `i`，仿照 awk 对 `words[i..]` 检查：任何选项形态词（`-` 开头且非 `"-"`）一律拒绝。挡住 `sed 's/a/b/' -i f`、`sed 's/a/b/' --in-place f`。现有 sed 测试全部不受影响。

### 3. [P1] git 家族写形态守卫 — `shell.rs:557-563`
扩展现有 `has_write_flag`，新增 git 分支：
- `git tag` / `git branch`：所有参数必须是已知只读 flag（`-l`/`--list`/`-a`/`-r`/`-v`/`-vv`/`--show-current`/`--sort=...` 等）；非 flag 参数（`git tag evil` 创建 ref）或写 flag（`-d`/`-D`/`-m`/`--force`...）判为非只读。`git tag -l 'v*'` 这类 pattern 形式会被保守拒绝，注释说明取舍。
- `git remote`：仅放行无参、`-v`/`--verbose`、`get-url`/`show` 子命令；`add`/`remove`/`update`/`prune` 拒绝。
- 整个 git 家族：拒绝 `--output`（两参形式）与 `--output=...`（`--output-indicator-*` 不受影响）。挡住 `git diff --output=foo.txt` 免审写文件。
- 新增测试：`git tag evil`、`git tag -d v1`、`git branch -D x`、`git remote add o url`、`git diff --output=foo.txt`、`git show --output foo.txt` 拒绝；`git tag -l`、`git branch -a`、`git remote -v`、`git diff --output-indicator-new='#'` 放行。

### 4. [P1] `git -c` 与危险环境变量 — `shell.rs:806-894`
- **`git -c` 整体拒绝**（最保守方案）：从 `is_git_global_flag` 和剥离循环移除 `-c`，`git -c core.pager=... log` 无法命中前缀表 → 非只读。注释说明后续可演进为可执行配置键黑名单。
- **env 危险变量**：`env` 剥离循环中，`VAR=x` 的变量名命中黑名单（`GIT_*` 全部、`PAGER`/`EDITOR`/`VISUAL`/`LD_PRELOAD`/`DYLD_*`/`BASH_ENV`/`ENV`，大小写不敏感）即停止剥离 → 非只读。挡住 `env GIT_PAGER=evil git log`。

### 5. `cargo check` 移出只读表 — `shell.rs:45`
删除该条目（会执行依赖的 build.rs 第三方代码，判定器是最后防线）；`cargo tree`/`cargo metadata` 不执行构建脚本、保留。表格处加注释说明取舍。`prefix_table_hits` 测试改为断言 `cargo check` 不再只读。

### 6. [P2] `load_skill` 归类 ReadOnly — `crates/rpi-core/src/permission/types.rs:73-90`
`classify_tool` 增加 `"load_skill" => ToolRiskClass::ReadOnly`（纯读 SKILL.md 进上下文），修复 Plan 模式被当 External 拒绝的 bug。补测试。

### 7. [P2] engine.rs 清理 — `crates/rpi-core/src/permission/engine.rs:117-124, 181-194`
- 删除 117-119 的 ReadOnly 提前返回，让 match 的 ReadOnly 分支成为唯一路径（行为等价：`cache_hit` 对 ReadOnly 恒为 false）。
- 把 `shell.rs` 的 `prefix_matches` 改为 `pub(crate)`，engine.rs 的 Confirm deny 规则匹配改用同一实现，消除重复。

### 8. [P2] `2>` IO number 空词 — `shell.rs:277-297`
`word.text.clear()` 后同步 `in_word = false`，flush 不再推入空词 token，修复 `2>/dev/null ls` 假阴性。新增测试。

### 9. [P2] 无沙箱平台 Plan 模式运行期兜底 — `engine.rs:157-172`
Plan/Shell 分支增加 `!self.sandbox_available` → Deny（与装配期降级矩阵一致：无沙箱平台 Plan 不执行 bash，即使模式切换后 bash 仍激活也兜得住判定器被绕过的场景）。补测试。同步更新 `apply_mode`（session.rs:529-533）与 `SandboxSpawnHook`（assembly.rs:605-607）注释。

### 验证
- `cargo test -p rpi-core`（新增约 20 个断言）+ `cargo test --workspace`
- `cargo clippy --workspace` 无新告警