//! entry 类型(06 文档 §1.2):12 种 SessionEntry + SessionHeader,serde 形态与
//! pi JSONL 同构(`type` 判别符 + camelCase 字段)。每个 entry 带 `id`(8 位 hex)、
//! `parentId`(根为 null)、`timestamp`(毫秒)构成树。
//!
//! 与 pi 的差异:entry timestamp 用毫秒整数(pi 用 ISO 字符串)—— rpi 会话文件
//! 自用,未承诺逐字节兼容(见 docs/06 踩坑记录)。

use serde::{Deserialize, Serialize};

use rpi_agent::{AgentMessage, Usage};

pub const CURRENT_SESSION_VERSION: u32 = 4;

/// 文件首行(06 文档 §1.1):`{"type":"session", ...}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

impl SessionHeader {
    pub fn new(id: String, cwd: String, parent_session: Option<String>) -> Self {
        SessionHeader {
            kind: "session".into(),
            version: CURRENT_SESSION_VERSION,
            id,
            timestamp: rpi_agent::now_ms(),
            cwd,
            parent_session,
        }
    }
}

/// context_edit 的替换内容:`None` = 从上下文剔除;`Some` = 只替换 content。
/// (pi 的 content 支持 string 或内容块;此处统一 string,投影时按消息角色包装)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextReplacement {
    pub content: String,
}

