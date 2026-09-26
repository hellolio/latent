//! I 系列不变量 property test(03 文档 §10.1/§10.7 M2+,11 计划 T6)。
//!
//! 每条不变量至少一个独立用例并注明 I 编号:
//! - **I6**:重放 system 消息的工具声明后,模型可见集恒等于 `context.tools`;
//! - **配对**:正常 / abort / length 截断三条路径下 toolCall 与 toolResult 一一配对;
//! - **I4**:并行时事件完成序 vs 消息源序双保序;
//! - **I5**:length 截断消息的全部 tool call 被拒执行(含防振荡计数)。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use proptest::prelude::*;
use rpi_agent::{
    run_agent_loop, AgentEvent, AgentMessage, LoopConfig, PassthroughHooks, SharedSubscriber,
    Subscriber, Tool, ToolCall, ToolError, ToolOutput, ToolUpdater,
};
use rpi_ai::{ContentBlock, Model, ScriptedProvider, ScriptedTurn};
use tokio_util::sync::CancellationToken;

fn model() -> Model {
    Model::minimal("mock-1", "mock", "mock")
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}

/// 记录 tool_execution_end 顺序与执行计数的订阅者。
#[derive(Default)]
struct EventLog {
    tool_end: Mutex<Vec<String>>,
    message_ends: Mutex<Vec<AgentMessage>>,
}

#[async_trait]
impl Subscriber for EventLog {
    async fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                self.tool_end.lock().unwrap().push(tool_call_id.clone());
            }
            AgentEvent::MessageEnd { message } => {
                self.message_ends.lock().unwrap().push((**message).clone());
            }
            _ => {}
        }
    }
}

/// 计数执行次数的工具;延迟按名字后缀分档(I4 完成序用)。
struct ProbeTool {
    name: String,
    delay_ms: u64,
    calls: AtomicU32,
}

impl ProbeTool {
    fn new(name: &str, delay_ms: u64) -> Arc<Self> {
        Arc::new(ProbeTool {
            name: name.to_string(),
            delay_ms,
            calls: AtomicU32::new(0),
        })
    }
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _call: ToolCall,
        _cancel: CancellationToken,
        _updater: &dyn ToolUpdater,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        Ok(ToolOutput::text(format!("{} done", self.name)))
    }
}

fn tool_call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: serde_json::json!({}),
    }
}

// ---------------------------------------------------------------------------
// I6:声明重放后模型可见集恒等于 context.tools
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn i6_replay_after_declaration_equals_context_tools(
        pool in prop::collection::vec("[a-z][a-z0-9_]{0,6}", 1..7),
        ops in prop::collection::vec((0usize..7, any::<bool>()), 0..10),
        desired_bits in prop::collection::vec(any::<bool>(), 7),
    ) {
        let pool: Vec<String> = pool.into_iter().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        // 转录:op 序列编码为 system 消息的 toolsAdded/toolsRemoved
        let mut transcript: Vec<AgentMessage> = Vec::new();
        for (index, add) in ops {
            let Some(name) = pool.get(index % pool.len()) else { continue };
            let (added, removed) = if add {
                (vec![rpi_ai::Tool::new(name.clone(), "", serde_json::json!({"type": "object"}))], vec![])
            } else {
                (vec![], vec![rpi_ai::ToolReference { name: name.clone() }])
            };
            transcript.push(AgentMessage::System {
                content: String::new(),
                sections: Default::default(),
                tools_added: added,
                tools_removed: removed,
                timestamp: 0,
            });
        }
        // 期望可见集:pool 的一个子集
        let desired: Vec<String> = pool
            .iter()
            .enumerate()
            .filter(|(i, _)| desired_bits.get(*i).copied().unwrap_or(false))
            .map(|(_, name)| name.clone())
            .collect();
        let tools: Vec<Arc<dyn Tool>> = desired
            .iter()
            .map(|name| ProbeTool::new(name, 0) as Arc<dyn Tool>)
            .collect();

        let pending = vec![AgentMessage::user("hi")];
        let injected = rpi_agent::declare_tool_changes(&transcript, &tools, pending);

        // I6:transcript + 注入结果重放 == context.tools(按名字集合与声明一致性)
        let mut full = transcript.clone();
        full.extend(injected.iter().cloned());
        let replay = rpi_agent::declared_tools(&full);
        let mut replay_names: Vec<String> = replay.iter().map(|t| t.name.clone()).collect();
        replay_names.sort();
        let mut desired_names = desired.clone();
        desired_names.sort();
        prop_assert_eq!(replay_names, desired_names);

        // 幂等:再次声明无增量(注入后可见集已收敛,重复注入必须原样返回)
        let again = rpi_agent::declare_tool_changes(&full, &tools, vec![AgentMessage::user("x")]);
        prop_assert_eq!(again.len(), 1);
        let is_user = matches!(&again[0], AgentMessage::User { .. });
        prop_assert!(is_user);
    }
}

