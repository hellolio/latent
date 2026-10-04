//! 结果存储(storage.ts 的移植):内存 Map(跨工具调用的 responseId 协议)+
//! fetch 全文磁盘缓存(原子写 tmp+rename、0700/0600、TTL 1h、LRU 修剪,
//! 默认 128 条/128MB)。
//!
//! 与上游的差异(已确认取舍):session journal 重放(restoreFromSession)
//! 不做 —— latent 的工具没有 appendEntry 面;内存 + 磁盘缓存已覆盖主场景。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::CacheLimits;
use crate::types::{ExtractedContent, SearchResult};

pub const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const FETCH_CACHE_DIR: &str = "web-search-cache";
const MAX_METADATA_TEXT: usize = 8_192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredType {
    Search,
    Fetch,
    Research,
}

/// 单个 query 的完整结果(web_search 存储单元)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResultData {
    pub query: String,
    pub answer: String,
    pub results: Vec<SearchResult>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchCacheRef {
    pub version: u32,
    pub key: String,
    pub stored_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFetchUrlMetadata {
    pub url: String,
    pub title: String,
    pub error: Option<String>,
    pub content_length: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
}

/// 存储条目(search = 每 query 完整结果;fetch = 全文(仅内存,落盘的是
/// fetch_cache 引用);research = source_check artifact)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSearchData {
    pub id: String,
    #[serde(rename = "type")]
    pub stored_type: StoredType,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queries: Option<Vec<QueryResultData>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<ExtractedContent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_cache: Option<FetchCacheRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_metadata: Option<Vec<StoredFetchUrlMetadata>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_cache_error: Option<String>,
}

static STORED_RESULTS: LazyLock<Mutex<HashMap<String, StoredSearchData>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 磁盘缓存目录(可在装配时配置;默认 <config_dir>/web-search-cache)。
static FETCH_CACHE_ROOT: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

/// 配置缓存目录(装配期调用一次)。
pub fn set_fetch_cache_dir(dir: PathBuf) {
    *FETCH_CACHE_ROOT.lock().unwrap() = Some(dir);
}

fn fetch_cache_dir() -> PathBuf {
    FETCH_CACHE_ROOT
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| crate::config::config_dir(None).join(FETCH_CACHE_DIR))
}

pub fn generate_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn valid_cache_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn truncate_metadata_text(value: &str) -> String {
    if value.chars().count() > MAX_METADATA_TEXT {
        let mut truncated: String = value.chars().take(MAX_METADATA_TEXT).collect();
        truncated.push_str("...");
        truncated
    } else {
        value.to_string()
    }
}

