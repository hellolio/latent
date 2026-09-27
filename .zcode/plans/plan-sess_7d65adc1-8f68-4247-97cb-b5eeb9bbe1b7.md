# reserveTokens 百分比语义

## 语义定义
`reserveTokens` 配置值 v：
- `v >= 1.0`：按绝对 token 数（现状不变，16384 = 16k）
- `0 < v < 1.0`：按 context_window 的百分比（0.1 = 10%），在 `should_compact` 调用时用当前模型的窗口解析（模型可被 /model 切换，窗口是运行期才知道的）
- `v <= 0`：归零（不预留；等价于只在 overflow 边缘触发）
- 100% 无法表达（1.0 = 1 token），文档注明；serde 用 f64 兼容整数写法 `16384` 和浮点写法 `0.1`

## 改动点
1. `crates/rpi-session/src/compaction.rs`
   - `CompactionSettings.reserve_tokens: u64 → f64`（去掉 `Eq` derive，注释说明语义；serde f64 天然兼容 JSON 整数）
   - `DEFAULT_COMPACTION_SETTINGS.reserve_tokens = 16_384.0`（默认仍为绝对值，与 pi 对齐）
   - 新增 `reserve_tokens_for_window(reserve_tokens: f64, context_window: u64) -> u64` 纯函数做解析
   - `should_compact` 改用它解析实际预留量
   - 导出新函数；单测：百分比解析（0.1 × 32000 = 3200）、边界（恰好触发/差一）、绝对值不受影响、`v <= 0` 归零、旧整数 JSON 反序列化兼容
2. `crates/rpi-cli/src/assembly.rs`
   - `CompactionConfig.reserve_tokens: u64 → f64`（Default 与 From 同步）；补 serde 解析单测（`0.1` / `16384` 两种写法）
   - 装配测试里 `reserve_tokens: 0` 改 `0.0`
3. `crates/rpi-cli/src/modes/interactive/handlers.rs`（/session 展示）
   - `v < 1.0` 显示为 `reserve 10% (3200 tok of 32000)`，否则显示绝对值
4. 文档：docs/06-session-compaction.md 差异记录补一句百分比语义

## 验证
`cargo test --workspace` 全绿；手动确认现有 `{"enabled":false,"reserveTokens":16384,"keepRecentTokens":20000}` 配置行为不变。