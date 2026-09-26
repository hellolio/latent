//! rpi —— pi agent 的 Rust 重写(M6:四种运行模式,08 文档)。
//!
//! 用法:
//! - `rpi [--mock] "prompt"`                              print 模式(非 TTY 自动)
//! - `rpi --mode interactive`                             TUI 聊天(TTY 必需)
//! - `rpi --mode json "prompt"`                           事件 JSONL 上线
//! - `rpi --mode rpc`                                     stdio JSONL RPC(编辑器集成)
//! - `rpi --provider <id> [--model <id>] "prompt"`        指定真实 provider
//! - `rpi --mcp-mock-server`                              MCP 扩展自检服务端

use std::io::IsTerminal;
use std::sync::Arc;

use rpi_ai::default_provider_endpoint;
use rpi_cli::assembly::{load_mcp_server_specs};
use rpi_cli::modes;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return;
    }
    if let Err(err) = run(&args).await {
        eprintln!("rpi: {err}");
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Interactive,
    Print,
    Json,
    Rpc,
}

enum Args {
    MockExtensionServer,
    Run { mode: Mode, provider: Option<String>, model: Option<String>, prompt: Option<String> },
    Invalid(String),
}

fn parse_args(args: &[String]) -> Args {
    if args.iter().any(|a| a == "--mcp-mock-server") {
        return Args::MockExtensionServer;
    }
    let mut mode: Option<Mode> = None;
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut mock = false;
    let mut prompt_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" if i + 1 < args.len() => {
                mode = Some(match args[i + 1].as_str() {
                    "interactive" => Mode::Interactive,
                    "print" => Mode::Print,
                    "json" => Mode::Json,
                    "rpc" => Mode::Rpc,
                    other => return Args::Invalid(format!("未知模式: {other}")),
                });
                i += 2;
            }
            "--provider" if i + 1 < args.len() => {
                provider = Some(args[i + 1].clone());
                i += 2;
            }
            "--model" if i + 1 < args.len() => {
                model = Some(args[i + 1].clone());
                i += 2;
            }
            "--mock" => {
                mock = true;
                i += 1;
            }
            arg if arg.starts_with("--") => return Args::Invalid(format!("未知参数: {arg}")),
            arg => {
                prompt_parts.push(arg.to_string());
                i += 1;
            }
        }
    }
    if mock {
        provider = Some("mock".into());
    }
    // 模式自动判定(pi 语义):未指定时,交互终端 → interactive,否则 → print
    let mode = mode.unwrap_or_else(|| {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            Mode::Interactive
        } else {
            Mode::Print
        }
    });
    let prompt = (!prompt_parts.is_empty()).then(|| prompt_parts.join(" "));
    Args::Run { mode, provider, model, prompt }
}

async fn run(args: &[String]) -> Result<(), String> {
    match parse_args(args) {
        Args::MockExtensionServer => rpi_cli::mcp_mock::run_mock_server().await,
        Args::Invalid(message) => {
            eprintln!("{message}");
            print_help();
            std::process::exit(2);
        }
        Args::Run { mode, provider, model, prompt } => {
            let (provider, model) = resolve_provider_and_model(provider, model)?;
            let extension_specs = load_mcp_server_specs();
            match mode {
                Mode::Print => {
                    let prompt = require_prompt(prompt).await?;
                    let stop =
                        modes::print_mode::run_print_mode(provider, model, prompt, extension_specs)
                            .await?;
                    println!("== 完成(stop: {stop:?})==");
                    Ok(())
                }
                Mode::Json => {
                    let prompt = require_prompt(prompt).await?;
                    let out: modes::json::SharedWriter = Arc::new(std::sync::Mutex::new(std::io::stdout()));
                    let built = modes::print_mode::build_bare_session(
                        provider,
                        model,
                        Arc::new(modes::json::JsonUi { out: out.clone() }),
                        extension_specs,
                    )
                    .await?;
                    modes::json::run_json_mode(built, prompt, out).await?;
                    Ok(())
                }
                Mode::Rpc => {
                    // writer → RpcUi → 装配(扩展 init 期就能走反向通道)
                    let writer: modes::rpc::SharedRpcWriter =
                        Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
                    let ui = Arc::new(modes::rpc::RpcUi::new(writer.clone()));
                    let built = modes::print_mode::build_bare_session(
                        provider,
                        model,
                        ui,
                        extension_specs,
                    )
                    .await?;
                    modes::rpc::run_rpc_mode(built, tokio::io::stdin(), writer).await
                }
                Mode::Interactive => {
                    // UI 通道在装配期创建(扩展 init 可能就会调 UI)
                    let (ui, ui_rx) = modes::interactive::create_tui_ui();
                    let built = modes::print_mode::build_bare_session(
                        provider,
                        model,
                        Arc::new(ui.clone()),
                        extension_specs,
                    )
                    .await?;
                    modes::interactive::run_interactive_mode(built, ui, ui_rx).await
                }
            }
        }
    }
}