// ---------------------------------------------------------------------------
// 配对不变量:正常路径,随机 turn 序列下 toolCall 与 toolResult 一一配对
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum TurnPlan {
    Text,
    Calls(usize),
}

fn turn_plan_strategy() -> impl Strategy<Value = TurnPlan> {
    prop_oneof![Just(TurnPlan::Text), (1usize..=3).prop_map(TurnPlan::Calls)]
}

fn build_script(
    m: &Model,
    plans: &[TurnPlan],
    missing_every: usize,
) -> (Vec<ScriptedTurn>, Vec<ContentBlock>) {
    let mut turns = Vec::new();
    let mut all_calls = Vec::new();
    for (turn_index, plan) in plans.iter().enumerate() {
        match plan {
            TurnPlan::Text => turns.push(ScriptedTurn::text(m, format!("t{turn_index}"))),
            TurnPlan::Calls(n) => {
                let mut calls = Vec::new();
                for i in 0..*n {
                    // 每 missing_every 个调用里有一个指向不存在的工具(错误路径)
                    let (id, name) =
                        if missing_every > 0 && (all_calls.len() + i) % missing_every == 0 {
                            (format!("c{}-{}", turn_index, i), "missing".to_string())
                        } else {
                            (format!("c{}-{}", turn_index, i), format!("tool{}", i % 3))
                        };
                    calls.push(tool_call(&id, &name));
                }
                all_calls.extend(calls.clone());
                turns.push(ScriptedTurn::tool_calls(m, calls));
            }
        }
    }
    (turns, all_calls)
}

fn collect_pairing(messages: &[AgentMessage]) -> (Vec<String>, Vec<String>) {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for message in messages {
        match message {
            AgentMessage::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall { id, .. } = block {
                        calls.push(id.clone());
                    }
                }
            }
            AgentMessage::ToolResult { tool_call_id, .. } => results.push(tool_call_id.clone()),
            _ => {}
        }
    }
    (calls, results)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn pairing_normal_path_every_call_has_exactly_one_result(
        plans in prop::collection::vec(turn_plan_strategy(), 1..5),
        missing_every in prop::option::of(2usize..=4),
    ) {
        block_on(async move {
            let m = model();
            let mut plans = plans;
            plans.push(TurnPlan::Text); // 收尾:无 pending 时循环必须能停
            let (turns, _) = build_script(&m, &plans, missing_every.unwrap_or(0));
            let provider = ScriptedProvider::new(&m, turns);
            let tools: Vec<Arc<dyn Tool>> =
                (0..3).map(|i| ProbeTool::new(&format!("tool{i}"), 0) as Arc<dyn Tool>).collect();
            let log = Arc::new(EventLog::default());
            let output = run_agent_loop(
                vec![AgentMessage::user("hi")],
                rpi_agent::AgentContext { system: None, messages: Vec::new(), tools },
                Arc::new(PassthroughHooks),
                LoopConfig::new(m),
                Arc::new(provider),
                log.clone() as SharedSubscriber,
                CancellationToken::new(),
                rpi_agent::create_injection_endpoints().1,
            )
            .await
            .0;
            let (calls, results) = collect_pairing(&output.messages);
            prop_assert_eq!(calls.len(), results.len(), "toolCall 与 toolResult 数量必须相等");
            let mut sorted_calls = calls.clone();
            let mut sorted_results = results.clone();
            sorted_calls.sort();
            sorted_results.sort();
            prop_assert_eq!(sorted_calls, sorted_results, "每个 toolCall 恰好一个同 id 的 toolResult");
            // 每个调用恰好一次 tool_execution_end
            prop_assert_eq!(log.tool_end.lock().unwrap().len(), calls.len());
            Ok(())
        })
        .unwrap_or_else(|error| panic!("property failed: {error:?}"));
    }
}

