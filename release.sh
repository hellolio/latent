cargo clean && cargo build --release -p latent && cp target/release/latent ~/.local/bin/

cargo publish -p latent-ai
cargo publish -p latent-agent
cargo publish -p latent-tui
cargo publish -p latent-sandbox
cargo publish -p latent-session
cargo publish -p latent-tools
cargo publish -p latent-web
cargo publish -p latent-core
cargo publish -p latent