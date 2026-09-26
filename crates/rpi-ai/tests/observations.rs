//! 观察回调验收(11 计划 T3):on_payload 可检查/替换请求体、on_response 观察
//! HTTP 响应、on_provider_stream_event 观察归一化前的原始事件;回调 panic
//! 不击穿流;不装配时路径行为不变(既有测试全集覆盖)。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use rpi_ai::types::{
    AssistantMessageEvent, Model, StreamOptions, TranscriptContext,
};
use rpi_ai::{create_anthropic_adapter, create_openai_completions_adapter};
use serde_json::Value;

/// 一次性 HTTP/SSE 服务器:读完整请求(含 JSON body)存入共享槽,再写 SSE 响应。
async fn spawn_capturing_server(
    chunks: Vec<String>,
) -> (String, Arc<Mutex<Option<Value>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_task = captured.clone();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        // 读到请求头结束;再按 content-length 读完 body
        loop {
            let read = socket.read(&mut buf).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
            let header_end = request
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| p + 4);
            if let Some(header_end) = header_end {
                let length = std::str::from_utf8(&request[..header_end])
                    .ok()
                    .and_then(|head| {
                        head.to_ascii_lowercase()
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().to_string()))
                    })
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= header_end + length {
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        // 提取 JSON body
        if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let body = &request[pos + 4..];
            if let Ok(value) = serde_json::from_slice::<Value>(body) {
                *captured_for_task.lock().unwrap() = Some(value);
            }
        }
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        for chunk in &chunks {
            let _ = socket.write_all(chunk.as_bytes()).await;
            let _ = socket.flush().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    (format!("http://{addr}"), captured, handle)
}

fn anthropic_model(base_url: &str) -> Model {
    let mut model = Model::minimal("claude-test", "anthropic-messages", "anthropic");
    model.base_url = base_url.to_string();
    model
}

fn anthropic_chunks() -> Vec<String> {
    vec![
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n".into(),
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".into(),
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n".into(),
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n".into(),
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n".into(),
        "data: {\"type\":\"message_stop\"}\n\n".into(),
    ]
}

async fn collect(mut stream: rpi_ai::AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let terminal = matches!(event, AssistantMessageEvent::Done(_) | AssistantMessageEvent::Error(_));
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

/// on_payload:观察到原始请求体,且可就地替换(replacement 生效于发出的请求)。
#[tokio::test]
async fn on_payload_observes_and_replaces_request_body() {
    let (base_url, captured, server) = spawn_capturing_server(anthropic_chunks()).await;
    let m = anthropic_model(&base_url);

    let observed_model = Arc::new(Mutex::new(None::<String>));
    let observed_for_cb = observed_model.clone();
    let on_payload: rpi_ai::OnPayload = Arc::new(move |body: &mut Value| {
        observed_for_cb
            .lock()
            .unwrap()
            .replace(body.get("model").and_then(|v| v.as_str()).unwrap_or_default().to_string());
        // 替换:改写 model 字段(替换语义验证)
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".into(), Value::String("rewritten-model".into()));
        }
    });
    let opts = StreamOptions {
        api_key: Some("k".into()),
        on_payload: Some(on_payload),
        ..Default::default()
    };

    let stream = create_anthropic_adapter()
        .stream(&m, TranscriptContext { messages: vec![] }, opts)
        .await;
    let events = collect(stream).await;
    assert!(matches!(events.last(), Some(AssistantMessageEvent::Done(_))), "{events:?}");

    // 回调观察到的是替换前的原始 model
    assert_eq!(observed_model.lock().unwrap().as_deref(), Some("claude-test"));
    // 服务器收到的是替换后的 body
    let sent = captured.lock().unwrap().clone().expect("server captured body");
    assert_eq!(sent["model"], "rewritten-model");
    server.await.unwrap();
}

