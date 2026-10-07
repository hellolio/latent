//! latent —— pi agent 的 Rust 重写(M6:四种运行模式,08 文档)。
//!
//! 用法:
//! - `latent [--mock] "prompt"`                              print 模式(非 TTY 自动)
//! - `latent --mode interactive`                             TUI 聊天(TTY 必需)
//! - `latent --mode json "prompt"`                           事件 JSONL 上线
//! - `latent --mode rpc`                                     stdio JSONL RPC(编辑器集成)
//! - `latent --provider <id> [--model <id>] "prompt"`        指定真实 provider
//! - `latent -r` / `latent -l`                                  续聊最近会话 / 列出历史会话
//! - `latent --mcp-mock-server`                              MCP 扩展自检服务端

use std::io::IsTerminal;
use std::sync::Arc;

use latent::assembly::load_mcp_server_specs;
use latent::modes;
use latent_runtime::bootstrap::{
    notify_legacy_data_dir, print_session_list, resolve_provider_and_model, resolve_session_store,
};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return;
    }
    if let Err(err) = run(&args).await {
        eprintln!("latent: {err}");
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
    /// landlock 沙箱 helper(内部使用,不由用户直接调用;分发已上移
    /// bootstrap::maybe_dispatch_landlock_helper,此变体仅保持枚举完整)
    LandlockHelper(#[allow(dead_code)] Vec<String>),
    Run {
        mode: Mode,
        provider: Option<String>,
        model: Option<String>,
        theme: Option<String>,
        tui_mode: Option<String>,
        cont: bool,
        resume_index: Option<usize>,
        list: bool,
        session_mode: Option<String>,
        sandbox_writes: Vec<String>,
        sandbox_network: bool,
        prompt: Option<String>,
    },
    Invalid(String),
}

fn parse_args(args: &[String]) -> Args {
    if args.iter().any(|a| a == "--mcp-mock-server") {
        return Args::MockExtensionServer;
    }
    // landlock helper:沙箱包装产物以本可执行文件为 helper,先落 Landlock/
    // seccomp 限制再 exec 真命令(参数由 latent-sandbox wrap_command 生成)
    if let Some(pos) = args.iter().position(|a| a == latent_sandbox::landlock::HELPER_FLAG) {
        return Args::LandlockHelper(args[pos + 1..].to_vec());
    }
    let mut mode: Option<Mode> = None;
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut theme: Option<String> = None;
    let mut tui_mode: Option<String> = None;
    let mut mock = false;
    let mut cont = false;
    let mut resume_index: Option<usize> = None;
    let mut list = false;
    let mut session_mode: Option<String> = None;
    let mut sandbox_writes: Vec<String> = Vec::new();
    let mut sandbox_network = false;
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
            "--theme" if i + 1 < args.len() => {
                theme = Some(args[i + 1].clone());
                i += 2;
            }
            "--tui-mode" if i + 1 < args.len() => {
                tui_mode = Some(args[i + 1].clone());
                i += 2;
            }
            "--mock" => {
                mock = true;
                i += 1;
            }
            // -r/--resume 是 -c/--continue 的别名;紧跟的纯数字 = -l 列表序号
            "--continue" | "-c" | "--resume" | "-r" => {
                cont = true;
                let next = args.get(i + 1).map(String::as_str).unwrap_or_default();
                if !next.is_empty() && next.chars().all(|c| c.is_ascii_digit()) {
                    resume_index = next.parse().ok();
                    if resume_index.is_none_or(|n| n == 0) {
                        return Args::Invalid(format!("无效的会话序号: {next}"));
                    }
                    i += 1;
                }
                i += 1;
            }
            "--list" | "-l" => {
                list = true;
                i += 1;
            }
            "--session-mode" if i + 1 < args.len() => {
                session_mode = Some(args[i + 1].clone());
                i += 2;
            }
            "--plan" => {
                session_mode = Some("plan".into());
                i += 1;
            }
            "--yolo" => {
                // 对标 Codex --dangerously-bypass-approvals-and-sandbox:全自动
                session_mode = Some("full-access".into());
                i += 1;
            }
            "--sandbox-write" if i + 1 < args.len() => {
                sandbox_writes.push(args[i + 1].clone());
                i += 2;
            }
            "--sandbox-network" => {
                sandbox_network = true;
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
    // 组合约束(13 文档 §10.1):FullAccess 与沙箱参数互斥
    if session_mode.as_deref() == Some("full-access")
        && (!sandbox_writes.is_empty() || sandbox_network)
    {
        return Args::Invalid(
            "--yolo/--session-mode full-access 与 --sandbox-write/--sandbox-network 互斥".into(),
        );
    }
    Args::Run {
        mode,
        provider,
        model,
        theme,
        tui_mode,
        cont,
        resume_index,
        list,
        session_mode,
        sandbox_writes,
        sandbox_network,
        prompt,
    }
}


async fn run(args: &[String]) -> Result<(), String> {
    if latent_runtime::bootstrap::maybe_dispatch_landlock_helper(args) {
        return Ok(());
    }
    match parse_args(args) {
        Args::MockExtensionServer => latent::mcp_mock::run_mock_server().await,
        Args::LandlockHelper(_) => unreachable!("landlock helper 已在 parse_args 前分发"),
        Args::Invalid(message) => {
            eprintln!("{message}");
            print_help();
            std::process::exit(2);
        }
    Args::Run {
        mode,
        provider,
        model,
        theme,
        tui_mode,
        cont,
        resume_index,
        list,
        session_mode,
        sandbox_writes,
        sandbox_network,
        prompt,
    } => {
        notify_legacy_data_dir();
        if list {
            print_session_list();
            return Ok(());
        }
        let (provider, model) = resolve_provider_and_model(provider, model)?;
        let extension_specs = load_mcp_server_specs();
        let session_store = resolve_session_store(cont, resume_index)?;
            // settings 运行期开关在入口解析一次,装配层不读用户配置文件
            let mut settings = latent::assembly::load_session_settings();
            // CLI flag > settings(13 文档 §12 优先级)
            if let Some(name) = &session_mode {
                settings.session_mode = latent_core::SessionMode::parse(name)
                    .ok_or_else(|| format!("未知会话模式: {name}(plan|confirm|full-access)"))?;
            }
            for dir in sandbox_writes {
                settings.sandbox.writable_roots.push(dir);
            }
            if sandbox_network {
                settings.sandbox.network_access = true;
            }
            // 会话模式 CLI 解析结果作为显式覆盖传入装配(resume 时优先于 entry)
            let cli_session_mode =
                session_mode.as_deref().and_then(latent_core::SessionMode::parse);
            match mode {
                Mode::Print => {
                    let prompt = require_prompt(prompt).await?;
                    let stop = modes::print_mode::run_print_mode(
                        provider,
                        model,
                        prompt,
                        extension_specs,
                        session_store,
                        settings,
                        cli_session_mode,
                    )
                    .await?;
                    println!("== 完成(stop: {stop:?})==");
                    Ok(())
                }
                Mode::Json => {
                    let prompt = require_prompt(prompt).await?;
                    let out: modes::json::SharedWriter =
                        Arc::new(std::sync::Mutex::new(std::io::stdout()));
                    let built = modes::print_mode::build_bare_session(
                        provider,
                        model,
                        Arc::new(modes::json::JsonUi { out: out.clone() }),
                        extension_specs,
                        session_store,
                        settings,
                        None,
                        cli_session_mode,
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
                    // 审批反向通道:与 RpcUi 同一 writer;run_rpc_mode 路由应答
                    let rpc_approval =
                        Arc::new(modes::rpc::RpcApprovalUi::new(writer.clone()));
                    // 审批反向通道同时作为 ApprovalUi 注入装配:修复此前
                    // approval_ui=None 兜底 Deny、RpcApprovalUi 从未参与审批
                    // 决策的缺陷(approval_ui 与路由句柄指向同一实例)
                    let approval_ui: Arc<dyn latent_core::ApprovalUi> = rpc_approval.clone();
                    let built = modes::print_mode::build_bare_session(
                        provider,
                        model,
                        ui,
                        extension_specs,
                        session_store,
                        settings,
                        Some(approval_ui),
                        cli_session_mode,
                    )
                    .await?;
                    modes::rpc::run_rpc_mode(built, rpc_approval, tokio::io::stdin(), writer)
                        .await
                }
                Mode::Interactive => {
                    // UI 通道在装配期创建(扩展 init 可能就会调 UI)
                    let (ui, ui_rx) = modes::interactive::create_tui_ui();
                    let approval_ui: Arc<dyn latent_core::ApprovalUi> =
                        Arc::new(ui.approval_ui());
                    let built = modes::print_mode::build_bare_session(
                        provider,
                        model,
                        Arc::new(ui.clone()),
                        extension_specs,
                        session_store,
                        settings,
                        Some(approval_ui),
                        cli_session_mode,
                    )
                    .await?;
                    modes::interactive::run_interactive_mode(built, ui, ui_rx, theme, tui_mode)
                        .await
                }
            }
        }
    }
}



/// prompt 来源:位置参数;缺省时若 stdin 非 TTY,读入整段管道输入(pi 的
/// print 模式语义:cat prompt.txt | latent)。
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



fn print_help() {
    println!(
        "latent —— pi agent 的 Rust 重写(M6 四种运行模式)\n\
         \n\
         用法:\n\
         \x20 latent [--mock] \"prompt\"                                  print 模式(非 TTY 自动进入)\n\
         \x20 latent --mode interactive                                  TUI 聊天(需要终端)\n\
         \x20 latent --mode json \"prompt\"                               AgentSession 事件 JSONL 上线\n\
         \x20 latent --mode rpc                                         stdio JSONL RPC(编辑器集成)\n\
         \x20 latent --provider <id> [--model <id>] \"prompt\"            指定真实 provider\n\
         \x20 latent --model <provider>/<id> \"prompt\"                    provider/model 形式\n\
         \x20 latent --tui-mode fullscreen|regular                       全屏(默认)/ scrollback 渲染模式\n\
         \x20 latent --continue [\"prompt\"]                               续聊当前项目最近的会话\n\
         \x20 latent -r [序号]                                            同 --continue(序号 = -l 列表序号)\n\
         \x20 latent -l                                                   列出当前项目的历史会话\n\
         \x20 latent --mcp-mock-server                                   MCP 扩展自检服务端\n\
         \n\
         provider 示例:anthropic / openai / deepseek / openrouter / groq …\n\
         API key 从环境变量读取(如 ANTHROPIC_API_KEY / OPENAI_API_KEY);\n\
         自定义 provider/model/URL 在 .latent/models.json 或用户数据目录 models.json 声明,\n\
         默认 provider/model 在 .latent/settings.json 的 defaultProvider/defaultModel 配置。\n\
         MCP 扩展在 .latent/settings.json 或数据目录 settings.json 的 mcpServers 中声明。\n\
         用户数据目录(LATENT_HOME 可覆盖;默认 ~/.config/latent,旧版 ~/.latent 自动沿用)\n\
         存放 settings/models/web-search/skills/agents/sessions 等全局数据。"
    );
}
