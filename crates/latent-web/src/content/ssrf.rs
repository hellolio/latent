//! SSRF 校验(ssrf-protection.ts 的移植):
//! 协议白名单(http/https)、私网/保留段封锁、DNS 解析断言公网、
//! `ssrf.allowRanges` 豁免(如 TUN/fake-IP 的 198.18.0.0/15)、
//! fetch_content 的域策略(allow/deny)。重定向的逐跳重校验在 http.rs
//! 之上由 extract.rs 组合实现。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use regex::Regex;

use crate::config::{FetchDomainPolicy, SsrfSettings};

/// 校验通过后返回的规范化 URL。
pub async fn validate_remote_url(
    raw_url: &str,
    ssrf: &SsrfSettings,
    domain_policy: &FetchDomainPolicy,
) -> Result<url::Url, String> {
    let parsed = url::Url::parse(raw_url).map_err(|error| error.to_string())?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Only HTTP and HTTPS URLs can be fetched remotely".to_string());
    }
    let host = parsed.host_str().ok_or("URL must include a hostname")?;
    let hostname = normalize_hostname(host);
    if hostname.is_empty() {
        return Err("URL must include a hostname".to_string());
    }
    if hostname == "localhost" || hostname.ends_with(".localhost") {
        return Err(format!("Blocked internal hostname: {hostname}"));
    }
    assert_domain_policy(&hostname, domain_policy)?;
    let allow_ranges = parse_allow_ranges(&ssrf.allow_ranges)?;

    // 裸 IP:直接断言公网
    if let Ok(ip) = hostname.parse::<IpAddr>() {
        assert_public_address(&ip, &hostname, &allow_ranges)?;
        return Ok(parsed);
    }
    // 域名:本地解析,每个地址都必须公网(SSRF 预检;真正请求由 reqwest 解析,
    // 可能命中不同地址 —— 与上游一致,预检 + 每跳重校验是纵深而非完备)
    let addresses = resolve_host(&hostname).await?;
    if addresses.is_empty() {
        return Err(format!("Failed to resolve {hostname}: no addresses returned"));
    }
    for address in &addresses {
        assert_public_address(address, &hostname, &allow_ranges)?;
    }
    Ok(parsed)
}

fn normalize_hostname(hostname: &str) -> String {
    hostname
        .trim()
        .to_lowercase()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_string()
}

fn assert_domain_policy(
    hostname: &str,
    policy: &FetchDomainPolicy,
) -> Result<(), String> {
    if policy
        .deny
        .iter()
        .any(|entry| domain_matches(hostname, entry))
    {
        return Err(format!(
            "Blocked hostname by fetch_content domain policy: {hostname}"
        ));
    }
    if !policy.allow.is_empty()
        && !policy
            .allow
            .iter()
            .any(|entry| domain_matches(hostname, entry))
    {
        return Err(format!(
            "Hostname not allowed by fetch_content domain policy: {hostname}"
        ));
    }
    Ok(())
}

fn domain_matches(hostname: &str, entry: &str) -> bool {
    hostname == entry || hostname.ends_with(&format!(".{entry}"))
}

