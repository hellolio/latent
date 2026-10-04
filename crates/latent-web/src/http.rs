//! 出网 HTTP 公共层:超时 + 取消令牌 + 手动重定向(跨域剥敏感头)。
//!
//! provider 的 API key / fetch 的凭证头不允许被重定向带到别的 origin
//! (上游 utils.ts fetchWithCredentialRedirects 语义);reqwest 关自动重定向,
//! 在这里逐跳处理。

use std::time::Duration;

use tokio_util::sync::CancellationToken;

const MAX_REDIRECTS: usize = 5;
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

/// 一次出网请求的公共参数。
#[derive(Debug, Clone)]
pub struct HttpRequestOptions<'a> {
    /// 逐次代理覆盖(http/https/socks5h);None = 不走代理
    pub proxy: Option<&'a str>,
    pub timeout: Duration,
    pub cancel: Option<&'a CancellationToken>,
    /// 跨域重定向时要剥掉的请求头(小写比较),如 ["x-api-key", "authorization"]
    pub sensitive_headers: &'a [&'a str],
}

pub struct HttpTextResponse {
    pub status: u16,
    pub url: String,
    pub body: String,
    pub content_type: Option<String>,
    /// 3xx 时的 Location 头(仅单跳原语填充)
    pub location: Option<String>,
}

/// 单跳请求(不跟重定向;extract 的逐跳 SSRF 校验用)。
pub async fn send_single_hop(
    method: reqwest::Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
    options: &HttpRequestOptions<'_>,
) -> Result<HttpTextResponse, String> {
    let client = build_client(options.proxy, options.timeout)?;
    let mut request = client
        .request(method, url)
        .headers(convert_headers(headers)?);
    if let Some(body) = &body {
        request = request.body(body.clone());
    }
    let response = send_with_cancel(request, options.cancel).await?;
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let response_headers = response.headers().clone();
    let location = response_headers
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let content_type = response_headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_string());
    let body = response.text().await.map_err(|error| error.to_string())?;
    Ok(HttpTextResponse {
        status,
        url: final_url,
        body,
        content_type,
        location,
    })
}

fn build_client(proxy: Option<&str>, timeout: Duration) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (compatible; latent-web/1.0; +https://github.com/hellolio/latent)");
    if let Some(proxy) = proxy {
        // reqwest 的 socks 特性原生支持 socks5h:// scheme
        let parsed = reqwest::Proxy::all(proxy.trim())
            .map_err(|error| format!("invalid proxy URL `{proxy}`: {error}"))?;
        builder = builder.proxy(parsed);
    }
    builder.build().map_err(|error| format!("failed to build HTTP client: {error}"))
}