pub fn metadata_for_urls(urls: &[ExtractedContent]) -> Vec<StoredFetchUrlMetadata> {
    urls.iter()
        .map(|url| StoredFetchUrlMetadata {
            url: truncate_metadata_text(&url.url),
            title: truncate_metadata_text(&url.title),
            error: url.error.as_ref().map(|error| truncate_metadata_text(error)),
            content_length: url.content.len(),
            mime_type: url.mime_type.as_ref().map(|mime| truncate_metadata_text(mime)),
            status: url.status,
            duration: url.duration,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 内存层
// ---------------------------------------------------------------------------

pub fn store_result(id: &str, data: StoredSearchData) {
    STORED_RESULTS.lock().unwrap().insert(id.to_string(), data);
}

pub fn get_result(id: &str) -> Option<StoredSearchData> {
    let mut guard = STORED_RESULTS.lock().unwrap();
    let data = guard.get(id)?.clone();
    if data.stored_type == StoredType::Fetch && now_ms() - data.timestamp >= CACHE_TTL.as_millis() as u64
    {
        let expired = unavailable_fetch_data(&data, "Cached fetched content is missing or expired");
        guard.insert(id.to_string(), expired.clone());
        return Some(expired);
    }
    if data.stored_type == StoredType::Fetch && data.urls.is_none() {
        // 内存里只有元数据 → 从磁盘缓存回灌
        let loaded = read_cached_fetch_data(&data);
        guard.insert(id.to_string(), loaded.clone());
        return Some(loaded);
    }
    Some(data)
}

/// fetch 存储入口:写磁盘缓存(失败降级为 fetch_cache_error),内存存轻量版。
pub fn store_fetched_content_result(
    id: &str,
    urls: Vec<ExtractedContent>,
    limits: CacheLimits,
) -> StoredSearchData {
    prune_expired_fetched_results(now_ms());
    let data = StoredSearchData {
        id: id.to_string(),
        stored_type: StoredType::Fetch,
        timestamp: now_ms(),
        queries: None,
        urls: Some(urls.clone()),
        artifact: None,
        fetch_cache: None,
        url_metadata: Some(metadata_for_urls(&urls)),
        fetch_cache_error: None,
    };
    let write_result = write_fetch_cache(&data, limits);
    let stored = match write_result {
        Ok(reference) => StoredSearchData {
            fetch_cache: Some(reference),
            urls: None,
            ..data
        },
        Err(error) => StoredSearchData {
            urls: None,
            fetch_cache_error: Some(truncate_metadata_text(&format!(
                "Failed to write fetched content cache: {error}"
            ))),
            ..data
        },
    };
    store_result(id, stored.clone());
    stored
}

fn unavailable_fetch_data(data: &StoredSearchData, reason: &str) -> StoredSearchData {
    let metadata = data.url_metadata.clone().unwrap_or_default();
    StoredSearchData {
        urls: Some(
            metadata
                .iter()
                .map(|meta| ExtractedContent {
                    url: meta.url.clone(),
                    title: meta.title.clone(),
                    content: String::new(),
                    error: Some(reason.to_string()),
                    mime_type: meta.mime_type.clone(),
                    status: meta.status,
                    duration: meta.duration,
                })
                .collect(),
        ),
        ..data.clone()
    }
}

/// 内存 miss 时从磁盘缓存回灌 fetch 全文。
fn read_cached_fetch_data(data: &StoredSearchData) -> StoredSearchData {
    if now_ms() - data.timestamp >= CACHE_TTL.as_millis() as u64 {
        return unavailable_fetch_data(data, "Cached fetched content is missing or expired");
    }
    let Some(reference) = &data.fetch_cache else {
        return unavailable_fetch_data(
            data,
            data.fetch_cache_error.as_deref().unwrap_or("Cached fetched content is unavailable"),
        );
    };
    let path = fetch_cache_dir().join(&reference.key);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return unavailable_fetch_data(data, "Cached fetched content is missing or expired");
        }
        Err(error) => {
            return unavailable_fetch_data(
                data,
                &format!("Cached fetched content could not be read: {error}"),
            );
        }
    };
    match serde_json::from_slice::<StoredSearchData>(&bytes) {
        Ok(parsed)
            if parsed.stored_type == StoredType::Fetch
                && parsed.id == data.id
                && parsed.urls.is_some() =>
        {
            StoredSearchData {
                url_metadata: data.url_metadata.clone(),
                ..parsed
            }
        }
        _ => unavailable_fetch_data(data, "Cached fetched content is invalid"),
    }
}

fn prune_expired_fetched_results(now: u64) {
    let mut guard = STORED_RESULTS.lock().unwrap();
    let ids: Vec<String> = guard
        .iter()
        .filter(|(_, data)| {
            data.stored_type == StoredType::Fetch
                && now - data.timestamp >= CACHE_TTL.as_millis() as u64
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        let data = guard.get(&id).expect("id from iteration").clone();
        guard.insert(
            id,
            unavailable_fetch_data(&data, "Cached fetched content is missing or expired"),
        );
    }
}

// ---------------------------------------------------------------------------
// 磁盘缓存层(原子写 + TTL/LRU 修剪)
// ---------------------------------------------------------------------------

/// 目录安全性检查:非符号链接且是目录;创建时 0700。
fn ensure_cache_dir(create: bool) -> Result<PathBuf, String> {
    let dir = fetch_cache_dir();
    if create {
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let metadata = std::fs::symlink_metadata(&dir).map_err(|error| error.to_string())?;
    if metadata.is_symlink() || !metadata.is_dir() {
        return Err("Fetched content cache path is not a safe directory".to_string());
    }
    Ok(dir)
}

/// 修剪:先按 TTL 删过期,再按 mtime 升序驱逐到条数/字节上限内。
fn prune_fetch_cache(limits: &CacheLimits, preferred_key: Option<&str>) -> Result<bool, String> {
    let Ok(dir) = ensure_cache_dir(false) else {
        return Ok(true);
    };
    let now = SystemTime::now();
    let mut files: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(false),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let is_cache_file = name.ends_with(".json")
            && valid_cache_key(name.trim_end_matches(".json"));
        let is_tmp_file = name.ends_with(".tmp");
        if !is_cache_file && !is_tmp_file {
            continue;
        }
        let Ok(metadata) = entry.metadata() else { continue };
        if metadata.is_symlink() || !metadata.is_file() {
            continue;
        }
        let age = now
            .duration_since(metadata.modified().unwrap_or(UNIX_EPOCH))
            .unwrap_or_default();
        if age >= CACHE_TTL {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if is_cache_file {
            files.push((path, metadata));
        }
    }
    files.sort_by_key(|(_, metadata)| {
        metadata
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
    });
    let total_bytes: u64 = files
        .iter()
        .map(|(_, metadata)| metadata.len())
        .sum::<u64>();
    let mut total_entries = files.len();
    let mut total_bytes = total_bytes;
    for (path, metadata) in &files {
        if total_entries <= limits.max_entries && total_bytes <= limits.max_bytes as u64 {
            break;
        }
        if preferred_key.is_some_and(|preferred| {
            path.file_name().and_then(|name| name.to_str()) == Some(preferred)
        }) {
            continue;
        }
        if std::fs::remove_file(path).is_ok() {
            total_entries -= 1;
            total_bytes -= metadata.len();
        }
    }
    Ok(total_entries <= limits.max_entries && total_bytes <= limits.max_bytes as u64)
}

fn write_fetch_cache(data: &StoredSearchData, limits: CacheLimits) -> Result<FetchCacheRef, String> {
    if !valid_cache_key(&data.id) {
        return Err(format!("Invalid fetched content cache id: {}", data.id));
    }
    let key = format!("{}.json", data.id);
    let serialized = serde_json::to_vec(data).map_err(|error| error.to_string())?;
    if serialized.len() as u64 > limits.max_bytes as u64 {
        return Err(format!(
            "Fetched content cache entry exceeds {} bytes",
            limits.max_bytes
        ));
    }
    let dir = ensure_cache_dir(true)?;
    if !prune_fetch_cache(&limits, Some(&key))? {
        return Err("Fetched content cache could not reserve space for a new entry".to_string());
    }
    let tmp_path = dir.join(format!(
        "{key}.{}.{}.tmp",
        std::process::id(),
        now_ms()
    ));
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .map_err(|error| error.to_string())?;
        file.write_all(&serialized).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
    }
    let final_path = dir.join(&key);
    if let Err(error) = std::fs::rename(&tmp_path, &final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error.to_string());
    }
    if !prune_fetch_cache(&limits, Some(&key))? {
        return Err("Fetched content cache could not meet its limits after writing".to_string());
    }
    Ok(FetchCacheRef {
        version: 1,
        key,
        stored_at: now_ms(),
    })
}

/// 手工验证入口:清空内存(测试用)。
#[cfg(test)]
pub fn clear_for_tests() {
    STORED_RESULTS.lock().unwrap().clear();
}

/// 供 get_search_content 展示:fetch 条目的元数据。
pub fn fetch_url_metadata(data: &StoredSearchData) -> Vec<StoredFetchUrlMetadata> {
    if let Some(metadata) = &data.url_metadata {
        return metadata.clone();
    }
    match &data.urls {
        Some(urls) => metadata_for_urls(urls),
        None => Vec::new(),
    }
}

/// 缓存根目录(测试/装配诊断用)。
pub fn cache_root() -> PathBuf {
    fetch_cache_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_urls(n: usize) -> Vec<ExtractedContent> {
        (0..n)
            .map(|index| ExtractedContent {
                url: format!("https://example.com/{index}"),
                title: format!("Page {index}"),
                content: "x".repeat(100),
                error: None,
                mime_type: Some("text/html".into()),
                status: Some(200),
                duration: Some(10),
            })
            .collect()
    }

    /// 两个场景合并为一个测试:全局缓存目录是共享静态,避免并行测试竞态。
    #[test]
    fn fetch_cache_roundtrip_and_lru_prune() {
        clear_for_tests();
        let temp = std::env::temp_dir().join(format!("latent-web-test-{}", uuid::Uuid::now_v7()));
        set_fetch_cache_dir(temp.clone());
        let limits = CacheLimits {
            max_entries: 2,
            max_bytes: 1024 * 1024,
        };

        // 回灌:内存存轻量版,读时从磁盘恢复全文
        let id = generate_id();
        store_fetched_content_result(&id, sample_urls(2), limits);
        let loaded = get_result(&id).expect("stored");
        assert_eq!(loaded.stored_type, StoredType::Fetch);
        let urls = loaded.urls.expect("urls rehydrated");
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].url, "https://example.com/0");
        assert!(temp.join(format!("{id}.json")).exists());

        // LRU:超出 max_entries 时驱逐最旧条目,最新条目保留
        let ids: Vec<String> = (0..3).map(|_| generate_id()).collect();
        for id in &ids {
            store_fetched_content_result(id, sample_urls(1), limits);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(temp.join(format!("{}.json", ids[2])).exists());
        assert!(!temp.join(format!("{}.json", ids[0])).exists());
        assert!(get_result(&ids[2]).is_some());

        let _ = std::fs::remove_dir_all(temp);
    }
}