/// 本地 DNS 解析(阻塞调用包进 spawn_blocking)。
pub async fn resolve_host(hostname: &str) -> Result<Vec<IpAddr>, String> {
    use std::net::ToSocketAddrs;
    let hostname = hostname.to_string();
    let runtime_handle = tokio::task::spawn_blocking(move || {
        (hostname.as_str(), 0u16)
            .to_socket_addrs()
            .map(|addrs| addrs.map(|addr| addr.ip()).collect::<Vec<IpAddr>>())
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?;
    runtime_handle
}

pub fn assert_public_address(
    address: &IpAddr,
    hostname: &str,
    allow_ranges: &[Cidr],
) -> Result<(), String> {
    if is_in_allowed_range(address, allow_ranges) {
        return Ok(());
    }
    let blocked = match address {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    };
    if blocked {
        let hint = match address {
            IpAddr::V4(v4) if is_fake_ip_proxy_address(v4) => ". This address is in 198.18.0.0/15, commonly used by TUN/fake-IP proxies. If that matches your setup, configure ssrf.allowRanges with [\"198.18.0.0/15\"] in web-search.json.".to_string(),
            _ => String::new(),
        };
        return Err(format!("Blocked internal address for {hostname}: {address}{hint}"));
    }
    Ok(())
}

fn is_fake_ip_proxy_address(address: &Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 198 && (octets[1] == 18 || octets[1] == 19)
}

fn is_blocked_ipv4(address: &Ipv4Addr) -> bool {
    let [a, b, _, _] = address.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || is_fake_ip_proxy_address(address)
        || a >= 224
}

fn is_blocked_ipv6(address: &Ipv6Addr) -> bool {
    let segments = address.segments();
    if segments.iter().all(|segment| *segment == 0) {
        return true; // ::
    }
    if segments[..7].iter().all(|segment| *segment == 0) && segments[7] == 1 {
        return true; // ::1
    }
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true; // ULA fc00::/7
    }
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true; // link-local fe80::/10
    }
    // IPv4-mapped ::ffff:a.b.c.d
    let is_mapped = segments[..5].iter().all(|segment| *segment == 0)
        && segments[5] == 0xffff;
    if is_mapped {
        let v4 = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            (segments[6] & 0xff) as u8,
            (segments[7] >> 8) as u8,
            (segments[7] & 0xff) as u8,
        );
        return is_blocked_ipv4(&v4);
    }
    false
}

/// CIDR 规则(网络地址 + 前缀长度)。
#[derive(Debug, Clone, PartialEq)]
pub struct Cidr {
    bytes: Vec<u8>,
    prefix: u8,
}

/// 解析 allowRanges(畸形条目报错,不静默 —— 上游条款)。
pub fn parse_allow_ranges(input: &[String]) -> Result<Vec<Cidr>, String> {
    let mut rules = Vec::new();
    for entry in input {
        let rule = parse_cidr(entry.trim()).ok_or_else(|| {
            format!("Invalid CIDR notation in ssrf.allowRanges: \"{entry}\"")
        })?;
        rules.push(rule);
    }
    Ok(rules)
}

fn parse_cidr(raw: &str) -> Option<Cidr> {
    if raw.is_empty() {
        return None;
    }
    let (addr_part, prefix_part) = match raw.rsplit_once('/') {
        Some((addr, prefix)) => {
            if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            (addr, Some(prefix.parse::<u8>().ok()?))
        }
        None => (raw, None),
    };
    if let Ok(v4) = addr_part.parse::<Ipv4Addr>() {
        let prefix = prefix_part.unwrap_or(32);
        if !(1..=32).contains(&prefix) {
            return None;
        }
        return Some(Cidr {
            bytes: v4.octets().to_vec(),
            prefix,
        });
    }
    if let Ok(v6) = addr_part.parse::<Ipv6Addr>() {
        let prefix = prefix_part.unwrap_or(128);
        if !(1..=128).contains(&prefix) {
            return None;
        }
        return Some(Cidr {
            bytes: v6.octets().to_vec(),
            prefix,
        });
    }
    None
}

