//! 06 集成测试:分支、投影三算法、切点算法、序列化、run_compaction 全流程。

use rpi_agent::{AgentMessage, AssistantMessage, ContentBlock, CustomMessage, StopReason, Usage};
use rpi_session::compaction::SummarizationRequest;
use rpi_session::{
    build_context_entries, build_session_projection, create_fixed_summarizer, create_session,
    create_session_with, estimate_context_tokens, estimate_tokens, find_cut_point, run_compaction,
    serialize_conversation, should_compact, CompactionOutcome, CompactionSettings,
    ContextReplacement, Entry, SummarizationResponse, Summarizer, DEFAULT_COMPACTION_SETTINGS,
};

fn assistant(text: &str, total_tokens: u64, stop: StopReason) -> AgentMessage {
    let mut usage = Usage::zero();
    usage.total_tokens = total_tokens;
    AgentMessage::Assistant(Box::new(AssistantMessage {
        content: vec![ContentBlock::text(text)],
        api: "mock".into(),
        provider: "mock".into(),
        model: "m1".into(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        usage,
        stop_reason: stop,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }))
}

/// 带 tool call 的 assistant 消息。
fn tool_call_assistant(call_id: &str, name: &str, path: &str) -> AgentMessage {
    AgentMessage::Assistant(Box::new(AssistantMessage {
        content: vec![ContentBlock::ToolCall {
            id: call_id.into(),
            name: name.into(),
            arguments: serde_json::json!({"path": path}),
        }],
        api: "mock".into(),
        provider: "mock".into(),
        model: "m1".into(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        usage: Usage::zero(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }))
}

#[test]
fn branch_moves_leaf_and_projection_follows_path() {
    let session = create_session(None::<String>).unwrap();
    let a = session.append_message(AgentMessage::user("a")).unwrap();
    let b = session.append_message(AgentMessage::user("b")).unwrap();
    session.append_message(AgentMessage::user("c")).unwrap();

    // 分支回 b:后续追加成为 b 的孩子,c 仍在树中但不活动
    session.branch(&b).unwrap();
    let d = session.append_message(AgentMessage::user("d")).unwrap();
    assert_eq!(session.get_entry(&d).unwrap().parent_id(), Some(b.as_str()));

    let entries = session.entries();
    let path_ids: Vec<&str> = rpi_session::build_session_path(&entries, None)
        .iter()
        .map(|e| e.id())
        .collect();
    assert_eq!(path_ids, vec![a.as_str(), b.as_str(), d.as_str()]);

    // 树:b 有两个孩子(c 与 d)
    let tree = session.get_tree();
    let a_node = &tree[0];
    let b_node = a_node.children.iter().find(|n| n.entry.id() == b).unwrap();
    assert_eq!(b_node.children.len(), 2, "c 与 d 都是 b 的孩子");

    session.branch("missing").unwrap_err();
}

#[test]
fn context_edit_projection_replaces_removes_and_wraps() {
    let session = create_session(None::<String>).unwrap();
    let user_id = session
        .append_message(AgentMessage::user("secret number is 1"))
        .unwrap();
    let assistant_id = session
        .append_message(assistant("ok", 10, StopReason::Stop))
        .unwrap();
    session
        .append_message(AgentMessage::tool_result_text(
            "t1",
            "read",
            "result text",
            false,
        ))
        .unwrap();

    // 替换 user content
    session
        .append_context_edit(
            user_id.clone(),
            Some(ContextReplacement {
                content: "[redacted]".into(),
            }),
        )
        .unwrap();
    // 剔除 toolResult
    // (需要其 entry id;直接用 leaf 前一条——这里改为再取 entries 找)
    let entries = session.entries();
    let result_entry_id = entries
        .iter()
        .find(|e| {
            matches!(
                e,
                Entry::Message {
                    message: AgentMessage::ToolResult { .. },
                    ..
                }
            )
        })
        .map(Entry::id)
        .unwrap()
        .to_string();
    session.append_context_edit(result_entry_id, None).unwrap();

    let projection = session.projection();
    let user = projection
        .messages
        .iter()
        .find_map(|m| match m {
            AgentMessage::User { content, .. } => Some(content.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(user, "[redacted]");

    // assistant 被另一条 edit 替换 → content 变成单个 text 块
    session
        .append_context_edit(
            assistant_id,
            Some(ContextReplacement {
                content: "patched".into(),
            }),
        )
        .unwrap();
    let projection = session.projection();
    let patched = projection.messages.iter().find_map(|m| match m {
        AgentMessage::Assistant(a) => Some(a.content.clone()),
        _ => None,
    });
    assert_eq!(
        patched,
        Some(vec![ContentBlock::text("patched")]),
        "assistant 的字符串替换应包成 text 块"
    );

    // toolResult 已被剔除
    assert!(
        !projection
            .messages
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult { .. })),
        "replacement=null 应剔除消息"
    );
}

#[test]
fn compaction_virtual_expansion_and_multiple_compactions() {
    let session = create_session(None::<String>).unwrap();
    session.append_message(AgentMessage::user("q1")).unwrap();
    session
        .append_message(assistant("r1", 10, StopReason::Stop))
        .unwrap();
    let m2 = session.append_message(AgentMessage::user("q2")).unwrap();

    // 压缩:保留从 q2 起
    session
        .append_compaction("summary of q1", m2.clone(), 500, None, None, false)
        .unwrap();
    let compaction_id = session.get_leaf_id().unwrap();

    let context = session.context_entries();
    assert_eq!(context.len(), 2, "[compaction, q2]");
    assert!(matches!(context[0], Entry::Compaction { .. }));
    let ids: Vec<&str> = context.iter().map(Entry::id).collect();
    assert_eq!(ids, vec![compaction_id.as_str(), m2.as_str()]);

    // 摘要消息进上下文,且 systemMessage 缺省不注入
    let projection = session.projection();
    assert!(projection
        .messages
        .iter()
        .any(|m| matches!(m, AgentMessage::CompactionSummary { .. })));
    assert!(projection
        .messages
        .iter()
        .any(|m| matches!(m, AgentMessage::User { content, .. } if content == "q2")));

    // 第二次压缩:旧 compaction 的 id 落在新保留范围内时,只有 index==0 的
    // compaction 产生消息
    session
        .append_compaction("summary v2", m2.clone(), 800, None, None, false)
        .unwrap();
    let projection = session.projection();
    let compaction_summaries = projection
        .messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::CompactionSummary { .. }))
        .count();
    assert_eq!(compaction_summaries, 1, "只投影 index==0 的 compaction");
    // 上下文 = [compaction v2, q2, compaction v1]:旧 compaction 的 entry 保留在
    // 上下文里(其 id 落在最新保留范围)但不产生消息
    let context = session.context_entries();
    assert_eq!(context.len(), 3);
    assert!(matches!(context[0], Entry::Compaction { .. }));
    assert!(matches!(context[2], Entry::Compaction { .. }));
}

#[test]
fn settings_entries_project_to_thinking_and_model() {
    let session = create_session(None::<String>).unwrap();
    session.append_thinking_level_change("high").unwrap();
    session
        .append_model_change("anthropic", "claude-x")
        .unwrap();
    // usage entry 不进上下文
    session
        .append_usage("cache_warm", "anthropic", "claude-x", Usage::zero(), None)
        .unwrap();

    let projection = session.projection();
    assert_eq!(projection.thinking_level, "high");
    assert_eq!(
        projection.model,
        Some(rpi_session::ModelRef {
            provider: "anthropic".into(),
            model_id: "claude-x".into()
        })
    );
    assert_eq!(
        projection.messages.len(),
        0,
        "usage/设置态 entry 不进上下文"
    );

    // assistant 消息也更新 model 投影(pi 语义,后写生效)
    session
        .append_message(assistant("done", 5, StopReason::Stop))
        .unwrap();
    let projection = session.projection();
    assert_eq!(projection.model.as_ref().unwrap().model_id, "m1");
    assert_eq!(projection.messages.len(), 1);
    assert_eq!(session.session_name(), None);
    session.append_session_info(Some("my name".into())).unwrap();
    assert_eq!(session.session_name().as_deref(), Some("my name"));
}

#[test]
fn custom_message_enters_context_custom_entry_does_not() {
    let session = create_session(None::<String>).unwrap();
    session
        .append_custom("state", Some(serde_json::json!({"k": 1})))
        .unwrap();
    session
        .append_custom_message("note", "hello ext", None, true)
        .unwrap();

    let projection = session.projection();
    assert_eq!(projection.messages.len(), 1);
    match &projection.messages[0] {
        AgentMessage::Custom(CustomMessage { kind, data }) => {
            assert_eq!(kind, "note");
            assert_eq!(data["content"], "hello ext");
        }
        other => panic!("应投影为 Custom: {other:?}"),
    }
}

#[test]
fn cut_point_never_lands_on_tool_result() {
    // 构造:user(小)→ assistant(tool call)→ toolResult(大)…
    let session = create_session(None::<String>).unwrap();
    session
        .append_message(AgentMessage::user("turn 1"))
        .unwrap();
    session
        .append_message(assistant("working", 0, StopReason::ToolUse))
        .unwrap();
    let big: String = "x".repeat(40_000);
    session
        .append_message(AgentMessage::tool_result_text("t", "bash", &big, false))
        .unwrap();
    session
        .append_message(AgentMessage::user("turn 2"))
        .unwrap();
    let entries = session.entries();

    // keepRecentTokens 很小 → 切点不能落在 toolResult 上
    let cut = find_cut_point(&entries, 0, entries.len(), 100);
    assert!(!matches!(
        &entries[cut.first_kept_entry_index],
        Entry::Message {
            message: AgentMessage::ToolResult { .. },
            ..
        }
    ));
    // 累积到 budget 后取不早于当前位置的最近切点;turn 2 是切点 → 不是 split turn
    assert!(!cut.is_split_turn);
}

#[test]
fn cut_point_reports_split_turn_and_merges_metadata() {
    let session = create_session(None::<String>).unwrap();
    session
        .append_message(AgentMessage::user("big turn"))
        .unwrap();
    session
        .append_message(assistant(&"y".repeat(40_000), 0, StopReason::ToolUse))
        .unwrap();
    session
        .append_message(AgentMessage::tool_result_text(
            "t",
            "bash",
            "small result",
            false,
        ))
        .unwrap();
    session.append_thinking_level_change("high").unwrap(); // 相邻元数据 entry
    session.append_message(AgentMessage::user("next")).unwrap();
    let entries = session.entries();

    // budget 落在 assistant 大消息 → 切点在 assistant(split turn)
    let cut = find_cut_point(&entries, 0, entries.len(), 100);
    assert!(cut.is_split_turn);
    assert_eq!(cut.turn_start_index, Some(0));
    // assistant 是切点,其后的 toolResult 保留(firstKept < toolResult 下标)
    assert!(cut.first_kept_entry_index < 2);
}

#[test]
fn should_compact_boundary() {
    let settings = &DEFAULT_COMPACTION_SETTINGS;
    assert!(!should_compact(1000, 200_000, settings));
    assert!(should_compact(200_000 - 16_384 + 1, 200_000, settings));
    assert!(!should_compact(
        u64::MAX,
        u64::MAX,
        &CompactionSettings {
            enabled: false,
            ..Default::default()
        }
    ));
}

#[test]
fn reserve_tokens_percent_of_window() {
    use rpi_session::reserve_tokens_for_window;

    // 绝对值语义不受影响(>= 1.0)
    assert_eq!(reserve_tokens_for_window(16_384.0, 32_000), 16_384);
    // 百分比:0.1 × 32000 = 3200(四舍五入)
    assert_eq!(reserve_tokens_for_window(0.1, 32_000), 3_200);
    assert_eq!(reserve_tokens_for_window(0.125, 131_072), 16_384);
    // 非正数归零
    assert_eq!(reserve_tokens_for_window(0.0, 32_000), 0);
    assert_eq!(reserve_tokens_for_window(-0.5, 32_000), 0);

    // 触发边界:32k 窗口、10% 预留 → 已用 > 28800 触发
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 0.1,
        keep_recent_tokens: 20_000,
    };
    assert!(!should_compact(28_800, 32_000, &settings));
    assert!(should_compact(28_801, 32_000, &settings));

    // serde 兼容:整数写法(16384)与浮点写法(0.1)都能反序列化
    let parsed: CompactionSettings =
        serde_json::from_str(r#"{"enabled":true,"reserveTokens":16384,"keepRecentTokens":20000}"#)
            .unwrap();
    assert_eq!(parsed.reserve_tokens, 16_384.0);
    let parsed: CompactionSettings =
        serde_json::from_str(r#"{"enabled":true,"reserveTokens":0.1,"keepRecentTokens":20000}"#)
            .unwrap();
    assert_eq!(parsed.reserve_tokens, 0.1);
}

#[test]
fn token_estimation_prefers_usage_and_estimates_trailing() {
    let messages = vec![
        AgentMessage::user("12345678"), // 8 chars → 2 tokens 估算
        assistant("abcd", 1_000, StopReason::Stop),
        AgentMessage::user("12345678"), // trailing → 2
    ];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.usage_tokens, 1_000);
    assert_eq!(estimate.trailing_tokens, 2);
    assert_eq!(estimate.tokens, 1_002);
    assert_eq!(estimate.last_usage_index, Some(1));

    // 无 usage:全估算
    let messages = vec![AgentMessage::user("12345678")];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.tokens, 2);
    assert_eq!(estimate.last_usage_index, None);

    // error/aborted 或全零 usage 无效
    let messages = vec![
        assistant("x", 0, StopReason::Stop),
        assistant("y", 500, StopReason::Aborted),
    ];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.last_usage_index, None);
}

