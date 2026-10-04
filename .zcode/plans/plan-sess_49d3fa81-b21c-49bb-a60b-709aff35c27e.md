修改 /Users/kin/Documents/10source/latent/crates/latent-web/src/http.rs 第 76 行的 User-Agent 字符串,把上游项目的地址换成用户自己的仓库:

```rust
.user_agent("Mozilla/5.0 (compatible; latent-web/1.0; +https://github.com/hellolio/latent)");
```

再同步检查 crates/latent-web/src/ 下是否有其他地方硬编码了同样的 UA 字符串(经 grep 只有 http.rs:76 一处),无其他改动。改完跑 `cargo check -p latent-web` 验证编译通过。