fn is_in_allowed_range(address: &IpAddr, allow_ranges: &[Cidr]) -> bool {
    if allow_ranges.is_empty() {
        return false;
    }
    let addr_bytes: Vec<u8> = match address {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    allow_ranges.iter().any(|rule| {
        rule.bytes.len() == addr_bytes.len() && bytes_match_prefix(&addr_bytes, &rule.bytes, rule.prefix)
    })
}

fn bytes_match_prefix(addr: &[u8], network: &[u8], prefix: u8) -> bool {
    let full_bytes = prefix as usize / 8;
    let remainder = prefix as usize % 8;
    if full_bytes > addr.len() || full_bytes > network.len() {
        return false;
    }
    if addr[..full_bytes] != network[..full_bytes] {
        return false;
    }
    if remainder > 0 && full_bytes < addr.len() {
        let mask = (0xffu16 << (8 - remainder)) as u8;
        if (addr[full_bytes] & mask) != (network[full_bytes] & mask) {
            return false;
        }
    }
    true
}

/// fetch_content 域策略条目校验(与 ssrf-protection.ts normalizeDomainEntry 一致)。
pub fn normalize_domain_entry(entry: &str) -> Option<String> {
    static HOSTNAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)*[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
            .expect("static regex")
    });
    let hostname = normalize_hostname(entry);
    if hostname.is_empty()
        || hostname.chars().any(|c| {
            c.is_whitespace() || "\\/?:#@".contains(c)
        })
    {
        return None;
    }
    if hostname.parse::<IpAddr>().is_ok() {
        return Some(hostname);
    }
    if hostname.len() > 253 || !HOSTNAME.is_match(&hostname) {
        return None;
    }
    Some(hostname)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn validate(url: &str) -> Result<(), String> {
        validate_remote_url(url, &SsrfSettings::default(), &FetchDomainPolicy::default())
            .await
            .map(|_| ())
    }

    #[tokio::test]
    async fn blocks_internal_targets() {
        assert!(validate("http://localhost/x").await.is_err());
        assert!(validate("http://sub.localhost/").await.is_err());
        assert!(validate("http://127.0.0.1/").await.is_err());
        assert!(validate("http://10.0.0.1/").await.is_err());
        assert!(validate("http://192.168.1.1/").await.is_err());
        assert!(validate("http://172.16.0.1/").await.is_err());
        assert!(validate("http://169.254.1.1/").await.is_err());
        assert!(validate("http://[::1]/").await.is_err());
        assert!(validate("http://[fe80::1]/").await.is_err());
        assert!(validate("http://[::ffff:127.0.0.1]/").await.is_err());
        assert!(validate("http://198.18.0.1/").await.is_err());
        assert!(validate("ftp://example.com/").await.is_err());
    }

    #[tokio::test]
    async fn allows_public_targets() {
        // 不联网的静态断言只覆盖裸公网 IP(域名走 DNS,留给手工验证)
        assert!(validate("https://1.1.1.1/").await.is_ok());
        assert!(validate("https://8.8.8.8:443/dns-query").await.is_ok());
    }

    #[tokio::test]
    async fn allow_ranges_exempt() {
        let ssrf = SsrfSettings {
            allow_ranges: vec!["198.18.0.0/15".to_string()],
        };
        assert!(validate_remote_url(
            "http://198.18.0.1/",
            &ssrf,
            &FetchDomainPolicy::default()
        )
        .await
        .is_ok());
        // 畸形条目显式报错
        let bad = SsrfSettings {
            allow_ranges: vec!["198.18.0.0/".to_string()],
        };
        assert!(parse_allow_ranges(&bad.allow_ranges).is_err());
    }

    #[tokio::test]
    async fn domain_policy() {
        let policy = FetchDomainPolicy {
            allow: vec!["example.com".to_string()],
            deny: vec!["evil.example.com".to_string()],
        };
        assert!(validate_remote_url("https://example.com/x", &SsrfSettings::default(), &policy).await.is_ok());
        assert!(validate_remote_url("https://other.com/x", &SsrfSettings::default(), &policy).await.is_err());
        assert!(validate_remote_url("https://sub.evil.example.com/x", &SsrfSettings::default(), &policy).await.is_err());
    }

    #[test]
    fn cidr_prefix_matching() {
        let rules = parse_allow_ranges(&["10.0.0.0/8".to_string(), "fd00::/8".to_string()]).unwrap();
        assert!(is_in_allowed_range(&"10.1.2.3".parse().unwrap(), &rules));
        assert!(!is_in_allowed_range(&"11.0.0.1".parse().unwrap(), &rules));
        assert!(is_in_allowed_range(&"fd12::1".parse().unwrap(), &rules));
        assert!(!is_in_allowed_range(&"fe80::1".parse().unwrap(), &rules));
    }
}