#[test]
fn estimate_tokens_covers_all_roles() {
    let mut usage = Usage::zero();
    usage.total_tokens = 0;
    assert_eq!(estimate_tokens(&AgentMessage::user("12345678")), 2);
    assert_eq!(
        estimate_tokens(&AgentMessage::CompactionSummary {
            summary: "12345678".into(),
            timestamp: 0
        }),
        2
    );
    assert_eq!(
        estimate_tokens(&AgentMessage::BashExecution {
            command: "ls".into(),
            output: "123456".into(),
            exit_code: None,
            timestamp: 0
        }),
        2 // (2 + 6) / 4 = 2
    );
    // image 按 4800 字符估算
    let image_message = AgentMessage::ToolResult {
        tool_call_id: "t".into(),
        tool_name: "read".into(),
        content: vec![ContentBlock::Image {
            data: "abc".into(),
            mime_type: "image/png".into(),
        }],
        details: None,
        usage: Some(usage),
        is_error: false,
        timestamp: 0,
    };
    assert_eq!(estimate_tokens(&image_message), 1200);
}

#[test]
fn serialize_conversation_formats_like_pi() {
    let messages = vec![
        AgentMessage::user("hello"),
        AgentMessage::Assistant(Box::new(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    thinking_signature: None,
                    redacted: None,
                },
                ContentBlock::text("world"),
                ContentBlock::ToolCall {
                    id: "t".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a.txt"}),
                },
            ],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            usage: Usage::zero(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        })),
        AgentMessage::tool_result_text("t", "read", "r".repeat(3000).as_str(), false),
    ];
    let serialized = serialize_conversation(&messages);
    assert!(serialized.contains("[User]: hello"));
    assert!(serialized.contains("[Assistant thinking]: hmm"));
    assert!(serialized.contains("[Assistant]: world"));
    assert!(serialized.contains("[Assistant tool calls]: read(path=\"a.txt\")"));
    assert!(serialized.contains("[Tool result]: "), "{serialized}");
    assert!(
        serialized.contains("more characters truncated"),
        "tool result 截 2000 字符"
    );
}

