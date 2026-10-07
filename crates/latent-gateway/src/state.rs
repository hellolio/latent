//! `<数据目录>/gateway/state.json`(原子写:临时文件 + rename)。
//!
//! **唯一写者是 daemon 进程**:CLI 子命令经控制面请求变更(§4.10,杜绝
//! 双写者丢更新)。去重表/幂等表仅在内存,不进本文件。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionRecord {
    pub file: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPairing {
    pub code: String,
    pub user_id: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovedPairing {
    pub approved_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PairingState {
    /// 渠道 id → 未决配对请求
    pub pending: HashMap<String, Vec<PendingPairing>>,
    /// "<channel>:<userId>" → 批准记录
    pub approved: HashMap<String, ApprovedPairing>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GatewayState {
    /// session key → 会话文件(重启后 resume)
    pub sessions: HashMap<String, SessionRecord>,
    pub pairing: PairingState,
}

/// 当前毫秒时间戳(仓库约定:时间戳用毫秒整数)。
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// state.json 存取(daemon 内单线程写;Mutex 保护读改写)。
pub struct StateStore {
    path: PathBuf,
    inner: Mutex<GatewayState>,
}

impl StateStore {
    /// 加载既有 state.json(缺失/损坏 = 空状态起家,损坏打诊断)。
    pub fn load(path: PathBuf) -> Self {
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| match serde_json::from_str::<GatewayState>(&text) {
                Ok(state) => Some(state),
                Err(error) => {
                    eprintln!(
                        "[latent-gateway] state.json 解析失败(按空状态继续): {error}"
                    );
                    None
                }
            })
            .unwrap_or_default();
        StateStore {
            path,
            inner: Mutex::new(state),
        }
    }

    pub fn in_memory() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        StateStore {
            path: std::env::temp_dir().join(format!("latent-gateway-test-state-{unique}.json")),
            inner: Mutex::new(GatewayState::default()),
        }
    }

    /// 原子落盘:临时文件 + rename(崩溃不留半文件)。
    pub fn save(&self) -> Result<(), String> {
        let state = self.inner.lock().unwrap().clone();
        let json = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn snapshot(&self) -> GatewayState {
        self.inner.lock().unwrap().clone()
    }

    // ---- sessions ----

    pub fn session_file(&self, key: &str) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .get(key)
            .map(|record| PathBuf::from(&record.file))
    }

    pub fn set_session_file(&self, key: &str, file: &Path, now_ms: u64) {
        self.inner.lock().unwrap().sessions.insert(
            key.to_string(),
            SessionRecord {
                file: file.display().to_string(),
                created_at: now_ms,
            },
        );
    }

    pub fn remove_session(&self, key: &str) {
        self.inner.lock().unwrap().sessions.remove(key);
    }

    // ---- pairing ----

    pub fn pairing(&self) -> PairingState {
        self.inner.lock().unwrap().pairing.clone()
    }

    pub fn set_pairing(&self, pairing: PairingState) {
        self.inner.lock().unwrap().pairing = pairing;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_reload_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "latent-gw-state-test-{}-{}.json",
            std::process::id(),
            chrono_id()
        ));
        {
            let store = StateStore::load(path.clone());
            store.set_session_file(
                "agent:main:qq:group:12345",
                Path::new("/tmp/sessions/a.jsonl"),
                1728000000000,
            );
            store.save().unwrap();
        }
        let store = StateStore::load(path.clone());
        assert_eq!(
            store.session_file("agent:main:qq:group:12345"),
            Some(PathBuf::from("/tmp/sessions/a.jsonl"))
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn corrupt_state_starts_empty() {
        let path = std::env::temp_dir().join(format!(
            "latent-gw-state-bad-{}-{}.json",
            std::process::id(),
            chrono_id()
        ));
        std::fs::write(&path, "{not json").unwrap();
        let store = StateStore::load(path.clone());
        assert_eq!(store.session_file("x"), None);
        std::fs::remove_file(&path).ok();
    }

    fn chrono_id() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
