//! 流式重试装饰器验收(11 计划 T1):成功路径逐 delta 到达(不是终态一次性)、
//! 失败重试路径无重复 delta、提交点后失败不中途重试(policy §4 失败编码进流)。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use rpi_ai::{
    create_retrying_provider, AssistantMessage, AssistantMessageEvent, Model, Provider,
    RetryCallbacks, RetryPolicy, StreamOptions, TranscriptContext,
};
use tokio::sync::oneshot;

fn model() -> Model {
    Model::minimal("mock-1", "mock", "mock")
}

/// 门控 provider:产出 Start/TextStart/TextDelta("part") 后等待放行信号,
/// 之后再产出 Done —— 消费端必须在终态前收到 delta,否则死锁。
struct GateProvider {
    rx: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl Provider for GateProvider {
    async fn stream(
        &self,
        model: &Model,
        _ctx: TranscriptContext,
        _opts: StreamOptions,
    ) -> rpi_ai::AssistantMessageEventStream {
        let model = model.clone();
        let rx = self.rx.lock().unwrap().take();
        Box::pin(async_stream::stream! {
            yield AssistantMessageEvent::Start;
            yield AssistantMessageEvent::TextStart { content_index: 0 };
            yield AssistantMessageEvent::TextDelta { content_index: 0, delta: "part".into() };
            if let Some(rx) = rx {
                // 消费端收到 delta 并放行后,流才继续
                let _ = rx.await;
            }
            let mut message = AssistantMessage::pending(&model);
            message.content = vec![rpi_ai::ContentBlock::text("part")];
            message.stop_reason = rpi_ai::StopReason::Stop;
            yield AssistantMessageEvent::Done(Box::new(message));
        })
    }
}

#[derive(Default)]
struct Hooks {
    scheduled: AtomicU32,
    finished: std::sync::Mutex<Vec<(bool, u32)>>,
}

impl RetryCallbacks for Hooks {
    fn on_retry_scheduled(&self, _a: u32, _m: u32, _d: u64, _e: &str) {
        self.scheduled.fetch_add(1, Ordering::SeqCst);
    }
    fn on_retry_finished(&self, success: bool, attempt: u32, _e: Option<&str>) {
        self.finished.lock().unwrap().push((success, attempt));
    }
}

async fn collect(stream: &mut rpi_ai::AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
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

/// 成功路径:delta 在终态前到达(逐帧直通,不是整体缓冲回放)。
#[tokio::test]
async fn success_path_forwards_deltas_before_terminal() {
    let (tx, rx) = oneshot::channel();
    let provider = create_retrying_provider(
        Arc::new(GateProvider {
            rx: std::sync::Mutex::new(Some(rx)),
        }),
        RetryPolicy {
            base_delay_ms: 1,
            ..Default::default()
        },
        None,
    );
    let m = model();
    let mut stream = provider
        .stream(
            &m,
            TranscriptContext { messages: vec![] },
            StreamOptions::default(),
        )
        .await;

    // 先收到 delta(收到即证明未缓冲到终态)
    let first = stream.next().await.expect("start");
    assert!(matches!(first, AssistantMessageEvent::Start));
    let second = stream.next().await.expect("text_start");
    assert!(matches!(second, AssistantMessageEvent::TextStart { .. }));
    let third = stream.next().await.expect("delta");
    assert!(matches!(third, AssistantMessageEvent::TextDelta { .. }));

    // 放行,流继续到终态
    tx.send(()).unwrap();
    let rest = collect(&mut stream).await;
    assert!(
        matches!(rest.last(), Some(AssistantMessageEvent::Done(_))),
        "{rest:?}"
    );
}

/// 失败重试:提交点前的失败被静默截断,下游无重复 delta/Start。
#[tokio::test]
async fn retryable_failure_before_first_delta_leaves_no_duplicate_frames() {
    let m = model();
    // 第 1 轮:Start + 可重试 Error;第 2 轮:完整成功流
    let scripted = rpi_ai::ScriptedProvider::new(
        &m,
        vec![
            {
                let mut turn = rpi_ai::ScriptedTurn::error(&m, "HTTP 503 Service Unavailable");
                turn.delay_ms = 0;
                turn
            },
            rpi_ai::ScriptedTurn::text(&m, "recovered"),
        ],
    );
    let hooks = Arc::new(Hooks::default());
    let provider = create_retrying_provider(
        Arc::new(scripted),
        RetryPolicy {
            base_delay_ms: 1,
            ..Default::default()
        },
        Some(hooks.clone()),
    );
    let mut stream = provider
        .stream(
            &m,
            TranscriptContext { messages: vec![] },
            StreamOptions::default(),
        )
        .await;
    let events = collect(&mut stream).await;

    // 无重复 Start;失败尝试的 Error 帧不泄漏;内容 delta 只有成功尝试的
    let starts = events
        .iter()
        .filter(|e| matches!(e, AssistantMessageEvent::Start))
        .count();
    assert_eq!(starts, 1, "应只有成功尝试的 Start: {events:?}");
    let errors = events
        .iter()
        .filter(|e| matches!(e, AssistantMessageEvent::Error(_)))
        .count();
    assert_eq!(errors, 0, "重试成功的路径不应有 Error 终态: {events:?}");
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done(_))
    ));
    assert_eq!(hooks.scheduled.load(Ordering::SeqCst), 1);
    assert_eq!(*hooks.finished.lock().unwrap(), vec![(true, 1)]);
}

