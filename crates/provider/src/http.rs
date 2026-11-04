//! HTTP 传输与错误分类。
//!
//! 只做一件事：POST 一个 JSON、拿回一个 JSON，把"厂商怎么坏掉的"翻译成
//! 人能照着修的提示。**重试放在这里**，不放在循环层——循环不应该知道 429 是什么。

use std::time::Duration;

use goldenagent_core::{Error, Result};
use reqwest::header::HeaderMap;
use reqwest::StatusCode;

pub(crate) struct HttpClient {
    client: reqwest::Client,
    max_retries: u32,
}

impl HttpClient {
    pub fn new(timeout_secs: u64, max_retries: u32) -> Result<Self> {
        let client = reqwest::Client::builder()
            // 低于 10 秒基本没法用：LLM 首字节就可能要几秒。
            .timeout(Duration::from_secs(timeout_secs.max(10)))
            .user_agent(concat!("GoldenAgent/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Provider(format!("创建 HTTP 客户端失败: {e}")))?;
        Ok(Self {
            client,
            max_retries,
        })
    }

    pub async fn post_json(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let response = self.send(url, headers, body).await?;
        let text = response.text().await.unwrap_or_default();
        serde_json::from_str(&text).map_err(|e| {
            Error::Provider(format!(
                "{url} 返回的不是合法 JSON（{e}）。响应片段：{}",
                excerpt(&text)
            ))
        })
    }

    /// 流式请求：把还没读的响应体交出去，由调用方逐块消费。
    ///
    /// **重试只发生在"还没开始读"之前**：状态码判定、连接建立都在这一步完成。
    /// 一旦把响应交出去就不再重试——否则会把已经推给前端的文本再推一遍。
    pub async fn post_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        self.send(url, headers, body).await
    }

    async fn send(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let mut attempt: u32 = 0;

        loop {
            let sent = self
                .client
                .post(url)
                .headers(headers.clone())
                .json(body)
                .send()
                .await;

            match sent {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(response);
                    }

                    let text = response.text().await.unwrap_or_default();

                    if retryable(status) && attempt < self.max_retries {
                        attempt += 1;
                        backoff(attempt).await;
                        continue;
                    }

                    return Err(classify_status(url, status, &text, attempt));
                }
                Err(err) => {
                    // 只重试"可能是抖动"的：超时和连接失败。
                    // 构造错误、编码错误重试多少次都一样。
                    let transient = err.is_timeout() || err.is_connect();
                    if transient && attempt < self.max_retries {
                        attempt += 1;
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(classify_transport(url, &err, attempt));
                }
            }
        }
    }
}

/// 响应体是不是 SSE。
///
/// 用得上是因为**有些网关会无视 `stream: true` 直接回一整段 JSON**，
/// 那时候按 SSE 解析只会得到"流里一个字都没有"，报错跟真实原因差得远。
pub(crate) fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains("text/event-stream"))
        .unwrap_or(false)
}

/// 529 是 Anthropic 自己的"过载"状态码，不在标准里但真实存在。
fn retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529)
}

async fn backoff(attempt: u32) {
    // 300ms → 600ms → 1.2s → 2.4s，封顶 5s。次数上限由调用方控制。
    let exponent = attempt.saturating_sub(1).min(4);
    let ms = 300u64 * 2u64.saturating_pow(exponent);
    tokio::time::sleep(Duration::from_millis(ms.min(5_000))).await;
}

fn classify_status(url: &str, status: StatusCode, body: &str, attempts: u32) -> Error {
    let retried = if attempts > 0 {
        format!("（已重试 {attempts} 次）")
    } else {
        String::new()
    };

    let hint = match status.as_u16() {
        401 | 403 => "API key 无效或被拒绝。检查 .goldenagent/provider.json 的 api_key，\
                      或环境变量 ANTHROPIC_API_KEY / OPENAI_API_KEY。",
        404 => "接口路径不存在。多半是 base_url 写错：Anthropic 填到域名为止，\
                OpenAI 兼容要带上 /v1。",
        429 => "厂商限流。稍后再试，或调小 max_tokens、换更小的模型。",
        400 | 422 => "请求被拒绝。常见原因：模型名不存在、max_tokens 超过该模型上限、\
                      消息里出现了该模型不接受的块类型。",
        500..=599 => "厂商侧服务异常。若你设置了 HTTP_PROXY，也可能是代理本身没起来。稍后重试。",
        _ => "请求失败。",
    };

    Error::Provider(format!(
        "{url} 返回 {status}{retried}。{hint}{}",
        extract_error(body)
    ))
}

fn classify_transport(url: &str, err: &reqwest::Error, attempts: u32) -> Error {
    let retried = if attempts > 0 {
        format!("（已重试 {attempts} 次）")
    } else {
        String::new()
    };

    if err.is_timeout() {
        return Error::Provider(format!(
            "请求超时{retried}：{url}。模型长回答可能超过 timeout_secs，可在 provider.json 调大。"
        ));
    }
    if err.is_connect() {
        return Error::Provider(format!(
            "无法连接{retried}：{url}。检查网络/代理，以及 base_url 是否写对。"
        ));
    }
    Error::Provider(format!("传输失败{retried}：{url}：{err}"))
}

/// 从错误响应体里挖出厂商那句人话。挖不到就退回原始片段。
fn extract_error(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        for pointer in ["/error/message", "/error", "/message"] {
            if let Some(text) = value.pointer(pointer).and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    return format!(" 厂商说明：{text}");
                }
            }
        }
    }
    format!(" 响应片段：{}", excerpt(trimmed))
}

/// 截断到字符边界，避免把一整页 HTML 错误页塞进事件流。
pub(crate) fn excerpt(text: &str) -> String {
    const LIMIT: usize = 400;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let head: String = text.chars().take(LIMIT).collect();
    format!("{head}…（已截断）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_vendor_message_from_anthropic_shape() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert!(extract_error(body).contains("invalid x-api-key"));
    }

    #[test]
    fn extracts_vendor_message_from_openai_shape() {
        let body = r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#;
        assert!(extract_error(body).contains("Incorrect API key provided"));
    }

    #[test]
    fn falls_back_to_excerpt_for_non_json() {
        assert!(extract_error("<html>502 Bad Gateway</html>").contains("502 Bad Gateway"));
    }

    #[test]
    fn excerpt_does_not_split_multibyte_chars() {
        let text = "错".repeat(500);
        let cut = excerpt(&text);
        assert!(cut.ends_with("…（已截断）"));
        // 能把它拼回去说明没切坏 UTF-8
        assert_eq!(cut.trim_end_matches("…（已截断）").chars().count(), 400);
    }
}
