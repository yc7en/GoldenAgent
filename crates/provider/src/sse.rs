//! 最小 SSE 解码器。
//!
//! 只做一件事：把字节流切成一个个 `event` / `data` 帧。
//! **不解释语义**——`text_delta` 与 `input_json_delta` 的含义属于各家协议，
//! 放在 `anthropic.rs` / `openai.rs` 里认。
//!
//! 按字节而不是按字符串切分，是因为一个 TCP 分片可能正好落在多字节字符中间。
//! UTF-8 的续字节永远 ≥ 0x80，不会等于 `\n`，所以按字节找分隔符不会把字符切坏。

use std::collections::VecDeque;

use goldenagent_core::{Error, Result};

/// 一个完整的 SSE 帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// `event:` 字段。OpenAI 兼容接口不用它，Anthropic 全靠它区分事件类型。
    pub event: Option<String>,
    /// `data:` 字段。多行 `data:` 按 SSE 规范用 `\n` 拼接。
    pub data: String,
}

#[derive(Default)]
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段字节，返回其中已经完整的帧（可能是 0 个或多个）。
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);

        let mut out = Vec::new();
        while let Some((end, consume)) = delimiter(&self.buf) {
            let frame = self.buf[..end].to_vec();
            self.buf.drain(..end + consume);
            if let Some(event) = parse_frame(&frame) {
                out.push(event);
            }
        }
        out
    }

    /// 流结束时收尾：服务端可能没发最后那个空行。
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buf.is_empty() {
            return None;
        }
        let frame = std::mem::take(&mut self.buf);
        parse_frame(&frame)
    }
}

/// 找帧分隔符：`\n\n` 或 `\r\n\r\n`。返回（起始位置，分隔符长度）。
fn delimiter(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2));
    // "\r\n\r\n" 里不含 "\n\n"（字节是 0d 0a 0d 0a），所以两种要分别找。
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| (p, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

fn parse_frame(frame: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(frame);

    let mut event = None;
    let mut data: Vec<&str> = Vec::new();

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        // 空行和注释行（`: ping`）都不是数据。很多网关靠注释行保活。
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }

    if data.is_empty() && event.is_none() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data.join("\n"),
    })
}

/// 把 `reqwest` 的响应体变成一串 [`SseEvent`]。
pub(crate) struct SseStream {
    response: reqwest::Response,
    decoder: SseDecoder,
    pending: VecDeque<SseEvent>,
    ended: bool,
}

impl SseStream {
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            decoder: SseDecoder::new(),
            pending: VecDeque::new(),
            ended: false,
        }
    }

    pub async fn next(&mut self) -> Result<Option<SseEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if self.ended {
                return Ok(self.decoder.finish());
            }

            match self.response.chunk().await {
                Ok(Some(bytes)) => {
                    self.pending.extend(self.decoder.push(&bytes));
                }
                Ok(None) => self.ended = true,
                Err(error) => {
                    return Err(Error::Provider(format!(
                        "流式响应读取中断：{error}。多半是网络断开，或连接被中间设备掐掉。"
                    )))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(event: &SseEvent) -> &str {
        &event.data
    }

    #[test]
    fn splits_frames_and_reads_named_events() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(
            b"event: content_block_delta\ndata: {\"a\":1}\n\nevent: message_stop\ndata: {}\n\n",
        );

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("content_block_delta"));
        assert_eq!(data(&events[0]), "{\"a\":1}");
        assert_eq!(events[1].event.as_deref(), Some("message_stop"));
    }

    /// 一个数据帧被 TCP 切成两半，是最容易出问题的地方。
    #[test]
    fn reassembles_frame_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: {\"tex").is_empty());
        let events = decoder.push(b"t\":\"hi\"}\n\n");

        assert_eq!(events.len(), 1);
        assert_eq!(data(&events[0]), "{\"text\":\"hi\"}");
    }

    /// 多字节字符被切开也不能崩——按字节切分正是为了这个。
    #[test]
    fn survives_multibyte_char_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        let full = "data: 你好\n\n".as_bytes().to_vec();
        // 在"你"的三个字节中间切开
        let (head, tail) = full.split_at(7);
        assert!(decoder.push(head).is_empty());
        let events = decoder.push(tail);
        assert_eq!(events.len(), 1);
        assert_eq!(data(&events[0]), "你好");
    }

    #[test]
    fn handles_crlf_frames() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b"event: ping\r\ndata: x\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(data(&events[0]), "x");
    }

    /// 保活注释行不该变成一个空事件。
    #[test]
    fn comment_only_frame_is_skipped() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b": keep-alive\n\n").is_empty());
    }

    /// 多行 data 按规范用换行拼接。
    #[test]
    fn joins_multiline_data() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(data(&events[0]), "line1\nline2");
    }

    #[test]
    fn finish_flushes_unterminated_tail() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: [DONE]").is_empty());
        let tail = decoder.finish().expect("尾帧要能被收走");
        assert_eq!(tail.data, "[DONE]");
    }
}
