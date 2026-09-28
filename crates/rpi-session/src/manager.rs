//! SessionManager(06 文档 §1):append-only JSONL 会话树。同一文件内分叉 =
//! 移动 leaf 指针继续追加,零拷贝;压缩/上下文修改都是追加 entry,从不改写历史。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rpi_agent::{AgentMessage, Usage};

use crate::entry::{generate_id, ContextReplacement, Entry, SessionHeader, SessionTreeNode};
use crate::projection::{
    build_context_entries, build_session_projection, ModelRef, SessionProjection,
};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing session header (first line must be a session header)")]
    MissingHeader,
    #[error("entry `{0}` not found")]
    UnknownEntry(String),
}

struct State {
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
}

/// 会话管理器:JSONL 文件的唯一权威所有者(09 A4)。
pub struct SessionManager {
    path: Option<PathBuf>,
    header: SessionHeader,
    state: Mutex<State>,
    /// 加载时跳过的损坏行数(边界:JSONL 损坏行不致命)
    corrupt_lines: usize,
}

/// 工厂:传入路径即持久化会话(存在则加载续聊),`None` 为纯内存会话。
pub fn create_session(path: Option<impl AsRef<Path>>) -> Result<Box<SessionManager>, SessionError> {
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();
    create_session_with(path, &cwd, None)
}

/// 工厂(带 cwd 与 parentSession):pi 的 NewSessionOptions。
pub fn create_session_with(
    path: Option<impl AsRef<Path>>,
    cwd: &str,
    parent_session: Option<&str>,
) -> Result<Box<SessionManager>, SessionError> {
    let path = path.map(|p| p.as_ref().to_path_buf());
    let mut fresh_file = false;
    if let Some(path) = &path {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        fresh_file = !path.exists();
    }
    let (header, entries, corrupt_lines) = match &path {
        Some(path) if path.exists() => load_file(path)?,
        _ => (
            SessionHeader::new(
                new_session_id(),
                cwd.to_string(),
                parent_session.map(str::to_string),
            ),
            Vec::new(),
            0,
        ),
    };
    // 新文件先落 header 首行(06 文档 §1.1)
    if fresh_file {
        if let Some(path) = &path {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(file, "{}", serde_json::to_string(&header)?)?;
        }
    }

    let by_id: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.id().to_string(), i))
        .collect();
    let leaf_id = entries.last().map(|entry| entry.id().to_string());

    let manager = SessionManager {
        path,
        header,
        state: Mutex::new(State {
            entries,
            by_id,
            leaf_id,
        }),
        corrupt_lines,
    };
    Ok(Box::new(manager))
}

fn new_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 文件名可用的项目前缀上限(字节截断按 char 边界;uuid + 分隔符 + 扩展名约占 50)
const PROJECT_PREFIX_MAX_CHARS: usize = 100;

/// 会话文件名的项目前缀:完整 cwd 编码为文件名安全字符串(分项目管理)。
/// 路径分隔符 `/` `\` → `-`,空白与文件名非法字符(: * ? " < > | 及控制字符)
/// → `_`,其余字符(含中文等非 ASCII)保留;去除首尾 `-`,超长按 char 边界
/// 截断;结果为空(如 cwd = "/")时回退 "session"。
pub fn project_prefix(cwd: &str) -> String {
    let mut prefix: String = cwd
        .chars()
        .map(|c| match c {
            '/' | '\\' => '-',
            ' ' | '\t' | '\n' | '\r' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    prefix = prefix.trim_matches('-').to_string();
    if prefix.chars().count() > PROJECT_PREFIX_MAX_CHARS {
        prefix = prefix.chars().take(PROJECT_PREFIX_MAX_CHARS).collect();
    }
    if prefix.is_empty() {
        prefix = "session".to_string();
    }
    prefix
}

/// 工厂:在目录下创建 `<项目前缀>__<session-id>.jsonl` 会话文件(目录不存在则
/// 创建),前缀由 cwd 编码(`project_prefix`)实现分项目管理,id 即 session id。
pub fn create_session_in_dir(
    dir: impl AsRef<Path>,
    cwd: &str,
    parent_session: Option<&str>,
) -> Result<Box<SessionManager>, SessionError> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir)?;
    let id = new_session_id();
    let path = dir.join(format!("{}__{id}.jsonl", project_prefix(cwd)));
    // 先落首行 header(保证文件名与 session id 一致),再按既有文件打开
    let header = SessionHeader::new(id, cwd.to_string(), parent_session.map(str::to_string));
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
    }
    create_session_with(Some(path), cwd, parent_session)
}