struct CapturingSummarizer {
    summary: String,
    stop_reason: StopReason,
    captured: std::sync::Mutex<Vec<SummarizationRequest>>,
}

#[async_trait::async_trait]
impl Summarizer for CapturingSummarizer {
    async fn summarize(
        &self,
        request: &SummarizationRequest,
    ) -> Result<SummarizationResponse, String> {
        self.captured.lock().unwrap().push(SummarizationRequest {
            messages: request.messages.clone(),
            instruction: request.instruction.clone(),
        });
        Ok(SummarizationResponse {
            summary: self.summary.clone(),
            stop_reason: self.stop_reason,
            error_message: None,
            usage: None,
        })
    }
}

#[tokio::test]
async fn run_compaction_end_to_end() {
    let session = create_session(None::<String>).unwrap();
    session
        .append_message(assistant("system-ish", 0, StopReason::Stop))
        .unwrap();
    session
        .append_message(AgentMessage::user("please read /tmp/a.txt"))
        .unwrap();
    // 带 tool call 的 assistant(read /tmp/a.txt)
    session
        .append_message(tool_call_assistant("t1", "read", "/tmp/a.txt"))
        .unwrap();
    session
        .append_message(AgentMessage::tool_result_text(
            "t1", "read", "contents", false,
        ))
        .unwrap();
    session
        .append_message(assistant("all done", 0, StopReason::Stop))
        .unwrap();
    let entries = session.entries();

    let summarizer = CapturingSummarizer {
        summary: "## Goal\ndo things".into(),
        stop_reason: StopReason::Stop,
        captured: std::sync::Mutex::new(Vec::new()),
    };
    let outcome: Option<CompactionOutcome> = run_compaction(
        &entries,
        &CompactionSettings {
            keep_recent_tokens: 1,
            ..Default::default()
        },
        &summarizer,
    )
    .await
    .unwrap();
    let outcome = outcome.expect("应产出压缩结果");

    // firstKept 指向切点 entry;tokensBefore > 0;details 有文件清单
    let details = outcome.details.as_object().unwrap();
    assert_eq!(details["readFiles"], serde_json::json!(["/tmp/a.txt"]));
    assert!(outcome.tokens_before > 0);

    // 摘要请求带固定模板;文件清单以 XML 追加进 summary
    let captured = summarizer.captured.lock().unwrap();
    assert!(captured[0]
        .instruction
        .starts_with("The messages above are a conversation to summarize"));
    assert!(outcome.summary.contains("<read-files>"));
    assert!(outcome.summary.contains("## Goal"));
}