/// 提交点(首个 delta)之后的可重试失败:按失败编码进流直通,不中途重试。
#[tokio::test]
async fn failure_after_content_committed_is_not_retried() {
    // 自定义 provider:一次调用,产出 Start/TextStart/delta 后 Error(可重试文案)
    struct MidStreamError;
    #[async_trait]
    impl Provider for MidStreamError {
        async fn stream(
            &self,
            model: &Model,
            _ctx: TranscriptContext,
            _opts: StreamOptions,
        ) -> rpi_ai::AssistantMessageEventStream {
            let model = model.clone();
            Box::pin(async_stream::stream! {
                yield AssistantMessageEvent::Start;
                yield AssistantMessageEvent::TextStart { content_index: 0 };
                yield AssistantMessageEvent::TextDelta { content_index: 0, delta: "partial".into() };
                yield AssistantMessageEvent::Error(Box::new(AssistantMessage::error(
                    &model, "HTTP 503 overloaded", false,
                )));
            })
        }
    }
    let hooks = Arc::new(Hooks::default());
    let provider = create_retrying_provider(
        Arc::new(MidStreamError),
        RetryPolicy {
            base_delay_ms: 1,
            ..Default::default()
        },
        Some(hooks.clone()),
    );
    let m = model();
    let mut stream = provider
        .stream(
            &m,
            TranscriptContext { messages: vec![] },
            StreamOptions::default(),
        )
        .await;
    let events = collect(&mut stream).await;

    let deltas = events
        .iter()
        .filter(|e| matches!(e, AssistantMessageEvent::TextDelta { .. }))
        .count();
    assert_eq!(deltas, 1, "内容 delta 已流出");
    assert!(
        matches!(events.last(), Some(AssistantMessageEvent::Error(_))),
        "失败编码进流: {events:?}"
    );
    assert_eq!(hooks.scheduled.load(Ordering::SeqCst), 0, "提交点后不重试");
    assert_eq!(
        *hooks.finished.lock().unwrap(),
        vec![],
        "未发生重试则无 finished 上报"
    );
}

/// P0 回归:空回复(无任何内容 delta 的 Done)经重试装饰仍以 Done 终态收尾,
/// 不得被"流无终态"兜底改写成 error。
#[tokio::test]
async fn empty_reply_through_retrying_provider_still_ends_done() {
    let m = model();
    let provider = create_retrying_provider(
        Arc::new(rpi_ai::MockProvider::new("")),
        RetryPolicy {
            base_delay_ms: 1,
            ..Default::default()
        },
        None,
    );
    let mut stream = provider
        .stream(
            &m,
            TranscriptContext { messages: vec![] },
            StreamOptions::default(),
        )
        .await;
    let events = collect(&mut stream).await;
    match events.last() {
        Some(AssistantMessageEvent::Done(message)) => {
            assert_eq!(message.text_content(), "");
            assert_eq!(message.stop_reason, rpi_ai::StopReason::Stop);
        }
        other => panic!("空回复应以 Done 收尾, got {other:?}"),
    }
}

/// aborted / quota / 退避中取消:一律不重试,直接以终态收尾。
#[tokio::test]
async fn aborted_and_quota_terminate_without_retry() {
    let m = model();
    for message in ["Request was aborted", "insufficient_quota: billing"] {
        let scripted = rpi_ai::ScriptedProvider::new(
            &m,
            vec![
                rpi_ai::ScriptedTurn::error(&m, message),
                rpi_ai::ScriptedTurn::text(&m, "should not be reached"),
            ],
        );
        let hooks = Arc::new(Hooks::default());
        let provider = create_retrying_provider(
            Arc::new(scripted),
            RetryPolicy {
                base_delay_ms: 1,
                ..Default::default()
            },
            Some(hooks.clone()),
        );
        let mut stream = provider
            .stream(
                &m,
                TranscriptContext { messages: vec![] },
                StreamOptions::default(),
            )
            .await;
        let events = collect(&mut stream).await;
        assert!(
            matches!(events.last(), Some(AssistantMessageEvent::Error(_))),
            "{message}: {events:?}"
        );
        assert_eq!(
            hooks.scheduled.load(Ordering::SeqCst),
            0,
            "{message} 不应重试"
        );
    }
}
