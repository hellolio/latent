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
        .user_agent("Mozilla/5.0 (compatible; rpi-web/1.0; +https://github.com/nicobailon/pi-web-access)");
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

/// provider 错误文案(与上游一致:`"{label} ... error {status}: {body 前 300 字符}"`)。
pub fn http_error_message(label: &str, status: u16, body: &str) -> String {
    let mut snippet: String = body.chars().take(300).collect();
    if body.chars().count() > 300 {
        snippet.push_str("...");
    }
    format!("{label} search error {status}: {snippet}")
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
}