#[tokio::test]
async fn run_compaction_skips_empty_conversation_range() {
    // 回归:短会话切点落在首个 user 消息,待摘要范围只有元数据 entry
    // (如 tool_set_change)→ 不应发出空对话的摘要请求
    let session = create_session(None::<String>).unwrap();
    session
        .append_tool_set_change(&["read".to_string()])
        .unwrap();
    session.append_message(AgentMessage::user("第一问")).unwrap();
    session
        .append_message(assistant("第一轮的回复", 17, StopReason::Stop))
        .unwrap();
    let entries = session.entries();

    struct NoCall;
    #[async_trait::async_trait]
    impl Summarizer for NoCall {
        async fn summarize(
            &self,
            request: &SummarizationRequest,
        ) -> Result<SummarizationResponse, String> {
            panic!("不应发出摘要请求,对话内容: {:?}", request.messages);
        }
    }
    let outcome = run_compaction(&entries, &CompactionSettings::default(), &NoCall)
        .await
        .unwrap();
    assert!(outcome.is_none(), "空对话范围不应产出压缩结果");
}

#[tokio::test]
async fn run_compaction_uses_update_template_when_previous_summary_exists() {
    let session = create_session(None::<String>).unwrap();
    session
        .append_message(AgentMessage::user("old question"))
        .unwrap();
    session
        .append_message(assistant("old answer", 0, StopReason::Stop))
        .unwrap();
    let old_leaf = session.get_leaf_id().unwrap();
    session
        .append_compaction(
            "previous summary",
            old_leaf.clone(),
            100,
            None,
            None,
            false,
        )
        .unwrap();
    session
        .append_message(AgentMessage::user("new question"))
        .unwrap();
    session
        .append_message(assistant("new answer", 0, StopReason::Stop))
        .unwrap();
    let entries = session.entries();

    let summarizer = CapturingSummarizer {
        summary: "updated".into(),
        stop_reason: StopReason::Stop,
        captured: std::sync::Mutex::new(Vec::new()),
    };
    let outcome = run_compaction(
        &entries,
        &CompactionSettings {
            // 切点落在最后一条 assistant:新 user 消息进摘要范围,旧消息不进
            keep_recent_tokens: 1,
            ..Default::default()
        },
        &summarizer,
    )
    .await
    .unwrap()
    .expect("应产出压缩结果");

    let captured = summarizer.captured.lock().unwrap();
    assert!(
        captured[0]
            .instruction
            .starts_with("The messages above are NEW conversation messages"),
        "增量场景应用 update 模板: {}",
        &captured[0].instruction[..80]
    );
    // 旧摘要进入对话文本的 <previous-summary> 标签(请求消息里的 user 承载对话)
    let conversation = captured[0]
        .messages
        .iter()
        .find_map(|m| match m {
            AgentMessage::User { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("user 承载序列化对话");
    assert!(conversation.contains("<previous-summary>\nprevious summary\n</previous-summary>"));
    let _ = outcome;
    // 增量只发新消息:旧摘要成为历史唯一来源,原始旧消息不再重发
    assert!(conversation.contains("new question"), "新消息应进入序列化对话");
    assert!(
        !conversation.contains("old question") && !conversation.contains("old answer"),
        "旧消息不应随增量请求重发: {conversation}"
    );
}

#[tokio::test]
async fn run_compaction_incremental_merges_file_lists_from_previous_summary() {
    let session = create_session(None::<String>).unwrap();
    session
        .append_message(AgentMessage::user("read old file"))
        .unwrap();
    session
        .append_message(tool_call_assistant("t0", "read", "/tmp/old-read.txt"))
        .unwrap();
    session
        .append_message(AgentMessage::tool_result_text(
            "t0", "read", "contents", false,
        ))
        .unwrap();
    session
        .append_message(assistant("old done", 0, StopReason::Stop))
        .unwrap();
    let old_leaf = session.get_leaf_id().unwrap();
    // 旧摘要自带上一轮累积的文件清单(与 format_file_operations 输出同构)
    session
        .append_compaction(
            "## Goal\nold work\n\n<read-files>\n/tmp/old-read.txt\n</read-files>\n\n<modified-files>\n/tmp/old-edit.txt\n</modified-files>",
            old_leaf,
            100,
            None,
            None,
            false,
        )
        .unwrap();
    // 新消息把旧摘要里 read 过的文件改掉,又 read 了一个新文件
    session
        .append_message(AgentMessage::user("now edit"))
        .unwrap();
    session
        .append_message(tool_call_assistant("t1", "edit", "/tmp/old-read.txt"))
        .unwrap();
    session
        .append_message(AgentMessage::tool_result_text("t1", "edit", "ok", false))
        .unwrap();
    session
        .append_message(tool_call_assistant("t2", "read", "/tmp/new-read.txt"))
        .unwrap();
    session
        .append_message(AgentMessage::tool_result_text(
            "t2", "read", "new contents", false,
        ))
        .unwrap();
    session
        .append_message(assistant("new done", 0, StopReason::Stop))
        .unwrap();
    let entries = session.entries();

    let summarizer = CapturingSummarizer {
        summary: "updated".into(),
        stop_reason: StopReason::Stop,
        captured: std::sync::Mutex::new(Vec::new()),
    };
    let outcome = run_compaction(
        &entries,
        &CompactionSettings {
            keep_recent_tokens: 1,
            ..Default::default()
        },
        &summarizer,
    )
    .await
    .unwrap()
    .expect("应产出压缩结果");

    let details = outcome.details.as_object().unwrap();
    // read = 新 read ∪ 旧 read − modified:old-read 被改掉后移出 read
    assert_eq!(
        details["readFiles"],
        serde_json::json!(["/tmp/new-read.txt"])
    );
    // modified = 新 modified ∪ 旧 modified
    assert_eq!(
        details["modifiedFiles"],
        serde_json::json!(["/tmp/old-edit.txt", "/tmp/old-read.txt"])
    );
    // 清单以 XML 节追加进摘要文本,供下次增量继续解析
    assert!(outcome.summary.contains("<read-files>\n/tmp/new-read.txt\n</read-files>"));
    assert!(outcome.summary.contains("/tmp/old-edit.txt"));
}

#[tokio::test]
async fn run_compaction_returns_none_when_nothing_new_since_last_compaction() {
    let session = create_session(None::<String>).unwrap();
    session
        .append_message(AgentMessage::user("old question"))
        .unwrap();
    session
        .append_message(assistant("old answer", 0, StopReason::Stop))
        .unwrap();
    let old_leaf = session.get_leaf_id().unwrap();
    session
        .append_compaction("previous summary", old_leaf, 100, None, None, false)
        .unwrap();
    // keep_recent_tokens=1 时切点落在最新 user 消息:上次压缩后无新消息进入摘要范围
    session
        .append_message(AgentMessage::user("new question"))
        .unwrap();
    let entries = session.entries();

    let summarizer = CapturingSummarizer {
        summary: "should not be used".into(),
        stop_reason: StopReason::Stop,
        captured: std::sync::Mutex::new(Vec::new()),
    };
    let outcome = run_compaction(
        &entries,
        &CompactionSettings {
            keep_recent_tokens: 1,
            ..Default::default()
        },
        &summarizer,
    )
    .await
    .unwrap();
    assert!(outcome.is_none(), "无新内容不应产出压缩结果");
    assert!(
        summarizer.captured.lock().unwrap().is_empty(),
        "不应发出摘要请求"
    );
}

#[tokio::test]
async fn run_compaction_rejects_length_and_error_summaries() {
    let session = create_session(None::<String>).unwrap();
    session
        .append_message(AgentMessage::user("q".repeat(4000)))
        .unwrap();
    session
        .append_message(assistant("a".repeat(4000).as_str(), 0, StopReason::Stop))
        .unwrap();
    let entries = session.entries();

    let summarizer = create_fixed_summarizer("s");
    let outcome = run_compaction(
        &entries,
        &CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        },
        &*summarizer,
    )
    .await
    .unwrap();
    assert!(outcome.is_some(), "内容足够长时应产出切点与压缩结果");

    // length 终态的摘要不可入库
    struct LengthSummarizer;
    #[async_trait::async_trait]
    impl Summarizer for LengthSummarizer {
        async fn summarize(
            &self,
            _request: &SummarizationRequest,
        ) -> Result<SummarizationResponse, String> {
            Ok(SummarizationResponse {
                summary: "partial".into(),
                stop_reason: StopReason::Length,
                error_message: None,
                usage: None,
            })
        }
    }
    let err = run_compaction(
        &entries,
        &CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        },
        &LengthSummarizer,
    )
    .await
    .unwrap_err();
    assert!(err.contains("token cap"), "{err}");

    struct ErrorSummarizer;
    #[async_trait::async_trait]
    impl Summarizer for ErrorSummarizer {
        async fn summarize(
            &self,
            _request: &SummarizationRequest,
        ) -> Result<SummarizationResponse, String> {
            Ok(SummarizationResponse {
                summary: String::new(),
                stop_reason: StopReason::Error,
                error_message: Some("boom".into()),
                usage: None,
            })
        }
    }
    let err = run_compaction(
        &entries,
        &CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        },
        &ErrorSummarizer,
    )
    .await
    .unwrap_err();
    assert!(err.contains("boom"), "{err}");
}

