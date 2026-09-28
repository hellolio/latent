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
    get_summarization_failure, reserve_tokens_for_window, run_compaction,
    serialize_conversation, should_compact, update_summarization_prompt, CompactionOutcome,
    CompactionSettings, ContextUsageEstimate, CutPointResult, SummarizationRequest,
    SummarizationResponse, Summarizer, DEFAULT_COMPACTION_SETTINGS, SUMMARIZATION_PROMPT,
    SUMMARIZATION_SYSTEM_PROMPT,
};
pub use entry::{
    ContextReplacement, Entry, SessionHeader, SessionTreeNode, CURRENT_SESSION_VERSION,
};
pub use manager::{
    create_session, create_session_in_dir, create_session_with, find_latest_session_file,
    project_prefix, SessionError, SessionManager,
};
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
        let path =
            std::env::temp_dir().join(format!("rpi-session-test-{}.jsonl", uuid::Uuid::now_v7()));
        let session = create_session(Some(&path)).unwrap();
        let first = session
            .append_message(AgentMessage::user("第一条"))
            .unwrap();
        session
            .append_message(AgentMessage::user("第二条"))
            .unwrap();

        assert_eq!(session.entries().len(), 2);
        assert_eq!(
            session.get_leaf_id().as_deref(),
            Some(session.entries()[1].id())
        );
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
        let path =
            std::env::temp_dir().join(format!("rpi-session-reload-{}.jsonl", uuid::Uuid::now_v7()));
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
        assert_eq!(
            session.get_entry(&next).unwrap().parent_id(),
            Some(leaf.as_str())
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn corrupt_lines_are_skipped_and_counted() {
        let path = std::env::temp_dir().join(format!(
            "rpi-session-corrupt-{}.jsonl",
            uuid::Uuid::now_v7()
        ));
        let session = create_session(Some(&path)).unwrap();
        session.append_message(AgentMessage::user("good")).unwrap();
        drop(session);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{{not json").unwrap();
        writeln!(file).unwrap();
        writeln!(file, "{{\"type\":\"message\",\"id\":\"x\"}}").unwrap(); // 缺 message 字段 → 损坏
        drop(file);

        let session = create_session(Some(&path)).unwrap();
        assert_eq!(session.corrupt_lines(), 2, "损坏行跳过并计数");
        assert_eq!(session.entries().len(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    // ---- 分项目管理:文件名项目前缀 ----

    #[test]
    fn project_prefix_encodes_cwd_for_filenames() {
        assert_eq!(
            project_prefix("/Users/kin/Documents/10source/rpi"),
            "Users-kin-Documents-10source-rpi"
        );
        // 非法字符 → _,首尾 - 去除
        assert_eq!(project_prefix("/a b:c/"), "a_b_c");
        // 根路径回退
        assert_eq!(project_prefix("/"), "session");
        // 中文等非 ASCII 保留
        assert_eq!(project_prefix("/home/项目"), "home-项目");
        // 超长截断(按 char 边界)
        let deep = format!("/{}", "x".repeat(300));
        assert_eq!(project_prefix(&deep).chars().count(), 100);
    }

    #[test]
    fn tagged_session_file_is_excluded_from_continue() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-session-tag-{}",
            uuid::Uuid::now_v7().simple()
        ));
        let cwd = "/Users/kin/Documents/10source/rpi";
        // 主会话 + 两个带 tag 的子会话(子会话更新时间更晚)
        let _main = create_session_in_dir(&dir, cwd, None, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let child_a = create_session_in_dir(&dir, cwd, None, Some("abc12345")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let child_b = create_session_in_dir(&dir, cwd, None, Some("reviewer")).unwrap();

        let name_a = child_a.file_path().unwrap().file_name().unwrap().to_string_lossy().to_string();
        let name_b = child_b.file_path().unwrap().file_name().unwrap().to_string_lossy().to_string();
        assert!(name_a.contains("__abc12345__"), "{name_a}");
        assert!(name_b.contains("__reviewer__"), "{name_b}");

        // --continue 只选主会话(带 tag 的子会话文件被排除)
        let latest = find_latest_session_file(&dir, Some(cwd)).unwrap();
        let latest_name = latest.file_name().unwrap().to_string_lossy().to_string();
        assert!(!latest_name.contains("__abc12345__") && !latest_name.contains("__reviewer__"), "{latest_name}");
        // 无 cwd 过滤同样排除
        let latest_any = find_latest_session_file(&dir, None).unwrap();
        assert!(
            !latest_any.file_name().unwrap().to_string_lossy().contains("__reviewer__"),
            "{}",
            latest_any.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_session_in_dir_uses_project_prefix_filename() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-session-prefix-{}",
            uuid::Uuid::now_v7().simple()
        ));
        let cwd = "/Users/kin/Documents/10source/rpi";
        let session = create_session_in_dir(&dir, cwd, None, None).unwrap();
        let file_name = session
            .file_path()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            file_name.starts_with("Users-kin-Documents-10source-rpi__"),
            "文件名应以项目前缀开头: {file_name}"
        );
        assert!(file_name.ends_with(".jsonl"));
        // header.cwd 仍是完整路径(--continue 匹配依据)
        assert_eq!(session.cwd(), cwd);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- 上下文快照:context_ref entry + .ctx 目录 ----

    #[test]
    fn context_snapshot_writes_file_and_entry_and_stays_out_of_context() {
        let path = std::env::temp_dir().join(format!(
            "rpi-session-ctxref-{}__.jsonl",
            uuid::Uuid::now_v7().simple()
        ));
        let session = create_session(Some(&path)).unwrap();
        session.append_message(AgentMessage::user("问题")).unwrap();

        // 原始请求体(on_payload 观测的第一手数据)
        let body = serde_json::json!({
            "model": "glm-5.3-flash",
            "messages": [{"role": "system", "content": "You are hart"}, {"role": "user", "content": "问题"}],
        });
        let entry_id = session
            .append_context_snapshot(&body)
            .unwrap()
            .expect("文件会话应记录快照");

        // entry 已落盘,path 指向旁路 .ctx 目录下的快照文件
        let entry = session.get_entry(&entry_id).unwrap();
        let snapshot_path = match &entry {
            Entry::ContextRef { path, .. } => path.clone(),
            other => panic!("应为 context_ref entry: {other:?}"),
        };
        let snapshot_path = std::path::Path::new(&snapshot_path);
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        assert_eq!(
            snapshot_path.parent().unwrap(),
            path.parent().unwrap().join(format!("{stem}.ctx")),
            "快照在 session 文件旁 <stem>.ctx/ 目录"
        );
        // 第一手:文件内容与请求体原样一致
        let content = std::fs::read_to_string(snapshot_path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value, body, "快照应原样保存请求体");

        // 核心回归:context_ref 不进模型上下文(重建结果与没有它时一致)
        let messages = session.projection().messages;
        assert_eq!(messages.len(), 1, "仅 user 消息进入上下文");
        assert!(matches!(messages[0], AgentMessage::User { .. }));
        // serde roundtrip
        let parsed: Entry = serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(parsed, entry);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir_all(snapshot_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn context_snapshot_skipped_for_memory_sessions() {
        let session = create_session(None::<String>).unwrap();
        let body = serde_json::json!({"messages": []});
        let result = session.append_context_snapshot(&body).unwrap();
        assert!(result.is_none(), "内存会话无文件,跳过快照");
        assert!(session.entries().is_empty());
    }
}