/// 会话 entry(06 文档 §1.2 表:11 种 pi 类型 + rpi 扩展的 context_ref)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// Compaction 携带快照/usage 等重字段,与 Label 等轻字段差距大是设计使然
#[allow(clippy::large_enum_variant)]
pub enum Entry {
    #[serde(rename_all = "camelCase")]
    Message {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        message: AgentMessage,
        #[serde(default)]
        timestamp: i64,
    },
    #[serde(rename_all = "camelCase")]
    ThinkingLevelChange {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        thinking_level: String,
        #[serde(default)]
        timestamp: i64,
    },
    #[serde(rename_all = "camelCase")]
    ModelChange {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        provider: String,
        model_id: String,
        #[serde(default)]
        timestamp: i64,
    },
    /// 核算用,不进模型上下文(如 cache_warm)
    #[serde(rename_all = "camelCase")]
    Usage {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        kind: String,
        provider: String,
        model: String,
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default)]
        timestamp: i64,
    },
    /// 压缩 entry:原 entry 保留在树中,上下文以摘要形式重建(06 文档 §3.4)
    #[serde(rename_all = "camelCase")]
    Compaction {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
        /// 压缩边界处的完整 prompt + 工具状态快照(下一轮上下文从这里恢复)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_message: Option<AgentMessage>,
        #[serde(default)]
        timestamp: i64,
    },
    /// 树导航离开分支时生成的摘要;**进**模型上下文
    #[serde(rename_all = "camelCase")]
    BranchSummary {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
        #[serde(default)]
        timestamp: i64,
    },
    /// 扩展持久状态,**不进** LLM 上下文
    #[serde(rename_all = "camelCase")]
    Custom {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
        #[serde(default)]
        timestamp: i64,
    },
    /// 扩展注入 LLM 上下文的消息(投影成 Custom)
    #[serde(rename_all = "camelCase")]
    CustomMessage {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        custom_type: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        #[serde(default)]
        display: bool,
        #[serde(default)]
        timestamp: i64,
    },
    /// append-only 上下文修改:replacement=None 剔除,Some 只替换 content
    #[serde(rename_all = "camelCase")]
    ContextEdit {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        target_id: String,
        replacement: Option<ContextReplacement>,
        #[serde(default)]
        timestamp: i64,
    },
    /// 书签
    #[serde(rename_all = "camelCase")]
    Label {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default)]
        timestamp: i64,
    },
    /// 会话名
    #[serde(rename_all = "camelCase")]
    SessionInfo {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default)]
        timestamp: i64,
    },
    /// 每次向模型提交请求的完整上下文快照引用:`path` 指向 session 文件旁
    /// `<file-stem>.ctx/` 下的快照文件(内容 = 发送前的原始请求体,on_payload
    /// 观测的第一手数据);**不进**模型上下文(投影显式排除)
    #[serde(rename_all = "camelCase")]
    ContextRef {
        id: String,
        #[serde(default)]
        parent_id: Option<String>,
        path: String,
        #[serde(default)]
        timestamp: i64,
    },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Message { id, .. }
            | Entry::ThinkingLevelChange { id, .. }
            | Entry::ModelChange { id, .. }
            | Entry::Usage { id, .. }
            | Entry::Compaction { id, .. }
            | Entry::BranchSummary { id, .. }
            | Entry::Custom { id, .. }
            | Entry::CustomMessage { id, .. }
            | Entry::ContextEdit { id, .. }
            | Entry::Label { id, .. }
            | Entry::SessionInfo { id, .. }
            | Entry::ContextRef { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Entry::Message { parent_id, .. }
            | Entry::ThinkingLevelChange { parent_id, .. }
            | Entry::ModelChange { parent_id, .. }
            | Entry::Usage { parent_id, .. }
            | Entry::Compaction { parent_id, .. }
            | Entry::BranchSummary { parent_id, .. }
            | Entry::Custom { parent_id, .. }
            | Entry::CustomMessage { parent_id, .. }
            | Entry::ContextEdit { parent_id, .. }
            | Entry::Label { parent_id, .. }
            | Entry::SessionInfo { parent_id, .. }
            | Entry::ContextRef { parent_id, .. } => parent_id.as_deref(),
        }
    }

    pub fn timestamp(&self) -> i64 {
        match self {
            Entry::Message { timestamp, .. }
            | Entry::ThinkingLevelChange { timestamp, .. }
            | Entry::ModelChange { timestamp, .. }
            | Entry::Usage { timestamp, .. }
            | Entry::Compaction { timestamp, .. }
            | Entry::BranchSummary { timestamp, .. }
            | Entry::Custom { timestamp, .. }
            | Entry::CustomMessage { timestamp, .. }
            | Entry::ContextEdit { timestamp, .. }
            | Entry::Label { timestamp, .. }
            | Entry::SessionInfo { timestamp, .. }
            | Entry::ContextRef { timestamp, .. } => *timestamp,
        }
    }

    pub fn set_parent(&mut self, parent_id: Option<String>) {
        match self {
            Entry::Message { parent_id: p, .. }
            | Entry::ThinkingLevelChange { parent_id: p, .. }
            | Entry::ModelChange { parent_id: p, .. }
            | Entry::Usage { parent_id: p, .. }
            | Entry::Compaction { parent_id: p, .. }
            | Entry::BranchSummary { parent_id: p, .. }
            | Entry::Custom { parent_id: p, .. }
            | Entry::CustomMessage { parent_id: p, .. }
            | Entry::ContextEdit { parent_id: p, .. }
            | Entry::Label { parent_id: p, .. }
            | Entry::SessionInfo { parent_id: p, .. }
            | Entry::ContextRef { parent_id: p, .. } => *p = parent_id,
        }
    }
}

/// 8 位 hex 短 id(pi 的 generateId;碰撞概率可忽略,仍由 by_id 索引兜底)。
pub(crate) fn generate_id(existing: &dyn Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        if !existing(&id) {
            return id;
        }
    }
    uuid::Uuid::new_v4().simple().to_string()
}

