//! 手工端到端验证(需要网络):直接驱动 web 工具,不经模型。
//! `cargo run -p latent-web --example e2e_manual`

use std::sync::Arc;

use latent_agent::{Tool, ToolCall};
use latent_web::llm::LlmDeps;
use latent_web::tools::{names, WebContext};
use tokio_util::sync::CancellationToken;

struct NoopUpdater;

#[async_trait::async_trait]
impl latent_agent::ToolUpdater for NoopUpdater {
    async fn update(&self, _partial: String) {}
}

fn context() -> Arc<WebContext> {
    Arc::new(WebContext {
        config: latent_web::config::load_web_search_config(None, None),
        cache_limits: Default::default(),
        llm: LlmDeps {
            provider: latent_ai::create_mock_provider("unused"),
            resolve_model: Arc::new(|spec| Ok(latent_ai::Model::minimal(spec, "mock", "mock"))),
            current_model: Arc::new(|| None),
        },
        notifier: None,
        tool_result_max_chars: 0,
        cwd: std::env::current_dir().unwrap(),
    })
}

#[tokio::main]
async fn main() {
    let context = context();
    let tools = latent_web::tools::create_web_tools(context);
    let find = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name() == name)
            .expect("tool registered")
            .as_ref()
    };

    // 1. 真实搜索(duckduckgo 兜底)
    let search_output = run(
        find(names::WEB_SEARCH),
        names::WEB_SEARCH,
        serde_json::json!({
            "queries": ["Rust programming language official site"],
            "numResults": 3
        }),
    )
    .await;
    let search_id = search_output["searchId"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    // 2. fetch readable
    let fetch_output = run(
        find(names::FETCH_CONTENT),
        names::FETCH_CONTENT,
        serde_json::json!({
            "url": "https://www.rust-lang.org/",
            "mode": "readable"
        }),
    )
    .await;
    let fetch_id = fetch_output["responseId"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    // 3. get_search_content:search 全量结果第一页
    run(
        find(names::GET_SEARCH_CONTENT),
        names::GET_SEARCH_CONTENT,
        serde_json::json!({ "responseId": search_id, "queryIndex": 0, "offset": 0, "limit": 400 }),
    )
    .await;

    // 4. fetch 全文 findText(fuzzy)
    run(
        find(names::GET_SEARCH_CONTENT),
        names::GET_SEARCH_CONTENT,
        serde_json::json!({
            "responseId": fetch_id,
            "urlIndex": 0,
            "findText": ["performanse memory safe"],  // 故意拼写误差,验证 fuzzy
            "findMode": "fuzzy"
        }),
    )
    .await;
}

/// 返回 details(供后续步骤取 responseId);工具错误打印后返回 Null(容错续跑)。
async fn run(tool: &dyn Tool, name: &str, args: serde_json::Value) -> serde_json::Value {
    let call = ToolCall {
        id: format!("call-{name}"),
        name: name.to_string(),
        args,
    };
    match tool.execute(call, CancellationToken::new(), &NoopUpdater).await {
        Ok(output) => {
            println!("=== {name} ===");
            println!("{}", output.output);
            println!("--- details: {}", output.details);
            println!();
            output.details
        }
        Err(error) => {
            println!("=== {name} (tool error) ===");
            println!("{error}");
            println!();
            serde_json::Value::Null
        }
    }
}
