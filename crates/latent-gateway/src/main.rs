//! latent-gateway —— 聊天网关 daemon(bin: latent-gateway)。
//!
//! 用法:
//! - `latent-gateway`                          前台跑 daemon(默认)
//! - `latent-gateway pairing list`             列出未决配对(控制面薄客户端)
//! - `latent-gateway pairing approve <channel> <code>`  批准配对
//! - `latent-gateway channels status [--probe]` 渠道连接状态
//! - `latent-gateway status`                    运行态
//!
//! 配置:`.latent/gateway.json`(项目,逐字段覆盖)/ `<数据目录>/gateway.json`
//! (全局)。非法配置/缺 token → 退出码 78。

use std::sync::Arc;

use latent_runtime::bootstrap::{
    dirs_home, maybe_dispatch_landlock_helper, notify_legacy_data_dir, resolve_provider_and_model,
};

use latent_gateway::agents::SessionFactory;
use latent_gateway::approval::ChatApprovalUi;
use latent_gateway::auto_reply::Gateway;
use latent_gateway::channels::{ChannelManager, TaggedChannelEvent};
use latent_gateway::config::{
    load_config, load_config_value, resolve_credential_scoped, EXIT_INVALID_CONFIG,
};
use latent_gateway::control::client::{resolve_client_token, ControlClient, DEFAULT_ENDPOINT};
use latent_gateway::control::events::GatewayEvent;
use latent_gateway::pairing::PairingStore;
use latent_gateway::state::StateStore;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return;
    }
    // landlock 沙箱 helper:沙箱包装产物以本可执行文件为 helper(Linux 必需)
    if maybe_dispatch_landlock_helper(&args) {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(error) = runtime.block_on(run(&args)) {
        eprintln!("latent-gateway: {error}");
        std::process::exit(1);
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None | Some("daemon") => run_daemon().await,
        Some("pairing") => run_pairing_subcommand(&args[1..]).await,
        Some("channels") => run_channels_status(&args[1..]).await,
        Some("status") => run_status().await,
        Some(other) => Err(format!("未知子命令: {other}(用 --help 查看用法)")),
    }
}

// ---------------------------------------------------------------------------
// 子命令(控制面薄客户端;daemon 单一写者,禁止 CLI 直写 state.json)
// ---------------------------------------------------------------------------

async fn connect_control_plane() -> Result<ControlClient, String> {
    let token = resolve_client_token(None)?;
    ControlClient::connect(DEFAULT_ENDPOINT, &token).await
}

async fn run_pairing_subcommand(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let mut client = connect_control_plane().await?;
            let payload = client
                .request("pairing.list", serde_json::json!({}))
                .await?;
            println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
            Ok(())
        }
        Some("approve") => {
            let (channel, code) = match (args.get(1), args.get(2)) {
                (Some(channel), Some(code)) => (channel.clone(), code.clone()),
                _ => return Err("用法: latent-gateway pairing approve <channel> <code>".into()),
            };
            let mut client = connect_control_plane().await?;
            let payload = client
                .request(
                    "pairing.approve",
                    serde_json::json!({ "channel": channel, "code": code }),
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
            Ok(())
        }
        _ => Err("用法: latent-gateway pairing <list|approve <channel> <code>>".into()),
    }
}

async fn run_channels_status(args: &[String]) -> Result<(), String> {
    let probe = args.iter().any(|arg| arg == "--probe");
    let mut client = connect_control_plane().await?;
    let payload = client
        .request("channels.status", serde_json::json!({ "probe": probe }))
        .await?;
    println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
    Ok(())
}

async fn run_status() -> Result<(), String> {
    let mut client = connect_control_plane().await?;
    let payload = client.request("status", serde_json::json!({})).await?;
    println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
    Ok(())
}

// ---------------------------------------------------------------------------
// daemon
// ---------------------------------------------------------------------------