// ---------------------------------------------------------------------------
// I4:并行双保序 —— end 事件按完成序(延迟递增),result 消息按源序
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn i4_parallel_batch_is_double_ordered(batch in 2usize..=4, seed_order in prop::collection::vec(any::<bool>(), 4)) {
        block_on(async move {
            let m = model();
            // 每个调用的工具延迟随槽位递增(3ms 步长),完成序可预测
            let mut calls = Vec::new();
            let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
            for i in 0..batch {
                // 源序随机打乱工具延迟分配,避免"源序 == 完成序"的平凡通过
                let slot = if seed_order.get(i).copied().unwrap_or(false) { batch - 1 - i } else { i };
                let name = format!("tool{slot}");
                // 延迟含 call 下标项,保证互异(重复 slot 时完成序仍可预测)
                tools.push(ProbeTool::new(&name, ((slot + 1) * 4 + i) as u64));
                calls.push(tool_call(&format!("t{i}"), &name));
            }
            let provider = ScriptedProvider::new(
                &m,
                vec![ScriptedTurn::tool_calls(&m, calls.clone()), ScriptedTurn::text(&m, "done")],
            );
            let log = Arc::new(EventLog::default());
            let output = run_agent_loop(
                vec![AgentMessage::user("hi")],
                rpi_agent::AgentContext { system: None, messages: Vec::new(), tools },
                Arc::new(PassthroughHooks),
                LoopConfig::new(m),
                Arc::new(provider),
                log.clone() as SharedSubscriber,
                CancellationToken::new(),
                rpi_agent::create_injection_endpoints().1,
            )
            .await
            .0;

            // 消息源序:结果消息顺序 == toolCall 声明顺序
            let results: Vec<String> = output
                .messages
                .iter()
                .filter_map(|msg| match msg {
                    AgentMessage::ToolResult { tool_name, .. } => Some(tool_name.clone()),
                    _ => None,
                })
                .collect();
            let source_order: Vec<String> = calls
                .iter()
                .filter_map(|c| match c {
                    ContentBlock::ToolCall { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .collect();
            prop_assert_eq!(results, source_order, "tool result 消息必须按源序");

            // 完成序:end 事件按延迟递增序出现,每个 id 恰好一次
            let end = log.tool_end.lock().unwrap().clone();
            prop_assert_eq!(end.len(), batch);
            let expected_by_delay: Vec<String> = {
                let mut ids_with_delay: Vec<(String, u64)> = calls
                    .iter()
                    .enumerate()
                    .map(|(i, c)| match c {
                        ContentBlock::ToolCall { name, .. } => {
                            let slot = name.trim_start_matches("tool").parse::<u64>().unwrap_or(0);
                            (format!("t{i}"), (slot + 1) * 4 + i as u64)
                        }
                        _ => unreachable!(),
                    })
                    .collect();
                ids_with_delay.sort_by_key(|(_, delay)| *delay);
                ids_with_delay.into_iter().map(|(id, _)| id).collect()
            };
            prop_assert_eq!(end, expected_by_delay, "tool_execution_end 必须按完成序");
            Ok(())
        })
        .unwrap_or_else(|error| panic!("property failed: {error:?}"));
    }
}

// ---------------------------------------------------------------------------
// 配对不变量:abort 路径(串行批中途中止,剩余调用以 Cancelled 收尾)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn pairing_abort_path_every_call_still_gets_result(batch in 2usize..=4) {
        block_on(async move {
            let m = model();
            struct CancelAfterFirstHooks(CancellationToken);
            #[async_trait]
            impl rpi_agent::LoopHooks for CancelAfterFirstHooks {
                fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<rpi_ai::Message> {
                    rpi_agent::PassthroughHooks.convert_to_llm(msgs)
                }
                async fn after_tool_call(
                    &self,
                    _ctx: rpi_agent::ToolResultCtx,
                ) -> Option<rpi_agent::ToolPatch> {
                    self.0.cancel();
                    None
                }
                fn tool_execution(&self) -> rpi_agent::ToolExecution {
                    rpi_agent::ToolExecution::Serial
                }
            }
            let cancel = CancellationToken::new();
            let loop_cancel = cancel.clone();
            let calls: Vec<ContentBlock> = (0..batch)
                .map(|i| tool_call(&format!("t{i}"), "probe"))
                .collect();
            let provider = ScriptedProvider::new(
                &m,
                vec![
                    ScriptedTurn::tool_calls(&m, calls.clone()),
                    ScriptedTurn::error(&m, "不应再请求"),
                ],
            );
            let tools: Vec<Arc<dyn Tool>> = vec![ProbeTool::new("probe", 0)];
            let log = Arc::new(EventLog::default());
            let output = run_agent_loop(
                vec![AgentMessage::user("hi")],
                rpi_agent::AgentContext { system: None, messages: Vec::new(), tools },
                Arc::new(CancelAfterFirstHooks(cancel)),
                LoopConfig::new(m),
                Arc::new(provider),
                log.clone() as SharedSubscriber,
                loop_cancel,
                rpi_agent::create_injection_endpoints().1,
            )
            .await
            .0;

            // abort 后 run 终止,但每个 toolCall 恰好一个 toolResult(配对不变量)
            prop_assert_eq!(output.stop, rpi_agent::RunStop::Aborted);
            let (calls_seen, results) = collect_pairing(&output.messages);
            prop_assert_eq!(calls_seen.len(), batch);
            prop_assert_eq!(results.len(), batch);
            let mut sorted_calls = calls_seen.clone();
            let mut sorted_results = results.clone();
            sorted_calls.sort();
            sorted_results.sort();
            prop_assert_eq!(sorted_calls, sorted_results);
            Ok(())
        })
        .unwrap_or_else(|error| panic!("property failed: {error:?}"));
    }
}

