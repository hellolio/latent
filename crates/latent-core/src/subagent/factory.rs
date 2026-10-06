//! `/subagent` 平行会话工厂(14 文档扩展):按 agent 定义现场创建与主会话
//! **平权**的独立 `AgentSession` —— 不是嵌套:独立 hooks 洋葱(含各自的
//! mode cell 与 PermissionEngine)、系统提示词 = 定义 md 正文、工具 = 白名单、
//! 转录纯内存。上下文与主会话及彼此完全隔离。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use latent_agent::{LoopHooks, PassthroughHooks, Tool};
use latent_ai::Model;

use crate::permission::{
    ApprovalHooks, ApprovalRules, ApprovalUi, HeadlessApproval, HeadlessApprovalUi,
    PermissionEngine, SandboxConfig,
};
use crate::session::{create_agent_session, AgentSession, SessionSharedSubscriber};

use super::defs::{discover_agent_defs, AgentDef};

/// 工具池工厂签名:入参 = 本会话的权限引擎(工具内的沙箱包装钩子按它取模式)。
pub type ToolPoolFactory =
    Arc<dyn Fn(Arc<PermissionEngine>) -> Vec<Arc<dyn Tool>> + Send + Sync>;

/// 平行会话工厂:字段全部装配期注入(与 task 工具共用同一套依赖)。
pub struct SubagentSessionFactory {
    pub provider: Arc<dyn latent_ai::Provider>,
    pub approval_ui: Arc<dyn ApprovalUi>,
    /// 初始会话模式与沙箱/审批规则的来源(每个会话独立引擎,模式互不影响)
    pub engine: Arc<PermissionEngine>,
    pub sandbox: SandboxConfig,
    pub rules: ApprovalRules,
    pub cwd: PathBuf,
    pub sandbox_available: bool,
    /// 白名单缺省集(装配层注入的只读集;不含 shell,可跨会话共享)
    pub default_tools: Vec<Arc<dyn Tool>>,
    /// 会话工具池工厂:每个平行会话现场构建工具实例,shell 沙箱包装钩子绑定
    /// **本会话**引擎 —— 模式切档(含退出 Plan)对沙箱即时生效。不可共享主
    /// 会话工具实例:那会把子会话 bash 钉死在主会话模式上(主 Plan + 子
    /// FullAccess → 命令仍被主引擎的 ReadOnly 沙箱拒绝)。
    pub tool_pool_factory: ToolPoolFactory,
    pub resolve_model: super::tool::ModelResolveFn,
    /// 子会话落盘工厂(可选;tag = agent 名;None = 纯内存)
    pub child_store_factory: Option<super::store::ChildStoreFactory>,
}

