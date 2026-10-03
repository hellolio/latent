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

use rpi_cli::assembly::load_mcp_server_specs;
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
    /// landlock 沙箱 helper(内部使用,不由用户直接调用;13 文档 §7.4)
    LandlockHelper(Vec<String>),
    Run {
        mode: Mode,
        provider: Option<String>,
        model: Option<String>,
        theme: Option<String>,
        cont: bool,
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
    // seccomp 限制再 exec 真命令(参数由 rpi-sandbox wrap_command 生成)
    if let Some(pos) = args.iter().position(|a| a == rpi_sandbox::landlock::HELPER_FLAG) {
        return Args::LandlockHelper(args[pos + 1..].to_vec());
    }
    let mut mode: Option<Mode> = None;
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut theme: Option<String> = None;
    let mut mock = false;
    let mut cont = false;
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
            "--mock" => {
                mock = true;
                i += 1;
            }
            "--continue" | "-c" => {
                cont = true;
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
        cont,
        session_mode,
        sandbox_writes,
        sandbox_network,
        prompt,
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    match parse_args(args) {
        Args::MockExtensionServer => rpi_cli::mcp_mock::run_mock_server().await,
        Args::LandlockHelper(args) => rpi_sandbox::landlock::run_helper(&args)
            .map_err(|error| format!("landlock helper: {error}")),
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
            cont,
            session_mode,
            sandbox_writes,
            sandbox_network,
            prompt,
        } => {
            let (provider, model) = resolve_provider_and_model(provider, model)?;
            let extension_specs = load_mcp_server_specs();
            let session_store = resolve_session_store(cont)?;
            // settings 运行期开关在入口解析一次,装配层不读用户配置文件
            let mut settings = rpi_cli::assembly::load_session_settings();
            // CLI flag > settings(13 文档 §12 优先级)
            if let Some(name) = &session_mode {
                settings.session_mode = rpi_core::SessionMode::parse(name)
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
                session_mode.as_deref().and_then(rpi_core::SessionMode::parse);
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
                    let built = modes::print_mode::build_bare_session(
                        provider,
                        model,
                        ui,
                        extension_specs,
                        session_store,
                        settings,
                        None,
                        Some(rpc_approval),
                        cli_session_mode,
                    )
                    .await?;
                    modes::rpc::run_rpc_mode(built, tokio::io::stdin(), writer).await
                }
                Mode::Interactive => {
                    // UI 通道在装配期创建(扩展 init 可能就会调 UI)
                    let (ui, ui_rx) = modes::interactive::create_tui_ui();
                    let approval_ui: Arc<dyn rpi_core::ApprovalUi> =
                        Arc::new(ui.approval_ui());
                    let built = modes::print_mode::build_bare_session(
                        provider,
                        model,
                        Arc::new(ui.clone()),
                        extension_specs,
                        session_store,
                        settings,
                        Some(approval_ui),
                        None,
                        cli_session_mode,
                    )
                    .await?;
                    modes::interactive::run_interactive_mode(built, ui, ui_rx, theme).await
                }
            }
        }
    }
}

/// 会话存储:默认在 `~/.rpi/sessions/<项目前缀>/` 新建 `<时间>__<session-id>.jsonl`;
/// `--continue` 续聊当前项目最近的会话文件。HOME 缺失时降级为内存会话。
fn resolve_session_store(cont: bool) -> Result<rpi_cli::assembly::SessionStore, String> {
    use rpi_cli::assembly::SessionStore;
    let Some(home) = dirs_home() else {
        if cont {
            return Err("--continue 需要 HOME 目录".into());
        }
        eprintln!("[rpi] 无法定位 HOME,本次会话仅保存在内存");
        return Ok(SessionStore::Memory);
    };
    let sessions_dir = home.join(".rpi/sessions");
    if cont {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let file = rpi_session::find_latest_session_file(
            &sessions_dir,
            Some(cwd.to_string_lossy().as_ref()),
        )
        .ok_or_else(|| {
            format!(
                "没有可续聊的会话({} 下没有当前项目的会话文件)",
                sessions_dir.display()
            )
        })?;
        println!("续聊会话:{}", file.display());
        Ok(SessionStore::Resume { file })
    } else {
        Ok(SessionStore::New { dir: sessions_dir })
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

/// provider/model 解析:mock 或真实 provider(models.json 配置体系 +
/// ModelResolver;凭据由 Model/适配器按配置与 env 解析,不再前置拦截)。
fn resolve_provider_and_model(
    provider: Option<String>,
    model: Option<String>,
) -> Result<(Arc<dyn rpi_ai::Provider>, rpi_ai::Model), String> {
    if provider.as_deref() == Some("mock") {
        let model = rpi_ai::Model::minimal("mock-1", "mock", "mock");
        return Ok((
            rpi_ai::create_mock_provider("你好!来自 rpi 的 MockProvider。"),
            model,
        ));
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let home = dirs_home();
    let resolver = rpi_core::create_model_resolver_from_config(Some(&cwd), home.as_deref());
    // 未指定 provider/model 时回退 settings.json 的 defaultProvider/defaultModel
    let (provider, model) = match (&provider, &model) {
        (Some(_), _) | (_, Some(_)) => (provider, model),
        (None, None) => {
            let (default_provider, default_model) =
                rpi_core::load_default_model_selection(Some(&cwd), home.as_deref());
            if default_provider.is_none() && default_model.is_none() {
                return Err(
                    "缺少 --provider(或 --mock);也可在 .rpi/settings.json 配置 defaultProvider/defaultModel"
                        .into(),
                );
            }
            (default_provider, default_model)
        }
    };
    // spec 组装:--provider p --model m → p/m;--model provider/model → 原样
    let spec = match (&provider, &model) {
        (Some(p), Some(m)) => {
            if m.contains('/') {
                return Err(format!(
                    "--model `{m}` 已含 provider 前缀,不要再传 --provider"
                ));
            }
            format!("{p}/{m}")
        }
        (Some(p), None) => p.clone(),
        (None, Some(m)) => m.clone(),
        (None, None) => unreachable!("上方已保证 provider/model 至少一个存在"),
    };
    let model = resolver.resolve(&spec)?;
    let adapter =
        rpi_ai::create_provider(&model.api).ok_or_else(|| format!("无适配器: {}", model.api))?;
    Ok((adapter, model))
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
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
         \x20 rpi --model <provider>/<id> \"prompt\"                    provider/model 形式\n\
         \x20 rpi --continue [\"prompt\"]                               续聊当前项目最近的会话\n\
         \x20 rpi --mcp-mock-server                                   MCP 扩展自检服务端\n\
         \n\
         provider 示例:anthropic / openai / deepseek / openrouter / groq …\n\
         API key 从环境变量读取(如 ANTHROPIC_API_KEY / OPENAI_API_KEY);\n\
         自定义 provider/model/URL 在 .rpi/models.json 或 ~/.rpi/models.json 声明,\n\
         默认 provider/model 在 .rpi/settings.json 的 defaultProvider/defaultModel 配置。\n\
         MCP 扩展在 .rpi/settings.json 或 ~/.rpi/settings.json 的 mcpServers 中声明。"
    );
}
