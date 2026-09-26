//! rpi-session —— JSONL 会话树、投影、compaction(06 文档)。
//!
//! 只依赖 rpi-agent 的**类型**(方针文档 §1;经 re-export,不直接依赖 rpi-ai);
//! core 经由注入使用,本 crate 整体可拆卸。核心不变量:**append-only** ——
//! 分支、压缩、上下文修改都是追加 entry,从不改写历史(06 文档)。
//!
//! 摘要的 LLM 调用经 `Summarizer` trait 注入,provider 支撑的实现由装配方提供。

pub mod compaction;
pub mod entry;
pub mod manager;
pub mod projection;

pub use compaction::{
    calculate_context_tokens, create_fixed_summarizer, estimate_context_tokens,
    estimate_projected_context_tokens, estimate_tokens, find_cut_point, find_turn_start_index,
    get_summarization_failure, run_compaction, serialize_conversation, should_compact,
    update_summarization_prompt, CompactionOutcome, CompactionSettings, ContextUsageEstimate,
    CutPointResult, SummarizationRequest, SummarizationResponse, Summarizer,
    DEFAULT_COMPACTION_SETTINGS, SUMMARIZATION_PROMPT, SUMMARIZATION_SYSTEM_PROMPT,
};
pub use entry::{
    ContextReplacement, Entry, SessionHeader, SessionTreeNode, CURRENT_SESSION_VERSION,
};
pub use manager::{create_session, create_session_with, SessionError, SessionManager};
pub use projection::{
    build_context_entries, build_session_context, build_session_path, build_session_projection,
    session_entry_to_context_messages, ModelRef, ProjectedEntry, SessionContext, SessionProjection,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::AgentMessage;

    #[test]
    fn appends_to_memory_and_persists_jsonl() {
        let path = std::env::temp_dir().join(format!("rpi-session-test-{}.jsonl", uuid::Uuid::now_v7()));
        let session = create_session(Some(&path)).unwrap();
        let first = session.append_message(AgentMessage::user("第一条")).unwrap();
        session.append_message(AgentMessage::user("第二条")).unwrap();

        assert_eq!(session.entries().len(), 2);
        assert_eq!(session.get_leaf_id().as_deref(), Some(session.entries()[1].id()));
        let _ = first;

        // 文件:首行 header + 两条 message entry
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        let header: SessionHeader = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header.kind, "session");
        assert_eq!(header.version, CURRENT_SESSION_VERSION);
        let parsed: Entry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed, session.entries()[0]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reloads_existing_session_file() {
        let path = std::env::temp_dir().join(format!("rpi-session-reload-{}.jsonl", uuid::Uuid::now_v7()));
        let (leaf, id) = {
            let session = create_session(Some(&path)).unwrap();
            let id = session.session_id().to_string();
            session.append_message(AgentMessage::user("q")).unwrap();
            let leaf = session.get_leaf_id().unwrap();
            (leaf, id)
        };
        // 重新打开:恢复树与 leaf(重启续聊)
        let session = create_session(Some(&path)).unwrap();
        assert_eq!(session.session_id(), id);
        assert_eq!(session.get_leaf_id().as_deref(), Some(leaf.as_str()));
        assert_eq!(session.entries().len(), 1);
        // 续写:parentId 接上原 leaf
        let next = session.append_message(AgentMessage::user("again")).unwrap();
        assert_eq!(session.get_entry(&next).unwrap().parent_id(), Some(leaf.as_str()));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn corrupt_lines_are_skipped_and_counted() {
        let path = std::env::temp_dir().join(format!("rpi-session-corrupt-{}.jsonl", uuid::Uuid::now_v7()));
        let session = create_session(Some(&path)).unwrap();
        session.append_message(AgentMessage::user("good")).unwrap();
        drop(session);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{not json").unwrap();
        writeln!(file).unwrap();
        writeln!(file, "{{\"type\":\"message\",\"id\":\"x\"}}").unwrap(); // 缺 message 字段 → 损坏
        drop(file);

        let session = create_session(Some(&path)).unwrap();
        assert_eq!(session.corrupt_lines(), 2, "损坏行跳过并计数");
        assert_eq!(session.entries().len(), 1);
        std::fs::remove_file(&path).unwrap();
    }
}
