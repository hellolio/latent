//! DM 配对(上游 docs `channels/pairing.md` —— 语义逐项核实):
//!
//! - dmPolicy 默认 `pairing`:陌生人私聊 → 生成 **8 位码(大写,剔除
//!   0O1I)**,回复「配对码 XXXX,1 小时内有效」;**每渠道账户 pending 上限 3**,
//!   超限拒绝;
//! - 批准走控制面(daemon 单一写者落 state.json);**批准只授 DM 访问,
//!   永不授群访问**(上游同款 fail-closed);
//! - `allowlist`:allowFrom 显式列表;`open`:仅当列表含 `"*"` 才真公开;
//!   `disabled`:拒绝所有 DM。

use std::sync::Arc;

use latent_channel::types::InboundMessage;

use crate::config::DmPolicy;
use crate::state::{PendingPairing, StateStore};

/// 配对码长度(8 位,大写,剔除易混淆字符)。
pub const CODE_LENGTH: usize = 8;
/// 配对码字符集(剔除 0O1I)。
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// 配对码有效期。
pub const CODE_TTL_MS: u64 = 60 * 60 * 1000;
/// 每渠道账户 pending 上限。
pub const MAX_PENDING_PER_CHANNEL: usize = 3;

/// DM 放行判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmDecision {
    /// 放行(进入正常管线)
    Allow,
    /// 回复配对码(不走模型)
    PairingCode(String),
    /// 拒绝(禁言提示;disabled / 超限 / allowlist 未命中)
    Reject(&'static str),
}

pub struct PairingStore {
    state: Arc<StateStore>,
    /// 渠道 id → allowFrom(dmPolicy 判定用)
    policies: std::collections::HashMap<String, (DmPolicy, Vec<String>)>,
}

impl PairingStore {
    pub fn new(state: Arc<StateStore>) -> Self {
        PairingStore {
            state,
            policies: std::collections::HashMap::new(),
        }
    }

    /// 注册渠道 DM 策略(daemon 启动时按渠道配置注入)。
    pub fn register_policy(&mut self, channel: &str, policy: DmPolicy, allow_from: Vec<String>) {
        self.policies
            .insert(channel.to_string(), (policy, allow_from));
    }

    /// 私聊 DM 判定主入口。
    pub fn decide(&self, msg: &InboundMessage, now_ms: u64) -> DmDecision {
        let channel = msg.platform;
        let (policy, allow_from) = self
            .policies
            .get(channel)
            .cloned()
            .unwrap_or((DmPolicy::Pairing, Vec::new()));
        let user_key = format!("{channel}:{}", msg.sender.user_id);
        match policy {
            DmPolicy::Disabled => DmDecision::Reject("该渠道未开放私聊"),
            DmPolicy::Open => {
                if allow_from.iter().any(|entry| entry == "*") {
                    DmDecision::Allow
                } else {
                    // open 但列表无 "*":回退 allowlist 语义
                    self.allowlist_decision(&user_key, &allow_from)
                }
            }
            DmPolicy::Allowlist => self.allowlist_decision(&user_key, &allow_from),
            DmPolicy::Pairing => {
                if self.is_approved(channel, &msg.sender.user_id)
                    || allow_from.contains(&user_key)
                {
                    return DmDecision::Allow;
                }
                self.request_pairing(channel, &msg.sender.user_id, now_ms)
            }
        }
    }

    fn allowlist_decision(&self, user_key: &str, allow_from: &[String]) -> DmDecision {
        if allow_from.iter().any(|entry| entry == user_key || entry == "*") {
            DmDecision::Allow
        } else {
            DmDecision::Reject("你不在这个机器人的私聊白名单里")
        }
    }

    fn is_approved(&self, channel: &str, user_id: &str) -> bool {
        self.state
            .pairing()
            .approved
            .contains_key(&format!("{channel}:{user_id}"))
    }

    /// 生成配对码(pending 上限 3,超限拒绝)。
    fn request_pairing(&self, channel: &str, user_id: &str, now_ms: u64) -> DmDecision {
        let mut pairing = self.state.pairing();
        let pending = pairing.pending.entry(channel.to_string()).or_default();
        // 清过期
        pending.retain(|entry| entry.expires_at > now_ms && entry.user_id != user_id);
        if pending.len() >= MAX_PENDING_PER_CHANNEL {
            return DmDecision::Reject("配对请求过多,请稍后再试或联系 owner");
        }
        let code = generate_code();
        pending.push(PendingPairing {
            code: code.clone(),
            user_id: user_id.to_string(),
            expires_at: now_ms + CODE_TTL_MS,
        });
        self.state.set_pairing(pairing);
        if let Err(error) = self.state.save() {
            eprintln!("[latent-gateway] state.json 落盘失败: {error}");
        }
        DmDecision::PairingCode(format!(
            "配对码 {code},1 小时内有效。请机器 owner 执行 `latent-gateway pairing approve {channel} {code}` 完成配对"
        ))
    }

