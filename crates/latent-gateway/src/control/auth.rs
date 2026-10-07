//! 控制面认证(§5.3/§5.4):token 常数时间比较;auth/握手失败按来源指数
//! 退避(连续 5 次失败 → 60s 冷却)。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 连续失败进入冷却的阈值。
pub const MAX_CONSECUTIVE_AUTH_FAILURES: u32 = 5;
/// 冷却时长。
pub const AUTH_COOLDOWN: Duration = Duration::from_secs(60);

/// 常数时间字符串比较(长度不同也走满轮比较,防时序侧信道)。
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = (a.len() ^ b.len()) as u8;
    for index in 0..max {
        diff |= a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0);
    }
    diff == 0
}

/// 按来源 IP 的认证失败退避表。
#[derive(Default)]
pub struct AuthThrottle {
    failures: Mutex<HashMap<IpAddr, (u32, Option<Instant>)>>,
}

impl AuthThrottle {
    pub fn new() -> Self {
        AuthThrottle::default()
    }

    /// 该来源是否在冷却中。
    pub fn is_cooled_down(&self, peer: IpAddr) -> bool {
        let failures = self.failures.lock().unwrap();
        match failures.get(&peer) {
            Some((_, Some(until))) => *until > Instant::now(),
            _ => false,
        }
    }

    /// 记录一次失败(连续 5 次 → 60s 冷却)。
    pub fn record_failure(&self, peer: IpAddr) {
        let mut failures = self.failures.lock().unwrap();
        let entry = failures.entry(peer).or_insert((0, None));
        entry.0 += 1;
        if entry.0 >= MAX_CONSECUTIVE_AUTH_FAILURES {
            entry.1 = Some(Instant::now() + AUTH_COOLDOWN);
            entry.0 = 0;
        }
    }

    /// 认证成功:清零。
    pub fn record_success(&self, peer: IpAddr) {
        self.failures.lock().unwrap().remove(&peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn ip(value: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, value])
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
        assert!(!constant_time_eq("secret", "secret1"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn cooldown_after_five_consecutive_failures() {
        let throttle = AuthThrottle::new();
        let peer = ip(1);
        assert!(!throttle.is_cooled_down(peer));
        for _ in 0..4 {
            throttle.record_failure(peer);
            assert!(!throttle.is_cooled_down(peer), "4 次以内不冷却");
        }
        throttle.record_failure(peer);
        assert!(throttle.is_cooled_down(peer), "第 5 次失败进入冷却");
        // 成功清零
        throttle.record_success(peer);
        assert!(!throttle.is_cooled_down(peer));
    }

    #[test]
    fn per_peer_isolation() {
        let throttle = AuthThrottle::new();
        for _ in 0..5 {
            throttle.record_failure(ip(2));
        }
        assert!(throttle.is_cooled_down(ip(2)));
        assert!(!throttle.is_cooled_down(ip(3)));
    }
}
