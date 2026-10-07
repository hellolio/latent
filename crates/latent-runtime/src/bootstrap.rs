//! 入口层共享助手(原 latent-cli `main.rs` 的公共面):数据目录提示、
//! provider/model 解析、会话存储解析、会话列表打印、landlock helper 分发。
//! latent-cli 与 latent-gateway 两个 bin 的 main 都从这里取用,行为一致。

use std::sync::Arc;

/// HOME 环境变量快照(数据目录解析链的入口;纯读 env,装配层约定)。
pub fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// 数据目录落在旧版 `~/.latent` 时提示一次:新版默认 `~/.config/latent`,
/// 可设 `LATENT_HOME` 显式指定,或把目录移动到新位置。只走 stderr,不污染
/// print/json/rpc 模式的 stdout 协议。
pub fn notify_legacy_data_dir() {
    let home = dirs_home();
    let env = std::env::var_os("LATENT_HOME");
    let (dir, source) = latent_core::resolve_latent_dir(home.as_deref(), env.as_deref());
    if source == latent_core::LatentDirSource::LegacyDotLatent {
        if let Some(dir) = dir {
            eprintln!(
                "[latent] 数据目录沿用旧版 {}(新版默认 ~/.config/latent;可设 LATENT_HOME 指定,或将目录移动到新位置)",
                dir.display()
            );
        }
    }
}

/// 会话存储:默认在数据目录 `sessions/<项目前缀>/` 新建
/// `<时间>__<session-id>.jsonl`(数据目录 = LATENT_HOME,默认
/// `~/.config/latent`,旧版 `~/.latent` 沿用);`--continue`/`-r` 续聊当前
/// 项目最近的会话文件(resume_index 指定 `-l` 列表中的序号,1 起)。
/// HOME/LATENT_HOME 都缺失时降级为内存会话。
pub fn resolve_session_store(
    cont: bool,
    resume_index: Option<usize>,
) -> Result<crate::assembly::SessionStore, String> {
    use crate::assembly::SessionStore;
    let Some(dir) = latent_core::latent_dir(dirs_home().as_deref()) else {
        if cont {
            return Err("--continue 需要 HOME 或 LATENT_HOME 环境变量".into());
        }
        eprintln!("[latent] 无法定位数据目录(HOME/LATENT_HOME),本次会话仅保存在内存");
        return Ok(SessionStore::Memory);
    };
    let sessions_dir = dir.join("sessions");
    if cont {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let sessions = latent_session::list_session_files(
            &sessions_dir,
            Some(cwd.to_string_lossy().as_ref()),
        );
        if sessions.is_empty() {
            return Err(format!(
                "没有可续聊的会话({} 下没有当前项目的会话文件)",
                sessions_dir.display()
            ));
        }
        let index = resume_index.unwrap_or(1);
        let summary = sessions.get(index - 1).ok_or_else(|| {
            format!(
                "会话序号 {index} 超出范围(共 {} 个,先用 latent -l 查看列表)",
                sessions.len()
            )
        })?;
        println!("续聊会话:{}", summary.path.display());
        Ok(SessionStore::Resume {
            file: summary.path.clone(),
        })
    } else {
        Ok(SessionStore::New { dir: sessions_dir })
    }
}

/// `-l`/`--list`:打印当前项目的历史会话列表(最新在前,序号供 `latent -r <n>`)。
pub fn print_session_list() {
    let Some(dir) = latent_core::latent_dir(dirs_home().as_deref()) else {
        eprintln!("latent: --list 需要 HOME 或 LATENT_HOME 环境变量");
        std::process::exit(1);
    };
    let sessions_dir = dir.join("sessions");
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let sessions =
        latent_session::list_session_files(&sessions_dir, Some(cwd.to_string_lossy().as_ref()));
    if sessions.is_empty() {
        println!(
            "没有历史会话({} 下没有当前项目的会话文件)",
            sessions_dir.display()
        );
        return;
    }
    println!("当前项目的历史会话(最新在前;`latent -r <序号>` 续聊对应会话):");
    for (index, summary) in sessions.iter().enumerate() {
        println!(
            "  {:>3}. {}  {}  {}",
            index + 1,
            summary.local_time(),
            &summary.session_id[..summary.session_id.len().min(8)],
            summary.preview
        );
        println!("        {}", summary.path.display());
    }
}

/// provider/model 解析:mock 或真实 provider(models.json 配置体系 +
/// ModelResolver;凭据由 Model/适配器按配置与 env 解析,不再前置拦截)。
pub fn resolve_provider_and_model(
    provider: Option<String>,
    model: Option<String>,
) -> Result<(Arc<dyn latent_ai::Provider>, latent_ai::Model), String> {
    if provider.as_deref() == Some("mock") {
        let model = latent_ai::Model::minimal("mock-1", "mock", "mock");
        return Ok((
            latent_ai::create_mock_provider("你好!来自 latent 的 MockProvider。"),
            model,
        ));
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let dir = latent_core::latent_dir(dirs_home().as_deref());
    let resolver = latent_core::create_model_resolver_from_config(Some(&cwd), dir.as_deref());
    // 未指定 provider/model 时回退 settings.json 的 defaultProvider/defaultModel
    let (provider, model) = match (&provider, &model) {
        (Some(_), _) | (_, Some(_)) => (provider, model),
        (None, None) => {
            let (default_provider, default_model) =
                latent_core::load_default_model_selection(Some(&cwd), dir.as_deref());
            if default_provider.is_none() && default_model.is_none() {
                return Err(
                    "缺少 --provider(或 --mock);也可在 .latent/settings.json 配置 defaultProvider/defaultModel"
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
        latent_ai::create_provider(&model.api).ok_or_else(|| format!("无适配器: {}", model.api))?;
    Ok((adapter, model))
}

/// landlock helper 分发:沙箱包装产物以本可执行文件为 helper(参数由
/// latent-sandbox wrap_command 生成)。命中 helper 标志时先落 Landlock/
/// seccomp 限制再 exec 真命令,然后返回 true(调用方直接结束本进程);
/// 未命中返回 false 走正常参数解析。**两个 bin(latent / latent-gateway)的
/// main 开头都必须调用**,否则 Linux 沙箱静默失效。
pub fn maybe_dispatch_landlock_helper(args: &[String]) -> bool {
    let Some(pos) = args.iter().position(|a| a == latent_sandbox::landlock::HELPER_FLAG)
    else {
        return false;
    };
    match latent_sandbox::landlock::run_helper(&args[pos + 1..]) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("landlock helper: {error}");
            std::process::exit(1);
        }
    }
    true
}