async fn run_daemon() -> Result<(), String> {
    notify_legacy_data_dir();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let data_dir = latent_runtime::facade::latent_dir(dirs_home().as_deref());

    // 配置:项目逐字段覆盖全局;非法 → 退出码 78(项目级 !shell 在 load_config 强制拒绝)
    let config = match load_config(Some(&cwd), data_dir.as_deref()) {
        Ok(config) => config,
        Err(error) => exit_invalid_config(&error.to_string()),
    };
    if config.gateway.auth.mode != "token" {
        exit_invalid_config("gateway.auth.mode 只支持 token");
    }
    // auth.token 必填(§5.4):未配置 → 拒绝启动,错误信息指明如何配置
    if config.gateway.auth.token.trim().is_empty() {
        eprintln!(
            "latent-gateway: gateway.auth.token 未配置(退出码 {EXIT_INVALID_CONFIG})。\n\
             在 .latent/gateway.json 或 <数据目录>/gateway.json 配置:\n\
             \x20 \"gateway\": {{ \"auth\": {{ \"mode\": \"token\", \"token\": \"$LATENT_GATEWAY_TOKEN\" }} }}"
        );
        std::process::exit(EXIT_INVALID_CONFIG);
    }
    let auth_token = match resolve_credential_scoped(
        "gateway.auth.token",
        &config.gateway.auth.token,
        /* allow_shell */ true,
        Some("LATENT_GATEWAY_TOKEN"),
    )
    .await
    {
        Ok(token) => token,
        Err(error) => exit_invalid_config(&error.to_string()),
    };

    // workspace:非空 = gateway 会话的工作区(进程级切换;gateway 是专属 daemon)
    if !config.agents.defaults.workspace.trim().is_empty() {
        let workspace = std::path::PathBuf::from(config.agents.defaults.workspace.trim());
        if !workspace.is_dir() {
            return Err(format!(
                "agents.defaults.workspace 目录不存在: {}",
                workspace.display()
            ));
        }
        std::env::set_current_dir(&workspace).map_err(|error| error.to_string())?;
        eprintln!("[latent-gateway] 工作区: {}", workspace.display());
    }

    // provider/model:gateway.json agents.defaults.model > settings defaultProvider/defaultModel
    let spec = config.agents.defaults.model.trim().to_string();
    let (provider_arg, model_arg) = match spec.as_str() {
        "" => (None, None),
        s if s.contains('/') => (None, Some(s.to_string())),
        s => (Some(s.to_string()), None),
    };
    let (provider, model) = resolve_provider_and_model(provider_arg, model_arg)?;

    // 会话运行期开关(settings.json 体系与 CLI 共用)+ MCP specs
    let settings = latent_runtime::assembly::load_session_settings();
    let extension_specs = latent_runtime::assembly::load_mcp_server_specs();

    // state.json(唯一写者是本 daemon;CLI 子命令经控制面请求变更)
    let state_dir = data_dir.clone().unwrap_or_else(std::env::temp_dir);
    let sessions_dir = state_dir.join("sessions");
    let state = Arc::new(StateStore::load(state_dir.join("gateway/state.json")));

    // 审批全局单例(所有会话 clone 注入同一 pending 表;§4.9)
    let approval = ChatApprovalUi::new();

    // DM 配对策略注册
    let mut pairing = PairingStore::new(state.clone());
    for (channel, policy, allow_from) in ChannelManager::dm_policies(&config) {
        pairing.register_policy(channel, policy, allow_from);
    }

    // 控制面事件广播(先建,工厂/引擎共享)
    let (broadcast_events, _) = tokio::sync::broadcast::channel(256);

    // 渠道凭据预解析(失败 = 该渠道拒绝启动,不阻断 daemon)

    // 会话工厂
    let thinking_level = config
        .agents
        .defaults
        .thinking_level
        .as_deref()
        .and_then(latent_runtime::assembly::parse_thinking_level);
    // messages.queue → factory(新会话队列设置)
    let queue = &config.messages.queue;
    let queue_settings = latent_gateway::auto_reply::queue::QueueSettings {
        mode: latent_gateway::config::QueueMode::parse(&queue.mode).unwrap_or_default(),
        debounce_ms: queue.debounce_ms,
        cap: queue.cap,
        drop_policy: match queue.drop.as_str() {
            "old" => latent_gateway::config::DropPolicy::Old,
            "new" => latent_gateway::config::DropPolicy::New,
            _ => latent_gateway::config::DropPolicy::Summarize,
        },
    };
    let factory = Arc::new(SessionFactory {
        provider: provider.clone(),
        model: model.clone(),
        settings,
        sessions_dir,
        extension_specs,
        approval_ui: approval.clone(),
        state: state.clone(),
        // P2-14:与 Gateway 共用同一份 typing 配置(单一事实来源)
        typing_config: latent_gateway::auto_reply::build_typing_config(&config),
        events: Some(latent_gateway::auto_reply::reply_dispatcher::EventSink {
            events: broadcast_events.clone(),
            session_key: String::new(),
        }),
        default_thinking_level: thinking_level,
        queue_settings,
    });

    // 渠道启动(raw 凭据先物化:插件最终以 raw overlay 为准)
    let mut raw_config = load_config_value(Some(&cwd), data_dir.as_deref()).0;
    let channels = Arc::new(ChannelManager::new());
    let (events_tx, events_rx) =
        tokio::sync::mpsc::channel::<TaggedChannelEvent>(256);
    // 防御:即使全部渠道启动失败(没有任何转发任务持有发送端),也保留一个
    // 发送端,避免主循环 recv() 返回 None 而静默退出(回归:曾致 daemon 秒退)
    let _events_keepalive = events_tx.clone();
    let config = latent_gateway::config::materialize_channel_credentials(config, &mut raw_config)
        .await
        .map_err(|error| format!("渠道凭据物化失败: {error}"))?;
    let failures = channels
        .start_enabled(
            &config,
            raw_config.get("channels").unwrap_or(&serde_json::Value::Null),
            events_tx,
        )
        .await;
    for (id, error) in &failures {
        eprintln!("[latent-gateway] 渠道 {id} 启动失败: {error}");
    }
    let enabled_total = [
        config.channels.qq.as_ref().map(|c| c.enabled),
        config.channels.wecom.as_ref().map(|c| c.enabled),
        config.channels.telegram.as_ref().map(|c| c.enabled),
        config.channels.mock.as_ref().map(|c| c.enabled),
    ]
    .iter()
    .filter(|enabled| **enabled == Some(true))
    .count();
    let started = enabled_total.saturating_sub(failures.len());
    if enabled_total == 0 {
        eprintln!(
            "[latent-gateway][warn] 未启用任何渠道(gateway.json channels 全空):daemon 仅控制面可用"
        );
    } else if started == 0 {
        eprintln!(
            "[latent-gateway][warn] 所有渠道启动失败(原因见上):daemon 保持运行,仅控制面可用;修复配置/凭据后重启生效"
        );
    }

    // gateway 引擎
    let gateway = Gateway::new(
        config.clone(),
        channels.clone(),
        factory,
        approval.clone(),
        pairing,
        state.clone(),
        broadcast_events,
    );

    // 审批通道:owner DM(第一个已连接的 owner 渠道)
    latent_gateway::daemon::refresh_approval_transport(&gateway).await;

    // 防抖冲刷循环
    tokio::spawn(gateway.clone().run_debounce_loop());

    // 控制面(axum;退出即 daemon 退出)
    let control_gateway = gateway.clone();
    let (bind, port) = (config.gateway.bind.clone(), config.gateway.port);
    let control_token = auth_token;
    tokio::spawn(async move {
        if let Err(error) =
            latent_gateway::control::server::serve(control_gateway, &bind, port, control_token)
                .await
        {
            eprintln!("[latent-gateway] 控制面退出: {error}");
            std::process::exit(1);
        }
    });

    eprintln!(
        "[latent-gateway] daemon 已就绪(渠道 {} 个,控制面 ws://{}:{})",
        gateway.channels.status_snapshot().await.len(),
        config.gateway.bind,
        config.gateway.port
    );

    // 主事件循环:渠道入站 → dispatch;状态变化 → 记录/审批通道/广播
    // (防御逻辑见 daemon::run_event_loop;Ctrl-C 返回后走下方优雅停机)
    latent_gateway::daemon::run_event_loop(gateway.clone(), events_rx).await;

    // 优雅停机:后台 subagent 全部中止 + 渠道断开 + state 落盘(§4.4)
    for key in gateway.registry.keys().await {
        if let Some(session) = gateway.registry.get(&key).await {
            if let Some(registry) = &session.built.subagent_registry {
                registry.abort_all();
            }
            session.built.session.abort();
        }
    }
    gateway.approval.close_all().await;
    gateway.channels.shutdown_all().await;
    if let Err(error) = gateway.state.save() {
        eprintln!("[latent-gateway] state.json 落盘失败: {error}");
    }
    let _ = gateway.events.send(GatewayEvent::Shutdown);
    eprintln!("[latent-gateway] 已退出");
    Ok(())
}

fn exit_invalid_config(reason: &str) -> ! {
    eprintln!("latent-gateway: 配置非法(退出码 {EXIT_INVALID_CONFIG}): {reason}");
    std::process::exit(EXIT_INVALID_CONFIG);
}

fn print_help() {
    println!(
        "latent-gateway —— 聊天网关 daemon\n\
         \n\
         用法:\n\
         \x20 latent-gateway                                前台跑 daemon(默认)\n\
         \x20 latent-gateway pairing list                   列出未决配对请求\n\
         \x20 latent-gateway pairing approve <channel> <code>  批准配对(daemon 单一写者)\n\
         \x20 latent-gateway channels status [--probe]      渠道连接状态\n\
         \x20 latent-gateway status                         运行态\n\
         \n\
         配置:.latent/gateway.json(项目,逐字段覆盖)/ <数据目录>/gateway.json(全局)。\n\
         gateway.auth.token 必填($ENV / !shell 来源;未配置 → 退出码 78)。\n\
         控制面:ws://127.0.0.1:18789/ws(默认;CLI 子命令经控制面,token 读 $LATENT_GATEWAY_TOKEN)。"
    );
}