/// getTree() 的防御性拷贝节点(06 文档 §1.1):含 resolved label。
/// rpc 模式(get_tree 命令)直接序列化上线。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionTreeNode {
    pub entry: Entry,
    pub children: Vec<SessionTreeNode>,
    pub label: Option<String>,
    pub label_timestamp: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P0 回归:entry 的 type 判别符与字段名和 pi JSONL 同构。
    #[test]
    fn entry_roundtrip_matches_pi_type_tags() {
        let entries = vec![
            Entry::Message {
                id: "a1".into(),
                parent_id: None,
                message: AgentMessage::user("hi"),
                timestamp: 1,
            },
            Entry::ThinkingLevelChange {
                id: "a2".into(),
                parent_id: Some("a1".into()),
                thinking_level: "medium".into(),
                timestamp: 2,
            },
            Entry::ModelChange {
                id: "a3".into(),
                parent_id: Some("a2".into()),
                provider: "anthropic".into(),
                model_id: "claude".into(),
                timestamp: 3,
            },
            Entry::Usage {
                id: "a4".into(),
                parent_id: Some("a3".into()),
                kind: "cache_warm".into(),
                provider: "anthropic".into(),
                model: "claude".into(),
                usage: Usage::zero(),
                note: Some("warm".into()),
                timestamp: 4,
            },
            Entry::Compaction {
                id: "a5".into(),
                parent_id: Some("a4".into()),
                summary: "s".into(),
                first_kept_entry_id: "a1".into(),
                tokens_before: 100,
                details: None,
                usage: None,
                from_hook: None,
                system_message: None,
                timestamp: 5,
            },
            Entry::BranchSummary {
                id: "a6".into(),
                parent_id: Some("a5".into()),
                from_id: "a1".into(),
                summary: "b".into(),
                details: None,
                usage: None,
                from_hook: None,
                timestamp: 6,
            },
            Entry::Custom {
                id: "a7".into(),
                parent_id: Some("a6".into()),
                custom_type: "state".into(),
                data: Some(serde_json::json!({"k": 1})),
                timestamp: 7,
            },
            Entry::CustomMessage {
                id: "a8".into(),
                parent_id: Some("a7".into()),
                custom_type: "note".into(),
                content: "hello".into(),
                details: None,
                display: true,
                timestamp: 8,
            },
            Entry::ContextEdit {
                id: "a9".into(),
                parent_id: Some("a8".into()),
                target_id: "a1".into(),
                replacement: Some(ContextReplacement {
                    content: "edited".into(),
                }),
                timestamp: 9,
            },
            Entry::Label {
                id: "b1".into(),
                parent_id: Some("a9".into()),
                target_id: "a1".into(),
                label: Some("start".into()),
                timestamp: 10,
            },
            Entry::SessionInfo {
                id: "b2".into(),
                parent_id: Some("b1".into()),
                name: Some("my session".into()),
                timestamp: 11,
            },
            Entry::ContextRef {
                id: "b3".into(),
                parent_id: Some("b2".into()),
                path: "/tmp/sess.ctx/abc.json".into(),
                timestamp: 12,
            },
        ];
        for entry in &entries {
            let value = serde_json::to_value(entry).unwrap();
            let back: Entry = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(&back, entry);
            assert_eq!(value["id"], entry.id());
            assert_eq!(
                value["parentId"],
                serde_json::to_value(entry.parent_id()).unwrap()
            );
        }
        // 判别符抽查
        assert_eq!(
            serde_json::to_value(&entries[0]).unwrap()["type"],
            "message"
        );
        assert_eq!(
            serde_json::to_value(&entries[4]).unwrap()["type"],
            "compaction"
        );
        assert_eq!(
            serde_json::to_value(&entries[5]).unwrap()["type"],
            "branch_summary"
        );
        assert_eq!(
            serde_json::to_value(&entries[8]).unwrap()["type"],
            "context_edit"
        );
        assert_eq!(
            serde_json::to_value(&entries[10]).unwrap()["type"],
            "session_info"
        );
        assert_eq!(
            serde_json::to_value(&entries[11]).unwrap()["type"],
            "context_ref"
        );
    }

    #[test]
    fn header_roundtrip() {
        let header = SessionHeader::new("sid".into(), "/tmp".into(), Some("parent".into()));
        let value = serde_json::to_value(&header).unwrap();
        assert_eq!(value["type"], "session");
        assert_eq!(value["version"], 4);
        assert_eq!(value["parentSession"], "parent");
        let back: SessionHeader = serde_json::from_value(value).unwrap();
        assert_eq!(back, header);
    }
}
