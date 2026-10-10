# rm 后再 cp:原地覆盖已签名的可执行文件会命中 macOS 内核的陈旧签名缓存
# —— 轻则下次 exec 被间歇性 SIGKILL(killed),重则杀掉还在运行的旧实例。
# rm 让目标拿到新 inode,内核对新文件重新校验内嵌签名,不会再 killed。
cargo clean && cargo build --release -p latent && rm -f ~/.local/bin/latent && cp target/release/latent ~/.local/bin/

cargo publish -p latent-ai
cargo publish -p latent-agent
cargo publish -p latent-tui
cargo publish -p latent-sandbox
cargo publish -p latent-session
cargo publish -p latent-tools
cargo publish -p latent-web
cargo publish -p latent-core
cargo publish -p latent