/// 判断两个 URL 是否同 origin(scheme://host[:port])。
fn same_origin(left: &str, right: &str) -> bool {
    let origin = |url: &str| -> Option<String> {
        let (scheme, rest) = url.split_once("://")?;
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let mut authority = rest[..authority_end].to_lowercase();
        // 默认端口归一化(https:443 / http:80)
        let default_port = if scheme == "https" { "443" } else if scheme == "http" { "80" } else { "" };
        if !default_port.is_empty() {
            if let Some(host) = authority.strip_suffix(&format!(":{default_port}")) {
                authority = host.to_string();
            }
        }
        Some(format!("{scheme}://{authority}"))
    };
    match (origin(left), origin(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// GET/POST,手动跟重定向:每一跳重放请求,跨域时剥掉敏感头;
/// 303 或(301/302 且原方法 POST)降级为 GET 并丢 body。
pub async fn send_with_redirects(
    method: reqwest::Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
    options: &HttpRequestOptions<'_>,
) -> Result<HttpTextResponse, String> {
    let mut current_url = url.to_string();
    let mut current_method = method;
    let mut current_headers: Vec<(String, String)> = headers.to_vec();
    let mut current_body = body;

    for redirects in 0..=MAX_REDIRECTS {
        let response = send_single_hop(
            current_method.clone(),
            &current_url,
            &current_headers,
            current_body.clone(),
            options,
        )
        .await?;
        let is_redirect = REDIRECT_STATUSES.contains(&response.status);
        if !is_redirect || response.location.is_none() {
            return Ok(response);
        }
        if redirects == MAX_REDIRECTS {
            return Err(format!("Too many redirects fetching {current_url}"));
        }
        let location = response.location.as_deref().expect("checked above");
        let next_url = resolve_redirect_target(&current_url, location)?;
        // 跨域:剥敏感头(重定向目标不得带上一跳的凭据)
        if !same_origin(&current_url, &next_url) {
            current_headers.retain(|(name, _)| {
                let name = name.to_lowercase();
                !options
                    .sensitive_headers
                    .iter()
                    .any(|sensitive| *sensitive == name)
            });
        }
        if response.status == 303
            || ((response.status == 301 || response.status == 302)
                && current_method == reqwest::Method::POST)
        {
            current_method = reqwest::Method::GET;
            current_body = None;
        }
        current_url = next_url;
    }
    unreachable!("redirect loop bounded above")
}

async fn send_with_cancel(
    request: reqwest::RequestBuilder,
    cancel: Option<&CancellationToken>,
) -> Result<reqwest::Response, String> {
    let future = request.send();
    match cancel {
        Some(cancel) => tokio::select! {
            result = future => result.map_err(|error| error.to_string()),
            _ = cancel.cancelled() => Err("Aborted".to_string()),
        },
        None => future.await.map_err(|error| error.to_string()),
    }
}

fn convert_headers(headers: &[(String, String)]) -> Result<reqwest::header::HeaderMap, String> {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("invalid header name `{name}`: {error}"))?;
        let header_value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|error| format!("invalid header value for `{name}`: {error}"))?;
        map.append(header_name, header_value);
    }
    Ok(map)
}

/// 相对 Location 解析(与上游 `new URL(location, current)` 对齐的最小子集)。
pub fn resolve_redirect_target(current: &str, location: &str) -> Result<String, String> {
    if location.contains("://") {
        return Ok(location.to_string());
    }
    if let Some(rest) = location.strip_prefix("//") {
        let scheme = current.split("://").next().ok_or("invalid current URL")?;
        return Ok(format!("{scheme}://{rest}"));
    }
    let (scheme, rest) = current.split_once("://").ok_or("invalid current URL")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if location.starts_with('/') {
        return Ok(format!("{scheme}://{authority}{location}"));
    }
    // 相对路径:取当前路径(authority 之后)的目录
    let path = rest[authority_end..]
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    let dir = match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "/",
    };
    Ok(format!("{scheme}://{authority}{dir}{location}"))
}

/// provider 错误文案(与上游一致:`"{label} ... error {status}: {body 摘要}"`)。
/// body 是 HTML 页面时(反爬拦截页等)先做噪声清理:`{status} 前缀保留原样,
/// 错误分类正则依赖它。
pub fn http_error_message(label: &str, status: u16, body: &str) -> String {
    format!("{label} search error {status}: {}", clean_error_body(body, ERROR_BODY_CAP))
}

const ERROR_BODY_CAP: usize = 300;

/// 错误 body 摘要:非 HTML 原样截断;HTML 页面提取 `<title>`(反爬页唯一
/// 有信息量的部分),无 title 再做剥标签兜底;清理后为空则不附 body。
pub fn clean_error_body(body: &str, cap: usize) -> String {
    let trimmed = body.trim();
    if !looks_like_html(trimmed) {
        return char_truncate(trimmed, cap);
    }
    if let Some(title) = html_title(trimmed) {
        return format!("HTML error page: \"{title}\"");
    }
    let text = collapse_whitespace(&decode_entities(&strip_tags(&strip_html_blocks(
        trimmed,
    ))));
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        char_truncate(text, cap)
    }
}