/// on_response + on_provider_stream_event:观察到 HTTP 响应与原始 provider 事件。
#[tokio::test]
async fn on_response_and_raw_event_observers_fire() {
    let (base_url, _captured, server) = spawn_capturing_server(anthropic_chunks()).await;
    let m = anthropic_model(&base_url);

    let statuses = Arc::new(Mutex::new(Vec::<u16>::new()));
    let raw_types = Arc::new(Mutex::new(Vec::<String>::new()));

    let statuses_for_cb = statuses.clone();
    let on_response: rpi_ai::OnResponse = Arc::new(move |observation| {
        statuses_for_cb.lock().unwrap().push(observation.status);
    });
    let raw_for_cb = raw_types.clone();
    let on_raw: rpi_ai::OnProviderStreamEvent = Arc::new(move |event: &Value| {
        raw_for_cb
            .lock()
            .unwrap()
            .push(event.get("type").and_then(|v| v.as_str()).unwrap_or_default().to_string());
    });
    let opts = StreamOptions {
        api_key: Some("k".into()),
        on_response: Some(on_response),
        on_provider_stream_event: Some(on_raw),
        ..Default::default()
    };

    let stream = create_anthropic_adapter()
        .stream(&m, TranscriptContext { messages: vec![] }, opts)
        .await;
    let events = collect(stream).await;
    assert!(matches!(events.last(), Some(AssistantMessageEvent::Done(_))));

    assert_eq!(*statuses.lock().unwrap(), vec![200]);
    let types = raw_types.lock().unwrap().clone();
    drop(raw_types);
    assert!(types.contains(&"message_start".to_string()), "应观察到原始 message_start: {types:?}");
    assert!(types.contains(&"content_block_delta".to_string()));
    assert!(types.contains(&"message_stop".to_string()));
    server.await.unwrap();
}

/// 回调 panic 不击穿流(policy §2):三个回调各自 panic,流仍以 Done 终态收尾。
#[tokio::test]
async fn observer_panics_do_not_break_the_stream() {
    let (base_url, _captured, server) = spawn_capturing_server(anthropic_chunks()).await;
    let m = anthropic_model(&base_url);

    let on_payload: rpi_ai::OnPayload = Arc::new(|_: &mut Value| panic!("payload observer boom"));
    let on_response: rpi_ai::OnResponse = Arc::new(|_: &rpi_ai::ResponseObservation| panic!("response observer boom"));
    let on_raw: rpi_ai::OnProviderStreamEvent = Arc::new(|_: &Value| panic!("raw observer boom"));
    let opts = StreamOptions {
        api_key: Some("k".into()),
        on_payload: Some(on_payload),
        on_response: Some(on_response),
        on_provider_stream_event: Some(on_raw),
        ..Default::default()
    };

    let stream = create_anthropic_adapter()
        .stream(&m, TranscriptContext { messages: vec![] }, opts)
        .await;
    let events = collect(stream).await;
    assert!(matches!(events.last(), Some(AssistantMessageEvent::Done(_))), "panic 不得击穿流: {events:?}");
    server.await.unwrap();
}

/// openai-completions 适配器同样走观察回调(替换生效)。
#[tokio::test]
async fn openai_adapter_observes_payload() {
    let (base_url, captured, server) = spawn_capturing_server(vec![
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"h\"}}]}\n\n".into(),
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".into(),
        "data: [DONE]\n\n".into(),
    ])
    .await;
    let mut m = Model::minimal("gpt-test", "openai-completions", "openai");
    m.base_url = base_url;

    let on_payload: rpi_ai::OnPayload = Arc::new(|body: &mut Value| {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".into(), Value::String("rewritten-model".into()));
        }
    });
    let opts = StreamOptions {
        api_key: Some("k".into()),
        on_payload: Some(on_payload),
        ..Default::default()
    };
    let stream = create_openai_completions_adapter()
        .stream(&m, TranscriptContext { messages: vec![] }, opts)
        .await;
    let events = collect(stream).await;
    assert!(matches!(events.last(), Some(AssistantMessageEvent::Done(_))), "{events:?}");

    let sent = captured.lock().unwrap().clone().expect("server captured body");
    assert_eq!(sent["model"], "rewritten-model");
    server.await.unwrap();
}

/// 未装配回调时 StreamOptions 仍可 Clone(热路径零开销的前提)。
#[tokio::test]
async fn stream_options_cloneable_without_observers() {
    let opts = StreamOptions::default();
    let cloned = opts.clone();
    assert!(cloned.on_payload.is_none());
}

/// DeferredHandle 纯类型 roundtrip(01 文档 §2 形态)。
#[test]
fn deferred_handle_serializes_camel_case() {
    let handle = rpi_ai::DeferredHandle {
        provider: "openai".into(),
        model_id: "gpt".into(),
        api: "openai-responses".into(),
        id: "job-1".into(),
        expires_at: Some(123),
        poll_after_ms: Some(5000),
        data: None,
    };
    let value = serde_json::to_value(&handle).unwrap();
    assert_eq!(value["modelId"], "gpt");
    assert_eq!(value["pollAfterMs"], 5000);
    let back: rpi_ai::DeferredHandle = serde_json::from_value(value).unwrap();
    assert_eq!(back, handle);
}