/// prompt 来源:位置参数;缺省时若 stdin 非 TTY,读入整段管道输入(pi 的
/// print 模式语义:cat prompt.txt | rpi)。
async fn require_prompt(prompt: Option<String>) -> Result<String, String> {
    if let Some(prompt) = prompt {
        return Ok(prompt);
    }
    if !std::io::stdin().is_terminal() {
        let mut buffer = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut buffer)
            .await
            .map_err(|e| format!("读取 stdin 失败: {e}"))?;
        let prompt = buffer.trim().to_string();
        if !prompt.is_empty() {
            return Ok(prompt);
        }
    }
    Err("该模式需要 prompt(位置参数,或经管道从 stdin 读入)".into())
}

/// provider/model 解析:mock 或真实 provider(env key 校验 + ModelResolver)。
fn resolve_provider_and_model(
    provider: Option<String>,
    model: Option<String>,
) -> Result<(Arc<dyn rpi_ai::Provider>, rpi_ai::Model), String> {
    match provider.as_deref() {
        Some("mock") => {
            let model = rpi_ai::Model::minimal("mock-1", "mock", "mock");
            Ok((rpi_ai::create_mock_provider("你好!来自 rpi 的 MockProvider。"), model))
        }
        Some(provider_id) => {
            if rpi_ai::env_keys::get_env_api_key(provider_id).is_none() {
                return Err(format!(
                    "未配置 API key:请设置 {provider_id} 对应的环境变量(如 ANTHROPIC_API_KEY)"
                ));
            }
            let spec = match &model {
                Some(model) => format!("{provider_id}/{model}"),
                None => provider_id.to_string(),
            };
            let model = rpi_core::create_model_resolver().resolve(&spec)?;
            let (api, _) = default_provider_endpoint(provider_id)
                .ok_or_else(|| format!("未知 provider: {provider_id}"))?;
            let provider = rpi_ai::create_provider(api).ok_or_else(|| format!("无适配器: {api}"))?;
            Ok((provider, model))
        }
        None => Err("缺少 --provider(或 --mock)".into()),
    }
}

fn print_help() {
    println!(
        "rpi —— pi agent 的 Rust 重写(M6 四种运行模式)\n\
         \n\
         用法:\n\
         \x20 rpi [--mock] \"prompt\"                                  print 模式(非 TTY 自动进入)\n\
         \x20 rpi --mode interactive                                  TUI 聊天(需要终端)\n\
         \x20 rpi --mode json \"prompt\"                               AgentSession 事件 JSONL 上线\n\
         \x20 rpi --mode rpc                                         stdio JSONL RPC(编辑器集成)\n\
         \x20 rpi --provider <id> [--model <id>] \"prompt\"            指定真实 provider\n\
         \x20 rpi --mcp-mock-server                                   MCP 扩展自检服务端\n\
         \n\
         provider 示例:anthropic / openai / deepseek / openrouter / groq …\n\
         API key 从环境变量读取(如 ANTHROPIC_API_KEY / OPENAI_API_KEY)。\n\
         MCP 扩展在 .rpi/settings.json 或 ~/.rpi/settings.json 的 mcpServers 中声明。"
    );
}