/// 找最近一次活动的会话文件(pi `--continue` 语义):按文件修改时间取最新;
/// 传 cwd 时只考虑 header.cwd 匹配的会话(当前项目的会话)。
/// 非法/损坏文件跳过;目录不存在或无匹配返回 None。
pub fn find_latest_session_file(dir: impl AsRef<Path>, cwd: Option<&str>) -> Option<PathBuf> {
    let dir = dir.as_ref();
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if let Some(cwd) = cwd {
            match read_session_header(&path) {
                Ok(header) if header.kind == "session" && header.cwd == cwd => {}
                _ => continue,
            }
        }
        if best.as_ref().is_none_or(|(latest, _)| modified > *latest) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

fn read_session_header(path: &Path) -> Result<SessionHeader, SessionError> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line)?;
    Ok(serde_json::from_str(line.trim())?)
}

/// 加载既有 JSONL:首行 header,其后每行一个 entry;损坏行跳过并计数。
fn load_file(path: &Path) -> Result<(SessionHeader, Vec<Entry>, usize), SessionError> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut header: Option<SessionHeader> = None;
    let mut entries = Vec::new();
    let mut corrupt = 0usize;
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if header.is_none() {
            let parsed: SessionHeader = serde_json::from_str(trimmed)?;
            if parsed.kind != "session" {
                return Err(SessionError::MissingHeader);
            }
            header = Some(parsed);
            continue;
        }
        match serde_json::from_str::<Entry>(trimmed) {
            Ok(entry) => entries.push(entry),
            Err(_) => corrupt += 1,
        }
    }
    let header = header.ok_or(SessionError::MissingHeader)?;
    repair_missing_trailing_newline(path)?;
    Ok((header, entries, corrupt))
}

/// 崩溃恢复:上次写入中途崩溃会留下末尾无换行的半行,load 时已按损坏行跳过;
/// 若不补上换行,下一次 append 会直接拼接在半行之后,新 entry 被静默吞掉。
fn repair_missing_trailing_newline(path: &Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    drop(file);
    if last[0] != b'\n' {
        let mut append = std::fs::OpenOptions::new().append(true).open(path)?;
        append.write_all(b"\n")?;
    }
    Ok(())
}

impl SessionManager {
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    pub fn session_id(&self) -> &str {
        &self.header.id
    }

    pub fn cwd(&self) -> &str {
        &self.header.cwd
    }

    pub fn file_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 加载时跳过的损坏行数(0 = 干净)。
    pub fn corrupt_lines(&self) -> usize {
        self.corrupt_lines
    }

    /// 全部 entry(文件顺序的防御性拷贝)。
    pub fn entries(&self) -> Vec<Entry> {
        self.state.lock().unwrap().entries.clone()
    }

    pub fn get_entry(&self, id: &str) -> Option<Entry> {
        let state = self.state.lock().unwrap();
        state.by_id.get(id).map(|&i| state.entries[i].clone())
    }

    pub fn get_leaf_id(&self) -> Option<String> {
        self.state.lock().unwrap().leaf_id.clone()
    }

