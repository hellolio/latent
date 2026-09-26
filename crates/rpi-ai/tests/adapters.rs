//! 适配器集成测试:本地 SSE 服务器 + 归一化事件流断言(02 文档 §1.2 流协议)。

use std::time::Duration;

use futures::StreamExt;
use rpi_ai::types::{
    AssistantMessageEvent, CacheRetention, Model, StopReason, StreamOptions, TranscriptContext,
};
use rpi_ai::{create_anthropic_adapter, create_openai_completions_adapter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// 起一个一次性 HTTP/SSE 服务器,返回 (base_url, 句柄)。
/// `chunks` 逐块写出;`hold_after_ms` 为 None 时写完立即断开,否则最后一块后挂住。
async fn spawn_sse_server(
    status: u16,
    chunks: Vec<String>,
    hold_after_ms: Option<u64>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // 读完整请求头(以 \r\n\r\n 结束),避免响应早于请求被 RST
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                read = socket.read(&mut buf) => {
                    match read {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            request.extend_from_slice(&buf[..n]);
                            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                _ = timeout => break,
            }
        }
        if std::env::var("RPI_DEBUG").is_ok() {
            eprintln!("server got request: {} bytes", request.len());
        }
        let reason = if status == 200 { "OK" } else { "Error" };
        let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
        let _ = socket.write_all(head.as_bytes()).await;
        for chunk in &chunks {
            let _ = socket.write_all(chunk.as_bytes()).await;
            let _ = socket.flush().await;
        }
        if let Some(ms) = hold_after_ms {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        } else {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // drop(socket) 断开
    });
    (format!("http://{addr}"), handle)
}