impl SubagentSessionFactory {
    /// 发现可用 agent 定义(项目优先;每次调用重新扫描,数据化)。
    /// 本方法是进程入口面,允许在此读 HOME/LATENT_HOME 解析数据目录。
    pub fn discover(&self) -> Vec<AgentDef> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        discover_agent_defs(&self.cwd, crate::paths::latent_dir(home.as_deref()).as_deref()).0
    }

    /// 创建一个平行会话。
    ///
    /// - `fallback_model`:定义未指定 `model` 时使用(通常传主会话当前模型);
    /// - `subscribers`:调用方提供的事件列表(TUI 渲染/审批弹窗经此到达);
    /// - 创建后系统提示词整体替换为定义正文。
    pub async fn create(
        &self,
        def: &AgentDef,
        fallback_model: Model,
        subscribers: Arc<Mutex<Vec<SessionSharedSubscriber>>>,
    ) -> Result<Arc<AgentSession>, String> {
        let model = match &def.model {
            Some(spec) => (self.resolve_model)(spec)?,
            None => fallback_model,
        };

        // 独立引擎:模式/审批缓存随本会话走,与主会话及其他平行会话互不影响
        // (平权);模式节由 create_agent_session 的 apply_mode append 进转录
        let engine = Arc::new(PermissionEngine::new(
            self.engine.mode(),
            self.sandbox.clone(),
            self.rules.clone(),
            self.cwd.clone(),
            self.sandbox_available,
        ));
        // 工具池按本会话现建:shell 沙箱钩子绑定本会话引擎,模式切档即时生效
        let tool_pool = (self.tool_pool_factory)(engine.clone());
        let tools = match &def.tools {
            Some(names) => names
                .iter()
                // 防嵌套:平行会话不装 subagent 工具本身
                .filter(|name| name.as_str() != super::tool::TOOL_NAME)
                .filter_map(|name| {
                    tool_pool
                        .iter()
                        .find(|tool| tool.name() == name)
                        .cloned()
                })
                .collect::<Vec<_>>(),
            None => self.default_tools.clone(),
        };
        let hooks: Arc<dyn LoopHooks> = Arc::new(ApprovalHooks::new(
            Arc::new(PassthroughHooks),
            engine.clone(),
            self.approval_ui.clone(),
            subscribers.clone(),
        ));

        // 会话落盘:与主会话同一套 JSONL 机制,文件名 tag = agent 名
        let store_was_persisted = self.child_store_factory.is_some();
        let store = match &self.child_store_factory {
            Some(factory) => Some(factory(&def.name)?),
            None => None,
        };
        let session = Arc::new(
            create_agent_session(crate::session::AgentSessionConfig {
                provider: self.provider.clone(),
                model,
                hooks,
                ui: Arc::new(crate::extensions::NoopUi),
                extensions: crate::extensions::ExtensionRegistry::default(),
                tools,
                active_tool_names: None,
                system_prompt: crate::system_prompt::SystemPromptOptions::default(),
                limits: latent_agent::TurnLimits::default(),
                stream_options: store
                    .as_ref()
                    .map(|store| store.stream_options.clone())
                    .unwrap_or_default(),
                session_sink: store.map(|store| store.sink),
                seed_messages: vec![],
                compactor: None,
                subscribers: Some(subscribers),
                permission: Some(engine),
            })
            .await
            .map_err(|e| e.to_string())?,
        );
        // 系统提示词 = 定义 md 正文(整体替换)
        session
            .agent()
            .set_system_prompt(Some(def.system_prompt.clone()));
        // 初始模式与主会话装配行为一致:应用当前模式(Plan 等模式节 append
        // 进转录,位于用户输入之前);落盘会话写 ModeChange entry,纯内存会话不写
        if store_was_persisted {
            session
                .set_mode(self.engine.mode())
                .await
                .map_err(|e| e.to_string())?;
        } else {
            session
                .apply_mode_without_persist(self.engine.mode())
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(session)
    }
}

/// 无 UI 通道时的兜底策略(与 print 模式同语义)。
pub fn default_async_approval_ui(policy: HeadlessApproval) -> Arc<dyn ApprovalUi> {
    Arc::new(HeadlessApprovalUi { policy })
}