    pub fn session_name(&self) -> Option<String> {
        // 最新一条 session_info entry 生效:name=None 表示"清除会话名",
        // 必须停止查找返回 None,不能跳过它让旧名复活
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                Entry::SessionInfo { name, .. } => Some(name.clone()),
                _ => None,
            })?
    }

    /// 内部追加:分配 id/parentId,落索引,append-only 写文件。
    fn append_entry(&self, mut entry: Entry) -> Result<String, SessionError> {
        let mut state = self.state.lock().unwrap();
        let id = generate_id(&|id| state.by_id.contains_key(id));
        entry.set_parent(state.leaf_id.clone());
        match &mut entry {
            Entry::Message {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::ThinkingLevelChange {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::ModelChange {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::Usage {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::Compaction {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::ToolSetChange {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::ModeChange {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::BranchSummary {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::Custom {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::CustomMessage {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::ContextEdit {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::Label {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::SessionInfo {
                id: entry_id,
                timestamp,
                ..
            }
            | Entry::ContextRef {
                id: entry_id,
                timestamp,
                ..
            } => {
                *entry_id = id.clone();
                if *timestamp == 0 {
                    *timestamp = rpi_agent::now_ms();
                }
            }
        }
        if let Some(path) = &self.path {
            let json = serde_json::to_string(&entry)?;
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            // 单次 write_all:避免 write_fmt 拆分系统调用产生半行
            let mut line = json.into_bytes();
            line.push(b'\n');
            file.write_all(&line)?;
        }
        let position = state.entries.len();
        state.by_id.insert(id.clone(), position);
        state.entries.push(entry);
        state.leaf_id = Some(id.clone());
        Ok(id)
    }

    pub fn append_message(&self, message: AgentMessage) -> Result<String, SessionError> {
        self.append_entry(Entry::Message {
            id: String::new(),
            parent_id: None,
            message,
            timestamp: 0,
        })
    }

    pub fn append_thinking_level_change(
        &self,
        thinking_level: impl Into<String>,
    ) -> Result<String, SessionError> {
        self.append_entry(Entry::ThinkingLevelChange {
            id: String::new(),
            parent_id: None,
            thinking_level: thinking_level.into(),
            timestamp: 0,
        })
    }

    pub fn append_model_change(
        &self,
        provider: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Result<String, SessionError> {
        self.append_entry(Entry::ModelChange {
            id: String::new(),
            parent_id: None,
            provider: provider.into(),
            model_id: model_id.into(),
            timestamp: 0,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_usage(
        &self,
        kind: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        usage: Usage,
        note: Option<String>,
    ) -> Result<String, SessionError> {
        self.append_entry(Entry::Usage {
            id: String::new(),
            parent_id: None,
            kind: kind.into(),
            provider: provider.into(),
            model: model.into(),
            usage,
            note,
            timestamp: 0,
        })
    }

    /// 追加压缩 entry(06 文档 §3.4):summary + firstKeptEntryId + tokensBefore;
    /// 原 entry 保留在树中。
    pub fn append_compaction(
        &self,
        summary: impl Into<String>,
        first_kept_entry_id: impl Into<String>,
        tokens_before: u64,
        details: Option<serde_json::Value>,
        usage: Option<Usage>,
        from_hook: bool,
    ) -> Result<String, SessionError> {
        self.append_entry(Entry::Compaction {
            id: String::new(),
            parent_id: None,
            summary: summary.into(),
            first_kept_entry_id: first_kept_entry_id.into(),
            tokens_before,
            details,
            usage,
            from_hook: from_hook.then_some(true),
            timestamp: 0,
        })
    }

    /// 追加激活工具集变更 entry(元数据,不进模型上下文;只记工具名)。
    pub fn append_tool_set_change(&self, tools: &[String]) -> Result<String, SessionError> {
        self.append_entry(Entry::ToolSetChange {
            id: String::new(),
            parent_id: None,
            tools: tools.to_vec(),
            timestamp: 0,
        })
    }

    /// 追加会话模式变更 entry(元数据,不进模型上下文;恢复时按此重建模式)。
    pub fn append_mode_change(&self, mode: impl Into<String>) -> Result<String, SessionError> {
        self.append_entry(Entry::ModeChange {
            id: String::new(),
            parent_id: None,
            mode: mode.into(),
            timestamp: 0,
        })
    }

    pub fn append_branch_summary(
        &self,
        from_id: impl Into<String>,
        summary: impl Into<String>,
        from_hook: bool,
    ) -> Result<String, SessionError> {
        self.append_entry(Entry::BranchSummary {
            id: String::new(),
            parent_id: None,
            from_id: from_id.into(),
            summary: summary.into(),
            details: None,
            usage: None,
            from_hook: from_hook.then_some(true),
            timestamp: 0,
        })
    }

    pub fn append_custom(
        &self,
        custom_type: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        self.append_entry(Entry::Custom {
            id: String::new(),
            parent_id: None,
            custom_type: custom_type.into(),
            data,
            timestamp: 0,
        })
    }

    pub fn append_custom_message(
        &self,
        custom_type: impl Into<String>,
        content: impl Into<String>,
        details: Option<serde_json::Value>,
        display: bool,
    ) -> Result<String, SessionError> {
        self.append_entry(Entry::CustomMessage {
            id: String::new(),
            parent_id: None,
            custom_type: custom_type.into(),
            content: content.into(),
            details,
            display,
            timestamp: 0,
        })
    }

    /// append-only 上下文修改(06 文档 §1.2):replacement=None 剔除 target。
    pub fn append_context_edit(
        &self,
        target_id: impl Into<String>,
        replacement: Option<ContextReplacement>,
    ) -> Result<String, SessionError> {
        self.append_entry(Entry::ContextEdit {
            id: String::new(),
            parent_id: None,
            target_id: target_id.into(),
            replacement,
            timestamp: 0,
        })
    }

    pub fn append_label(
        &self,
        target_id: impl Into<String>,
        label: Option<String>,
    ) -> Result<String, SessionError> {
        self.append_entry(Entry::Label {
            id: String::new(),
            parent_id: None,
            target_id: target_id.into(),
            label,
            timestamp: 0,
        })
    }

    pub fn append_session_info(&self, name: Option<String>) -> Result<String, SessionError> {
        self.append_entry(Entry::SessionInfo {
            id: String::new(),
            parent_id: None,
            name,
            timestamp: 0,
        })
    }

    /// 记录一次向模型提交的请求(上下文审计):`snapshot` 是 **发送前的原始
    /// 请求体**(provider on_payload 观测的第一手数据),原样写入 session 文件
    /// 旁的 `<file-stem>.ctx/<uuid>.json`,并在会话树追加 `context_ref` entry
    /// (path 指向快照文件)。快照 entry **不进**模型上下文(projection 显式
    /// 排除),恢复/压缩等既有管线不受影响。纯内存会话(无文件)跳过,返回 None。
    pub fn append_context_snapshot(
        &self,
        snapshot: &serde_json::Value,
    ) -> Result<Option<String>, SessionError> {
        let Some(session_path) = self.path.clone() else {
            return Ok(None);
        };
        // 快照目录:session 文件旁 `<file-stem>.ctx/`(find_latest_session_file
        // 只扫描 *.jsonl,该目录不会被误读)
        let stem = session_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_else(|| "session".to_string());
        let ctx_dir = session_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{stem}.ctx"));
        std::fs::create_dir_all(&ctx_dir)?;
        let snapshot_file = ctx_dir.join(format!("{}.json", uuid::Uuid::now_v7().simple()));
        {
            use std::io::Write;
            // 原样落盘(第一手),单次 write_all,与 append_entry 同样的半行防护
            let json = serde_json::to_vec_pretty(snapshot)?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&snapshot_file)?;
            file.write_all(&json)?;
        }
        let entry_id = self.append_entry(Entry::ContextRef {
            id: String::new(),
            parent_id: None,
            path: snapshot_file.display().to_string(),
            timestamp: 0,
        })?;
        Ok(Some(entry_id))
    }

    /// 分支(06 文档 §1.3):把 leaf 指针移到树中较早节点继续追加 —— 同一文件
    /// 内分叉,零拷贝。
    pub fn branch(&self, branch_from_id: &str) -> Result<(), SessionError> {
        let mut state = self.state.lock().unwrap();
        if !state.by_id.contains_key(branch_from_id) {
            return Err(SessionError::UnknownEntry(branch_from_id.to_string()));
        }
        state.leaf_id = Some(branch_from_id.to_string());
        Ok(())
    }

    /// 活动分支(leaf→root 路径)的 entry 拷贝。
    pub fn branch_entries(&self) -> Vec<Entry> {
        let leaf = self.state.lock().unwrap().leaf_id.clone();
        crate::projection::build_session_path(&self.state.lock().unwrap().entries, leaf.as_deref())
            .into_iter()
            .cloned()
            .collect()
    }

    /// 投影(①②③ 全流程)。
    pub fn projection(&self) -> SessionProjection {
        let leaf = self.state.lock().unwrap().leaf_id.clone();
        build_session_projection(&self.state.lock().unwrap().entries, leaf.as_deref())
    }

    /// 压缩感知的上下文 entry(①②)。
    pub fn context_entries(&self) -> Vec<Entry> {
        let leaf = self.state.lock().unwrap().leaf_id.clone();
        build_context_entries(&self.state.lock().unwrap().entries, leaf.as_deref())
            .into_iter()
            .cloned()
            .collect()
    }

    /// 模型引用(设置态投影)。
    pub fn model_ref(&self) -> Option<ModelRef> {
        self.projection().model
    }

    /// getTree():防御性拷贝的树(含 resolved label)。
    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        let state = self.state.lock().unwrap();
        // label 解析:label entry 按 targetId 生效(后写覆盖)
        let mut labels: HashMap<String, (String, i64)> = HashMap::new();
        for entry in &state.entries {
            if let Entry::Label {
                target_id,
                label: Some(label),
                timestamp,
                ..
            } = entry
            {
                labels.insert(target_id.clone(), (label.clone(), *timestamp));
            }
        }
        let labeled = |entry: &Entry| {
            labels
                .get(entry.id())
                .map(|(label, ts)| (Some(label.clone()), Some(*ts)))
                .unwrap_or((None, None))
        };

        // 逆序两阶段构建:children 先于 parent 定型,parent 槽位保持可访问;
        // parent 不在文件中(损坏行被跳过等)时按根处理,不 panic
        let index: HashMap<&str, usize> = state
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| (entry.id(), i))
            .collect();
        let mut nodes: Vec<Option<SessionTreeNode>> = state
            .entries
            .iter()
            .map(|entry| {
                let (label, label_timestamp) = labeled(entry);
                Some(SessionTreeNode {
                    entry: entry.clone(),
                    children: Vec::new(),
                    label,
                    label_timestamp,
                })
            })
            .collect();
        let mut roots: Vec<SessionTreeNode> = Vec::new();
        for i in (0..state.entries.len()).rev() {
            let mut node = nodes[i].take().expect("each entry yields one node");
            node.children.reverse(); // 逆序挂接恢复文件顺序
            match state.entries[i]
                .parent_id()
                .and_then(|pid| index.get(pid).copied())
            {
                Some(parent) => match nodes[parent].as_mut() {
                    Some(parent_node) => parent_node.children.push(node),
                    // 损坏数据:parentId 指向文件中更靠后的 entry(槽位已被消费)→ 按根处理
                    None => roots.push(node),
                },
                None => roots.push(node),
            }
        }
        roots.reverse();
        roots
    }
}