    /// 批准配对(daemon 内调用;返回被批准的 userId)。
    pub fn approve(&self, channel: &str, code: &str, now_ms: u64) -> Result<String, String> {
        let mut pairing = self.state.pairing();
        let pending = pairing
            .pending
            .get_mut(channel)
            .ok_or_else(|| format!("渠道 {channel} 没有未决配对请求"))?;
        let index = pending
            .iter()
            .position(|entry| entry.code.eq_ignore_ascii_case(code))
            .ok_or_else(|| format!("配对码不存在: {code}"))?;
        let entry = pending.remove(index);
        if entry.expires_at <= now_ms {
            self.state.set_pairing(pairing);
            return Err("配对码已过期".into());
        }
        pairing.approved.insert(
            format!("{channel}:{}", entry.user_id),
            crate::state::ApprovedPairing { approved_at: now_ms },
        );
        self.state.set_pairing(pairing);
        if let Err(error) = self.state.save() {
            eprintln!("[latent-gateway] state.json 落盘失败: {error}");
        }
        Ok(entry.user_id)
    }

    pub fn list_pending(&self) -> Vec<(String, Vec<PendingPairing>)> {
        self.state
            .pairing()
            .pending
            .into_iter()
            .collect()
    }
}

/// 生成 8 位配对码(大写,剔除 0O1I;系统熵源)。
pub fn generate_code() -> String {
    let mut bytes = [0u8; CODE_LENGTH];
    getrandom_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| CODE_ALPHABET[(*b as usize) % CODE_ALPHABET.len()] as char)
        .collect()
}

/// 系统熵源(避免引入 getrandom 依赖:/dev/urandom 或 RandomState 混合)。
fn getrandom_bytes(buf: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
            if file.read_exact(buf).is_ok() {
                return;
            }
        }
    }
    // 兜底:RandomState 熵(hash 迭代)
    let seed = std::collections::hash_map::RandomState::new();
    for byte in buf.iter_mut() {
        let hash = {
            use std::hash::{BuildHasher, Hasher};
            let mut hasher = seed.build_hasher();
            hasher.write_u64(0x9E3779B97F4A7C15);
            hasher.finish()
        };
        *byte = (hash & 0xFF) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateStore;
    use latent_channel::types::{ChatRef, Sender};

    fn dm(channel: &'static str, user: &str) -> InboundMessage {
        InboundMessage::plain_text(
            channel,
            ChatRef::private(channel, user),
            Sender {
                user_id: user.into(),
                display_name: user.into(),
            },
            format!("m-{user}"),
            "你好",
            true,
        )
    }

    fn store() -> PairingStore {
        PairingStore::new(Arc::new(StateStore::in_memory()))
    }

    #[test]
    fn pairing_flow_request_approve_allow() {
        let mut store = store();
        store.register_policy("telegram", DmPolicy::Pairing, vec![]);
        let now = 1_000_000;
        // 陌生人 → 配对码
        let decision = store.decide(&dm("telegram", "u1"), now);
        let DmDecision::PairingCode(text) = decision else {
            panic!("应产出配对码: {decision:?}");
        };
        let code = store
            .list_pending()
            .into_iter()
            .find(|(channel, _)| channel == "telegram")
            .unwrap()
            .1[0]
            .code
            .clone();
        assert!(text.contains(&code));
        // 批准前仍然拦截(新请求清掉同 user 旧 pending)
        // 批准 → 放行
        let user = store.approve("telegram", &code, now + 10).unwrap();
        assert_eq!(user, "u1");
        assert_eq!(store.decide(&dm("telegram", "u1"), now + 20), DmDecision::Allow);
    }

    #[test]
    fn pairing_code_expires() {
        let mut store = store();
        store.register_policy("telegram", DmPolicy::Pairing, vec![]);
        let now = 1_000_000;
        let _ = store.decide(&dm("telegram", "u1"), now);
        let code = store.list_pending()[0].1[0].code.clone();
        let error = store.approve("telegram", &code, now + CODE_TTL_MS + 1).unwrap_err();
        assert!(error.contains("过期"), "{error}");
    }

    #[test]
    fn pending_cap_is_three_per_channel() {
        let mut store = store();
        store.register_policy("telegram", DmPolicy::Pairing, vec![]);
        let now = 1_000_000;
        for user in ["u1", "u2", "u3"] {
            assert!(matches!(
                store.decide(&dm("telegram", user), now),
                DmDecision::PairingCode(_)
            ));
        }
        assert_eq!(
            store.decide(&dm("telegram", "u4"), now),
            DmDecision::Reject("配对请求过多,请稍后再试或联系 owner")
        );
    }

    #[test]
    fn policies_open_allowlist_disabled() {
        let mut store = store();
        store.register_policy("telegram", DmPolicy::Open, vec![]);
        assert!(
            matches!(store.decide(&dm("telegram", "u1"), 0), DmDecision::Reject(_)),
            "open 但列表无 * → 回退 allowlist,拒绝"
        );
        store.register_policy("telegram", DmPolicy::Open, vec!["*".into()]);
        assert_eq!(store.decide(&dm("telegram", "u1"), 0), DmDecision::Allow);

        store.register_policy("wecom", DmPolicy::Allowlist, vec!["wecom:boss".into()]);
        assert_eq!(store.decide(&dm("wecom", "boss"), 0), DmDecision::Allow);
        assert!(matches!(store.decide(&dm("wecom", "u9"), 0), DmDecision::Reject(_)));

        store.register_policy("qq", DmPolicy::Disabled, vec![]);
        assert!(matches!(store.decide(&dm("qq", "u1"), 0), DmDecision::Reject(_)));
    }

    #[test]
    fn code_alphabet_excludes_ambiguous() {
        for _ in 0..50 {
            let code = generate_code();
            assert_eq!(code.len(), CODE_LENGTH);
            assert!(
                code.chars().all(|c| CODE_ALPHABET.contains(&(c as u8))),
                "{code}"
            );
        }
    }
}