#[cfg(test)]
struct StubTool(&'static str);
#[cfg(test)]
#[async_trait::async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        self.0
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}})
    }
    async fn execute(
        &self,
        _call: latent_agent::ToolCall,
        _cancel: tokio_util::sync::CancellationToken,
        _updater: &dyn latent_agent::ToolUpdater,
    ) -> Result<latent_agent::ToolOutput, latent_agent::ToolError> {
        Ok(latent_agent::ToolOutput::text("stub"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::SandboxConfig;
    use crate::SessionMode;
    use latent_ai::{ScriptedProvider, ScriptedTurn};

    fn model() -> Model {
        Model::minimal("mock-1", "mock", "mock")
    }

    fn def(name: &str, body: &str, tools: Option<Vec<&str>>) -> AgentDef {
        AgentDef {
            name: name.into(),
            description: String::new(),
            model: None,
            tools: tools.map(|list| list.into_iter().map(String::from).collect()),
            system_prompt: body.into(),
        }
    }

    /// 捕获记录:(system 提示词, 工具声明名单, 角色序列)
    type Captured = Vec<(String, Vec<String>, Vec<String>)>;

    /// 记录型 provider:捕获请求的 system 提示词、工具声明名单与角色序列。
    struct RecordingProvider {
        model: Model,
        captured: Mutex<Captured>,
    }

    #[async_trait::async_trait]
    impl latent_ai::Provider for RecordingProvider {
        async fn stream(
            &self,
            _model: &Model,
            ctx: latent_ai::TranscriptContext,
            _opts: latent_ai::StreamOptions,
        ) -> latent_ai::AssistantMessageEventStream {
            // normalize_context 已把 system 提示词与工具声明折叠进首条 System
            let (system, tools) = match ctx.messages.first() {
                Some(latent_ai::Message::System {
                    content, tools_added, ..
                }) => (
                    content.clone(),
                    tools_added.iter().map(|t| t.name.clone()).collect(),
                ),
                _ => (String::new(), Vec::new()),
            };
            let mut roles = Vec::new();
            for message in &ctx.messages {
                let (role, text) = match message {
                    latent_ai::Message::System { content, .. } => ("system", content.clone()),
                    latent_ai::Message::Developer { content, .. } => ("developer", content.clone()),
                    latent_ai::Message::User {
                        content: latent_ai::UserContent::Text(text),
                        ..
                    } => ("user", text.clone()),
                    _ => ("other", String::new()),
                };
                roles.push(format!("{role}:{text}"));
            }
            self.captured
                .lock()
                .unwrap()
                .push((system, tools, roles));
            let model = self.model.clone();
            Box::pin(async_stream::stream! {
                yield latent_ai::AssistantMessageEvent::Done(Box::new(
                    latent_ai::assistant_message(&model, vec![latent_ai::ContentBlock::text("ok")], latent_ai::StopReason::Stop),
                ));
            })
        }
    }

    #[tokio::test]
    async fn persisted_session_uses_agent_name_tag() {
        use crate::session::SessionSink;
        use latent_agent::AgentMessage;

        struct ManagerSink(Arc<latent_session::SessionManager>);
        #[async_trait::async_trait]
        impl SessionSink for ManagerSink {
            async fn append(&self, message: &AgentMessage) -> Result<(), String> {
                self.0
                    .append_message(message.clone())
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "latent-subagent-factory-persist-{}",
            std::process::id()
        ));
        let store_dir = dir.clone();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let provider = Arc::new(ScriptedProvider::new(
            &model(),
            vec![ScriptedTurn::text(&model(), "ok")],
        ));
        let factory = SubagentSessionFactory {
            provider,
            approval_ui: default_async_approval_ui(HeadlessApproval::Deny),
            engine: Arc::new(PermissionEngine::new(
                crate::SessionMode::FullAccess,
                SandboxConfig::default(),
                ApprovalRules::default(),
                std::env::temp_dir(),
                true,
            )),
            sandbox: SandboxConfig::default(),
            rules: ApprovalRules::default(),
            cwd: std::env::temp_dir(),
            sandbox_available: true,
            default_tools: vec![],
            tool_pool_factory: Arc::new(|_engine| vec![]),
            resolve_model: Arc::new(|spec: &str| Ok(Model::minimal(spec, "mock", "mock"))),
            child_store_factory: Some(Arc::new(move |tag: &str| {
                let manager = latent_session::create_session_in_dir(
                    &store_dir,
                    "/tmp/proj",
                    None,
                    Some(tag),
                )
                .map_err(|e| e.to_string())?;
                let sink: Arc<dyn SessionSink> = Arc::new(ManagerSink(manager.into()));
                Ok(crate::ChildStore {
                    sink,
                    stream_options: Default::default(),
                })
            })),
        };

        let session = factory
            .create(&def("reviewer", "body", None), model(), Arc::new(Mutex::new(Vec::new())))
            .await
            .unwrap();
        session.prompt("hi").await.unwrap();

        // 会话文件存在,文件名带 agent 名 tag,含消息条目;文件在
        // `<dir>/<项目目录>/` 下(含旧版式根目录兼容扫描)
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                for sub in std::fs::read_dir(&path).unwrap().flatten() {
                    if sub.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
                        files.push(sub.path());
                    }
                }
            } else if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
        assert_eq!(files.len(), 1, "{files:?}");
        let name = files[0].file_name().unwrap().to_string_lossy().to_string();
        assert!(name.contains("__reviewer__"), "{name}");
        let content = std::fs::read_to_string(&files[0]).unwrap();
        assert!(content.contains("hi"), "应含用户消息");
        assert!(content.contains("ok"), "应含 assistant 回复");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn creates_isolated_session_with_md_system_prompt_and_whitelist() {
        let provider = Arc::new(RecordingProvider {
            model: model(),
            captured: Mutex::new(Vec::new()),
        });
        let read_tool: Arc<dyn Tool> = Arc::new(StubTool("read"));
        let write_tool: Arc<dyn Tool> = Arc::new(StubTool("write"));
        let engine = Arc::new(PermissionEngine::new(
            crate::SessionMode::Plan,
            SandboxConfig::default(),
            ApprovalRules::default(),
            std::env::temp_dir(),
            true,
        ));
        let factory = SubagentSessionFactory {
            provider: provider.clone(),
            approval_ui: default_async_approval_ui(HeadlessApproval::Deny),
            engine: engine.clone(),
            sandbox: SandboxConfig::default(),
            rules: ApprovalRules::default(),
            cwd: std::env::temp_dir(),
            sandbox_available: true,
            default_tools: vec![read_tool.clone()],
            tool_pool_factory: Arc::new(move |_engine| {
                vec![read_tool.clone(), write_tool.clone()]
            }),
            resolve_model: Arc::new(|spec: &str| {
                if spec == "mock/child" {
                    Ok(Model::minimal("child", "mock", "mock"))
                } else {
                    Err(format!("unknown model: {spec}"))
                }
            }),
            child_store_factory: None,
        };

        // 白名单 [write]:write 进工具面,嵌套用的 subagent 被过滤
        let session = factory
            .create(
                &def("reviewer", "You are a reviewer.", Some(vec!["write", "subagent"])),
                model(),
                Arc::new(Mutex::new(Vec::new())),
            )
            .await
            .unwrap();
        assert_eq!(session.agent().state_snapshot().tool_count, 1);

        session.prompt("hi").await.unwrap();
        let captured = provider.captured.lock().unwrap().clone();
        let (system, tools, roles) = &captured[0];
        assert_eq!(system, "You are a reviewer.", "系统提示词 = md 正文");
        assert_eq!(tools, &vec!["write".to_string()]);
        // 主引擎处于 Plan 模式:子会话与主会话行为一致 —— 模式节(进入句)
        // 作为持久 ModeSection 消息 append 在转录(user 之前),非系统提示词
        assert_eq!(roles.len(), 3, "{roles:?}");
        assert!(roles[0].starts_with("system:You are a reviewer."));
        assert!(
            roles[1].starts_with("developer:You are entering Plan mode"),
            "模式节应为 Plan 进入提示词,append 在用户输入之前:{roles:?}"
        );
        assert_eq!(roles[2], "user:hi", "用户输入在历史末尾");
        // 定义 model 缺省 → 继承 fallback(主会话模型)
        assert_eq!(session.agent().state_snapshot().model.unwrap().id, "mock-1");

        // def.model 指定时覆盖
        let mut with_model = def("scoped", "s", None);
        with_model.model = Some("mock/child".into());
        let session2 = factory
            .create(&with_model, model(), Arc::new(Mutex::new(Vec::new())))
            .await
            .unwrap();
        assert_eq!(session2.agent().state_snapshot().model.unwrap().id, "child");
    }

    /// 工具池工厂收到的是**子会话自己的引擎**:子会话切档后,工厂捕获的
    /// 引擎模式同步变化(沙箱包装钩子据此即时生效,不钉死在主会话模式)。
    #[tokio::test]
    async fn tool_pool_factory_receives_child_engine() {
        let provider = Arc::new(ScriptedProvider::new(
            &model(),
            vec![ScriptedTurn::text(&model(), "ok")],
        ));
        let captured: Arc<Mutex<Vec<Arc<PermissionEngine>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let factory = SubagentSessionFactory {
            provider,
            approval_ui: default_async_approval_ui(HeadlessApproval::Deny),
            engine: Arc::new(PermissionEngine::new(
                SessionMode::Plan,
                SandboxConfig::default(),
                ApprovalRules::default(),
                std::env::temp_dir(),
                true,
            )),
            sandbox: SandboxConfig::default(),
            rules: ApprovalRules::default(),
            cwd: std::env::temp_dir(),
            sandbox_available: true,
            default_tools: vec![],
            tool_pool_factory: Arc::new(move |engine: Arc<PermissionEngine>| {
                sink.lock().unwrap().push(engine);
                vec![]
            }),
            resolve_model: Arc::new(|spec: &str| Ok(Model::minimal(spec, "mock", "mock"))),
            child_store_factory: None,
        };

        let child = factory
            .create(&def("reviewer", "body", None), model(), Arc::new(Mutex::new(Vec::new())))
            .await
            .unwrap();
        child.set_mode(SessionMode::FullAccess).await.unwrap();

        let engines = captured.lock().unwrap();
        assert_eq!(engines.len(), 1, "工具池工厂每会话调用一次");
        assert_eq!(
            engines[0].mode(),
            SessionMode::FullAccess,
            "工具池拿到的是子会话引擎(随子会话切档),而非主会话引擎"
        );
    }

}