// ---------------------------------------------------------------------------
// I5:length 截断防御 —— 全部 tool call 拒执行 + 防振荡计数兜底
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn i5_truncated_calls_all_rejected_with_anti_oscillation(
        truncated_turns in 1usize..=6,
        calls_per_turn in 1usize..=3,
    ) {
        block_on(async move {
            let m = model();
            let tool = ProbeTool::new("read", 0);
            let mut turns = Vec::new();
            for i in 0..truncated_turns {
                let calls = (0..calls_per_turn)
                    .map(|j| tool_call(&format!("c{i}-{j}"), "read"))
                    .collect();
                turns.push(ScriptedTurn::truncated(&m, calls));
            }
            turns.push(ScriptedTurn::text(&m, "recovered"));
            let provider = ScriptedProvider::new(&m, turns);
            let log = Arc::new(EventLog::default());
            let output = run_agent_loop(
                vec![AgentMessage::user("hi")],
                rpi_agent::AgentContext { system: None, messages: Vec::new(), tools: vec![tool.clone() as Arc<dyn Tool>] },
                Arc::new(PassthroughHooks),
                LoopConfig::new(m),
                Arc::new(provider),
                log.clone() as SharedSubscriber,
                CancellationToken::new(),
                rpi_agent::create_injection_endpoints().1,
            )
            .await
            .0;

            // I5:截断轮的全部 tool call 不执行
            prop_assert_eq!(tool.calls.load(Ordering::SeqCst), 0, "截断消息的 tool call 必须全部拒执行");

            // 每个被拒调用都有错误 result(配对),文案提示重发
            let rejected: Vec<&AgentMessage> = output
                .messages
                .iter()
                .filter(|msg| matches!(msg, AgentMessage::ToolResult { is_error: true, .. }))
                .collect();
            let expected = truncated_turns.min(4) * calls_per_turn; // 防振荡:最多处理 4 个截断轮
            prop_assert_eq!(rejected.len(), expected);
            for message in &rejected {
                let text = message.tool_result_content().unwrap_or_default();
                prop_assert!(text.contains("truncated"), "拒执行文案应提示截断: {text}");
            }

            // 防振荡:超过 max_truncation_retries(默认 3,第 4 轮处理完后)以护栏终止
            if truncated_turns > 3 {
                prop_assert_eq!(
                    output.stop,
                    rpi_agent::RunStop::BudgetExhausted(rpi_agent::BudgetKind::TruncationRetries)
                );
            } else {
                prop_assert_eq!(output.stop, rpi_agent::RunStop::EndTurn);
            }
            Ok(())
        })
        .unwrap_or_else(|error| panic!("property failed: {error:?}"));
    }
}