fn looks_like_html(body: &str) -> bool {
    let head = &body[..body.len().min(512)].to_lowercase();
    ["<!doctype", "<html", "<head", "<body"]
        .iter()
        .any(|marker| head.starts_with(marker) || head.contains(marker))
}

/// 首个 `<title>` 文本,解码实体并压空白;超长截断。
fn html_title(body: &str) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
    re.captures(body)
        .and_then(|captures| captures.get(1))
        .map(|title| {
            let text = collapse_whitespace(&decode_entities(title.as_str()));
            char_truncate(text.trim(), 120)
        })
        .filter(|title| !title.is_empty())
}

/// 整块删除 script/style/注释(标签内文本不属于错误信息)。
fn strip_html_blocks(body: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?is)<script\b.*?</script>|<style\b.*?</style>|<!--.*?-->").unwrap()
    });
    re.replace_all(body, " ").into_owned()
}

/// 逐字符剥标签:引号内的 `>` 不结束标签(属性值里可能含 `>`)。
fn strip_tags(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(c);
            continue;
        }
        let mut quote: Option<char> = None;
        for c in chars.by_ref() {
            match quote {
                Some(q) if c == q => quote = None,
                None if c == '"' || c == '\'' => quote = Some(c),
                None if c == '>' => break,
                _ => {}
            }
        }
        out.push(' ');
    }
    out
}

fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let Some(semicolon) = tail.find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semicolon];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            digits if digits.starts_with("#x") || digits.starts_with("#X") => {
                u32::from_str_radix(&digits[2..], 16).ok().and_then(char::from_u32)
            }
            digits if digits.starts_with('#') => {
                digits[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semicolon + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn char_truncate(input: &str, cap: usize) -> String {
    if input.chars().count() > cap {
        let mut snippet: String = input.chars().take(cap).collect();
        snippet.push_str("...");
        snippet
    } else {
        input.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_target_resolution() {
        assert_eq!(
            resolve_redirect_target("https://a.com/x/y?z=1", "https://b.com/p").unwrap(),
            "https://b.com/p"
        );
        assert_eq!(
            resolve_redirect_target("https://a.com/x/y", "//cdn.a.com/s.js").unwrap(),
            "https://cdn.a.com/s.js"
        );
        assert_eq!(
            resolve_redirect_target("https://a.com/x/y", "/root").unwrap(),
            "https://a.com/root"
        );
        assert_eq!(
            resolve_redirect_target("https://a.com/x/y", "z").unwrap(),
            "https://a.com/x/z"
        );
    }

    #[test]
    fn origin_comparison() {
        assert!(same_origin(
            "https://a.com/x",
            "https://a.com:443/y"
        ));
        assert!(!same_origin("https://a.com", "http://a.com"));
    }

    #[test]
    fn plain_body_passes_through() {
        assert_eq!(
            clean_error_body("rate limited", 300),
            "rate limited"
        );
        let long = "a".repeat(400);
        assert_eq!(clean_error_body(&long, 300), format!("{}...", "a".repeat(300)));
    }

    #[test]
    fn html_body_uses_title() {
        let body = "<!DOCTYPE HTML>\n<html><head><title>Attention Required! | Cloudflare</title></head><body>lots of noise</body></html>";
        assert_eq!(
            clean_error_body(body, 300),
            "HTML error page: \"Attention Required! | Cloudflare\""
        );
    }

    #[test]
    fn html_without_title_strips_tags() {
        let body = "<html><body><h1>Blocked</h1><p>You are being &amp;#160;filtered</p><script>evil()</script></body></html>";
        assert_eq!(
            clean_error_body(body, 300),
            "Blocked You are being &#160;filtered"
        );
    }

    #[test]
    fn numeric_entities_decode() {
        assert_eq!(decode_entities("&#65;&#x42;&quot;"), "AB\"");
    }

    #[test]
    fn strip_tags_respects_quoted_gt() {
        assert_eq!(
            strip_tags("<a title=\"a > b\" href=/x>t</a>"),
            " t "
        );
    }
}