async fn collect(stream: rpi_ai::AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    let mut stream = stream;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if std::env::var("RPI_DEBUG").is_ok() {
            eprintln!("event: {event:?}");
        }
        let terminal = matches!(
            event,
            AssistantMessageEvent::Done(_) | AssistantMessageEvent::Error(_)
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

fn anthropic_model(base_url: &str) -> Model {
    let mut model = Model::minimal("claude-test", "anthropic-messages", "anthropic");
    model.base_url = base_url.to_string();
    model
}

fn openai_model(base_url: &str) -> Model {
    let mut model = Model::minimal("gpt-test", "openai-completions", "openai");
    model.base_url = base_url.to_string();
    model
}

fn key_opts() -> StreamOptions {
    StreamOptions {
        api_key: Some("test-key".into()),
        ..Default::default()
    }
}

fn sse_lines(lines: &[&str]) -> String {
    lines.iter().map(|l| format!("data: {l}\n\n")).collect()
}

#[tokio::test]
async fn anthropic_streams_text_and_usage() {
    let body = sse_lines(&[
        r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":5,"cache_creation_input_tokens":2}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"世界"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let (base, server) = spawn_sse_server(200, vec![body], None).await;
    let provider = create_anthropic_adapter();
    let events = collect(
        provider
            .stream(
                &anthropic_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();

    assert!(matches!(events.first(), Some(AssistantMessageEvent::Start)));
    assert!(events
        .iter()
        .find(|e| matches!(e, AssistantMessageEvent::TextDelta { delta, .. } if delta == "你好"))
        .is_some());
    assert!(events
        .iter()
        .find(|e| matches!(e, AssistantMessageEvent::TextEnd { .. }))
        .is_some());
    match events.last() {
        Some(AssistantMessageEvent::Done(message)) => {
            assert_eq!(message.text_content(), "你好世界");
            assert_eq!(message.stop_reason, StopReason::Stop);
            assert_eq!(message.response_id.as_deref(), Some("msg_1"));
            assert_eq!(message.usage.input, 10);
            assert_eq!(message.usage.cache_read, 5);
            assert_eq!(message.usage.cache_write, 2);
            assert_eq!(message.usage.output, 7);
            assert_eq!(message.usage.total_tokens, 24);
        }
        other => panic!("expected done, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_streams_tool_call_with_partial_json() {
    let body = sse_lines(&[
        r#"{"type":"message_start","message":{"id":"msg_2","model":"claude-test","usage":{"input_tokens":3,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"a"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":".txt\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let (base, server) = spawn_sse_server(200, vec![body], None).await;
    let provider = create_anthropic_adapter();
    let events = collect(
        provider
            .stream(
                &anthropic_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();

    assert!(events
        .iter()
        .any(|e| matches!(e, AssistantMessageEvent::ToolCallDelta { delta, .. } if delta == "{\"path\": \"a")));
    match events.last() {
        Some(AssistantMessageEvent::Done(message)) => {
            assert_eq!(message.stop_reason, StopReason::ToolUse);
            match message.content.first() {
                Some(rpi_ai::types::ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                }) => {
                    assert_eq!(id, "toolu_1");
                    assert_eq!(name, "read");
                    assert_eq!(*arguments, serde_json::json!({"path": "a.txt"}));
                }
                other => panic!("expected tool call, got {other:?}"),
            }
        }
        other => panic!("expected done, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_maps_stop_reasons_and_errors() {
    // max_tokens → Length
    let body = sse_lines(&[
        r#"{"type":"message_start","message":{"id":"m","model":"claude-test","usage":{}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let (base, server) = spawn_sse_server(200, vec![body], None).await;
    let provider = create_anthropic_adapter();
    let events = collect(
        provider
            .stream(
                &anthropic_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();
    match events.last() {
        Some(AssistantMessageEvent::Done(message)) => {
            assert_eq!(message.stop_reason, StopReason::Length)
        }
        other => panic!("expected done, got {other:?}"),
    }

    // SSE error 事件 → Error 终态
    let body =
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n";
    let (base, server) = spawn_sse_server(200, vec![body.into()], None).await;
    let events = collect(
        create_anthropic_adapter()
            .stream(
                &anthropic_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();
    match events.last() {
        Some(AssistantMessageEvent::Error(message)) => {
            assert_eq!(message.stop_reason, StopReason::Error);
            assert!(message
                .error_message
                .as_deref()
                .unwrap()
                .contains("overloaded"));
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_http_error_is_classified_retryable() {
    let (base, server) = spawn_sse_server(
        429,
        vec!["{\"error\":{\"message\":\"rate limit exceeded\"}}".into()],
        None,
    )
    .await;
    let events = collect(
        create_anthropic_adapter()
            .stream(
                &anthropic_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();
    match events.as_slice() {
        [AssistantMessageEvent::Error(message)] => {
            assert!(message.error_message.as_deref().unwrap().contains("429"));
            // 请求建立失败:不出 start
            assert!(!events
                .iter()
                .any(|e| matches!(e, AssistantMessageEvent::Start)));
            assert!(rpi_ai::is_retryable_assistant_error(message));
        }
        other => panic!("expected single error event, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_builtin_provider_missing_env_key_is_setup_error() {
    let (base, server) = spawn_sse_server(200, vec![String::new()], None).await;
    // 白名单内 provider 缺 env key:请求前 setup error(不发起连接)。
    // 选 xiaomi 是因为它基本不会出现在宿主环境里
    let mut model = anthropic_model(&base);
    model.provider = "xiaomi".into();
    let events = collect(
        create_anthropic_adapter()
            .stream(
                &model,
                TranscriptContext { messages: vec![] },
                StreamOptions::default(),
            )
            .await,
    )
    .await;
    server.abort(); // 适配器缺 key 时不发起连接,服务器不会 accept
    match events.as_slice() {
        [AssistantMessageEvent::Error(message)] => {
            assert!(message
                .error_message
                .as_deref()
                .unwrap()
                .contains("No API key"));
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_keyless_custom_provider_proceeds_without_auth() {
    let (base, server) = spawn_sse_server(200, vec![String::new()], None).await;
    // 白名单外自定义 provider 无 key(models.json 体系):请求照发(不带
    // x-api-key),不再前置报 "No API key";空响应流产生的是流错误
    let mut model = anthropic_model(&base);
    model.provider = "provider-without-env".into();
    let events = collect(
        create_anthropic_adapter()
            .stream(
                &model,
                TranscriptContext { messages: vec![] },
                StreamOptions::default(),
            )
            .await,
    )
    .await;
    server.await.unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::Error(m)
            if m.error_message.as_deref().unwrap_or_default().contains("No API key"))),
        "自定义 provider 不应因缺 key 被前置拦截,got {events:?}"
    );
}

#[tokio::test]
async fn anthropic_stream_end_without_message_stop_is_error() {
    // message_start 后直接断流(可重试的流早断)
    let body = sse_lines(&[
        r#"{"type":"message_start","message":{"id":"m","model":"claude-test","usage":{}}}"#,
    ]);
    let (base, server) = spawn_sse_server(200, vec![body], None).await;
    let events = collect(
        create_anthropic_adapter()
            .stream(
                &anthropic_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();
    match events.last() {
        Some(AssistantMessageEvent::Error(message)) => {
            assert!(message
                .error_message
                .as_deref()
                .unwrap()
                .contains("message_stop"));
            assert!(rpi_ai::is_retryable_assistant_error(message));
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_streams_text_reasoning_and_usage() {
    let body = sse_lines(&[
        r#"{"id":"c1","model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"gpt-test","choices":[{"index":0,"delta":{"reasoning_content":"想一下"},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"gpt-test","choices":[{"index":0,"delta":{"content":"你好"},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"gpt-test","choices":[{"index":0,"delta":{"content":"世界"},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":30}}}"#,
        "[DONE]",
    ]);
    let (base, server) = spawn_sse_server(200, vec![body], None).await;
    let events = collect(
        create_openai_completions_adapter()
            .stream(
                &openai_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();

    assert!(matches!(events.first(), Some(AssistantMessageEvent::Start)));
    assert!(events.iter().any(
        |e| matches!(e, AssistantMessageEvent::ThinkingDelta { delta, .. } if delta == "想一下")
    ));
    assert!(events
        .iter()
        .any(|e| matches!(e, AssistantMessageEvent::TextDelta { delta, .. } if delta == "你好")));
    match events.last() {
        Some(AssistantMessageEvent::Done(message)) => {
            assert_eq!(message.text_content(), "你好世界");
            assert_eq!(message.stop_reason, StopReason::Stop);
            assert_eq!(message.usage.input, 70); // 100 - 30 cached
            assert_eq!(message.usage.cache_read, 30);
            assert_eq!(message.usage.output, 50);
        }
        other => panic!("expected done, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_streams_tool_calls() {
    let body = sse_lines(&[
        r#"{"id":"c2","model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
        r#"{"id":"c2","model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
        r#"{"id":"c2","model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]},"finish_reason":null}]}"#,
        r#"{"id":"c2","model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let (base, server) = spawn_sse_server(200, vec![body], None).await;
    let events = collect(
        create_openai_completions_adapter()
            .stream(
                &openai_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();

    match events.last() {
        Some(AssistantMessageEvent::Done(message)) => {
            assert_eq!(message.stop_reason, StopReason::ToolUse);
            match message.content.first() {
                Some(rpi_ai::types::ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                }) => {
                    assert_eq!(id, "call_a");
                    assert_eq!(name, "read");
                    assert_eq!(*arguments, serde_json::json!({"path": "a.txt"}));
                }
                other => panic!("expected tool call, got {other:?}"),
            }
        }
        other => panic!("expected done, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_finish_reason_error_maps_to_error_event() {
    let body = sse_lines(&[
        r#"{"id":"c3","model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}"#,
    ]);
    let (base, server) = spawn_sse_server(200, vec![body], None).await;
    let events = collect(
        create_openai_completions_adapter()
            .stream(
                &openai_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();
    match events.last() {
        Some(AssistantMessageEvent::Error(message)) => {
            assert!(message
                .error_message
                .as_deref()
                .unwrap()
                .contains("content_filter"));
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_stream_without_finish_reason_errors_when_supported() {
    // 有 finish_reason 支持(compat 默认)却没有收到 → 错误(pi 语义)
    let body = sse_lines(&[
        r#"{"id":"c4","model":"gpt-test","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
    ]);
    let (base, server) = spawn_sse_server(200, vec![body], None).await;
    let events = collect(
        create_openai_completions_adapter()
            .stream(
                &openai_model(&base),
                TranscriptContext { messages: vec![] },
                key_opts(),
            )
            .await,
    )
    .await;
    server.await.unwrap();
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error(_))
    ));
}

#[tokio::test]
async fn cancel_mid_stream_yields_aborted_error() {
    let first = sse_lines(&[
        r#"{"id":"c5","model":"gpt-test","choices":[{"index":0,"delta":{"content":"部分"},"finish_reason":null}]}"#,
    ]);
    let (base, server) = spawn_sse_server(200, vec![first], Some(30_000)).await;
    let cancel = CancellationToken::new();
    let opts = StreamOptions {
        api_key: Some("test-key".into()),
        cancel: Some(cancel.clone()),
        ..Default::default()
    };
    let stream = create_openai_completions_adapter()
        .stream(
            &openai_model(&base),
            TranscriptContext { messages: vec![] },
            opts,
        )
        .await;
    let handle = tokio::spawn(collect(stream));
    // 等首块送达后取消
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();
    let events = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
    match events.last() {
        Some(AssistantMessageEvent::Error(message)) => {
            assert_eq!(message.stop_reason, StopReason::Aborted);
        }
        other => panic!("expected aborted error, got {other:?}"),
    }
    server.abort();
}

#[tokio::test]
async fn anthropic_cache_retention_none_stream_succeeds() {
    // cacheRetention=none 是受支持取值:请求应正常建立并流式收尾。
    // none 时请求体省略 cache_control 字段的行为由 anthropic 单元测试覆盖。
    let body = sse_lines(&[
        r#"{"type":"message_start","message":{"id":"m","model":"claude-test","usage":{}}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let (base, server) = spawn_sse_server(200, vec![body], None).await;
    let opts = StreamOptions {
        api_key: Some("k".into()),
        cache_retention: Some(CacheRetention::None),
        ..Default::default()
    };
    let events = collect(
        create_anthropic_adapter()
            .stream(
                &anthropic_model(&base),
                TranscriptContext { messages: vec![] },
                opts,
            )
            .await,
    )
    .await;
    server.await.unwrap();
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done(_))
    ));
}

/// 起一个"响应头挂住"的一次性 HTTP 服务器:读请求后延迟 `hold_head_ms`
/// 才回响应头,模拟上游迟迟不响应(cancel 必须能在 send 阶段生效)。
async fn spawn_slow_head_server(hold_head_ms: u64) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                read = socket.read(&mut buf) => {
                    match read {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            request.extend_from_slice(&buf[..n]);
                            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                _ = timeout => break,
            }
        }
        tokio::time::sleep(Duration::from_millis(hold_head_ms)).await;
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.flush().await;
        tokio::time::sleep(Duration::from_millis(30_000)).await;
    });
    (format!("http://{addr}"), handle)
}

/// cancel 在响应头到达前触发:send 阶段(连接挂起)必须可中断并产出
/// aborted 终态(此前 cancel 只覆盖响应体读取,上游不响应时 Ctrl+C 无效)。
#[tokio::test]
async fn cancel_before_response_head_yields_aborted_error_anthropic() {
    let (base, server) = spawn_slow_head_server(30_000).await;
    let cancel = CancellationToken::new();
    let opts = StreamOptions {
        api_key: Some("test-key".into()),
        cancel: Some(cancel.clone()),
        ..Default::default()
    };
    let stream = create_anthropic_adapter()
        .stream(
            &anthropic_model(&base),
            TranscriptContext { messages: vec![] },
            opts,
        )
        .await;
    let handle = tokio::spawn(collect(stream));
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();
    let events = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
    match events.last() {
        Some(AssistantMessageEvent::Error(message)) => {
            assert_eq!(message.stop_reason, StopReason::Aborted);
        }
        other => panic!("expected aborted error, got {other:?}"),
    }
    server.abort();
}

#[tokio::test]
async fn cancel_before_response_head_yields_aborted_error_openai() {
    let (base, server) = spawn_slow_head_server(30_000).await;
    let cancel = CancellationToken::new();
    let opts = StreamOptions {
        api_key: Some("test-key".into()),
        cancel: Some(cancel.clone()),
        ..Default::default()
    };
    let stream = create_openai_completions_adapter()
        .stream(
            &openai_model(&base),
            TranscriptContext { messages: vec![] },
            opts,
        )
        .await;
    let handle = tokio::spawn(collect(stream));
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();
    let events = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
    match events.last() {
        Some(AssistantMessageEvent::Error(message)) => {
            assert_eq!(message.stop_reason, StopReason::Aborted);
        }
        other => panic!("expected aborted error, got {other:?}"),
    }
    server.abort();
}