#[test]
fn serialize_includes_folded_custom_messages() {
    let messages = vec![
        AgentMessage::BranchSummary {
            summary: "branch so far".into(),
            timestamp: 0,
        },
        AgentMessage::BashExecution {
            command: "ls".into(),
            output: "a.txt".into(),
            exit_code: Some(0),
            timestamp: 0,
        },
        AgentMessage::CompactionSummary {
            summary: "checkpoint".into(),
            timestamp: 0,
        },
    ];
    let serialized = serialize_conversation(&messages);
    assert!(serialized.contains("[User]: branch so far"));
    assert!(serialized.contains("Ran `ls`"));
    assert!(serialized.contains("[User]: checkpoint"), "{serialized}");
}

#[test]
fn estimate_trusts_usage_when_no_edit_or_compaction() {
    // 无 context_edit/compaction:usage 恒可信(pi 语义,latestInvalidating = -1)
    let session = create_session(None::<String>).unwrap();
    session.append_message(AgentMessage::user("q")).unwrap();
    session
        .append_message(assistant("a", 7_000, StopReason::Stop))
        .unwrap();
    let entries = session.entries();
    let projection = rpi_session::build_session_projection(&entries, None);
    let estimate = rpi_session::estimate_projected_context_tokens(&projection, &entries);
    assert_eq!(
        estimate.usage_tokens, 7_000,
        "应使用真实 usage 而非全量估算"
    );

    // compaction 之后的旧 usage 失真 → 全量估算
    let leaf = session.get_leaf_id().unwrap();
    session
        .append_compaction("s", leaf, 100, None, None, false)
        .unwrap();
    session.append_message(AgentMessage::user("q2")).unwrap();
    let entries = session.entries();
    let projection = rpi_session::build_session_projection(&entries, None);
    let estimate = rpi_session::estimate_projected_context_tokens(&projection, &entries);
    assert_eq!(estimate.usage_tokens, 0, "compaction 后旧 usage 不可信");
}

#[test]
fn tree_resolves_labels_and_damaged_parent_chain() {
    let session = create_session(None::<String>).unwrap();
    let a = session.append_message(AgentMessage::user("a")).unwrap();
    session.append_label(&a, Some("start".into())).unwrap();
    let tree = session.get_tree();
    assert_eq!(tree[0].label.as_deref(), Some("start"));
    assert!(tree[0].label_timestamp.is_some());

    // 损坏 parent 链:path 截断不 panic;未知 leaf 回退到最后一条
    let entries = session.entries();
    let path = rpi_session::build_session_path(&entries, Some("missing-id"));
    assert_eq!(path.len(), 2, "未知 leaf 回退到末尾 entry 的完整路径");
}

#[test]
fn empty_entries_projection_is_empty() {
    let session = create_session(None::<String>).unwrap();
    let projection = build_session_projection(&session.entries(), None);
    assert!(projection.messages.is_empty());
    assert_eq!(projection.thinking_level, "off");
    assert!(projection.model.is_none());
    assert!(build_context_entries(&session.entries(), None).is_empty());
}

#[test]
fn crash_half_line_is_isolated_and_next_append_survives() {
    let dir = std::env::temp_dir().join(format!("rpi_session_crash_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("crash.jsonl");

    // 正常创建会话并写一条消息
    {
        let session = create_session_with(Some(&path), "/tmp/proj", None).unwrap();
        session
            .append_message(AgentMessage::user("before crash"))
            .unwrap();
    }
    // 模拟写入中途崩溃:留下末尾无换行的半行
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"mess").unwrap();
    }

    // 重新加载:半行按损坏行跳过;恢复后的第一条 append 不能被半行吞掉
    let session = create_session_with(Some(&path), "/tmp/proj", None).unwrap();
    assert_eq!(session.corrupt_lines(), 1, "半行应计为损坏行");
    session
        .append_message(AgentMessage::user("after crash"))
        .unwrap();

    let reloaded = create_session_with(Some(&path), "/tmp/proj", None).unwrap();
    let entries = reloaded.entries();
    assert!(matches!(
        entries.last().unwrap(),
        Entry::Message {
            message: AgentMessage::User { content, .. },
            ..
        } if content == "after crash"
    ));

    // 不存在半行与新 entry 拼接成的行
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.lines().any(|l| l.contains("\"type\":\"mess{\"")),
        "半行后应补换行,不允许拼接: {raw}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_session_files_sorts_and_previews() {
    let dir = std::env::temp_dir().join(format!("rpi_session_list_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 旧会话:两条消息,预览取首条 user 消息首行
    let old_path = dir.join("old.jsonl");
    {
        let session = create_session_with(Some(&old_path), "/tmp/proj", None).unwrap();
        session
            .append_message(AgentMessage::user("first question\nsecond line"))
            .unwrap();
        session.append_message(AgentMessage::user("later")).unwrap();
    }
    // 新会话:仅落 header,无消息
    let new_path = dir.join("new.jsonl");
    {
        let _session = create_session_with(Some(&new_path), "/tmp/proj", None).unwrap();
    }
    // 保证 mtime 有可见差(同一秒内 old/new 顺序不确定)
    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(5);
    let file = std::fs::File::options().append(true).open(&old_path).unwrap();
    file.set_modified(old_time).unwrap();

    let list = rpi_session::list_session_files(&dir, None);
    assert_eq!(list.len(), 2, "应列出全部主会话文件");
    assert_eq!(list[0].path, new_path, "mtime 新者在前");
    assert_eq!(list[1].path, old_path);
    assert_eq!(list[1].preview, "first question", "预览取首条 user 首行");
    assert_eq!(list[0].preview, "(空会话)", "无消息会话用占位文案");
    assert!(!list[0].session_id.is_empty());

    // tag 子会话文件(<时间>__<tag>__<id>.jsonl)不参与列表
    let tag_path = dir.join("20260101-000000__child__abcdef01.jsonl");
    std::fs::write(&tag_path, "{}\n").unwrap();
    let list = rpi_session::list_session_files(&dir, None);
    assert_eq!(list.len(), 2, "tag 子会话应被排除");

    // cwd 过滤:header.cwd 不匹配的会话不进入
    let list = rpi_session::list_session_files(&dir, Some("/tmp/other"));
    assert!(list.is_empty(), "cwd 不匹配应过滤全部");

    // find_latest 与列表共享同一候选集
    assert_eq!(
        rpi_session::find_latest_session_file(&dir, Some("/tmp/proj")),
        Some(new_path)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_session_files_preview_is_truncated() {
    let dir = std::env::temp_dir().join(format!("rpi_session_trunc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("long.jsonl");
    let session = create_session_with(Some(&path), "/tmp/proj", None).unwrap();
    let long_text = "啊".repeat(200);
    session.append_message(AgentMessage::user(long_text)).unwrap();

    let list = rpi_session::list_session_files(&dir, None);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].preview.chars().count(), 80, "预览截 80 字符");
    assert!(list[0].preview.ends_with('…') || list[0].preview.chars().count() < 200);

    let _ = std::fs::remove_dir_all(&dir);
}
