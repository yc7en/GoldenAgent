//! Anthropic Messages API（`POST /v1/messages`）。
//!
//! 协议要点，全部在这里消化，不外溢到循环层：
//!
//! - `system` 是顶层字段，不在 `messages` 里；
//! - 工具调用是 `assistant` 消息里的 `tool_use` 块，工具结果是 `user` 消息里的 `tool_result` 块；
//! - `input_schema` 之外多一个字段（我们的 `ToolSpec.kind`）会被 400 拒绝，所以工具声明手写；
//! - 新模型会返回 `thinking` / `redacted_thinking` 块，解析时要跳过而不是报错。

use async_trait::async_trait;
use goldenagent_core::{ContentBlock, Error, Message, Result, Role, StopReason, ToolCall, ToolSpec};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::config::ProviderConfig;
use crate::http::{excerpt, is_event_stream, HttpClient};
use crate::sse::{SseEvent, SseStream};
use crate::{pump_text, Completion, CompletionRequest, Provider, StreamChunk};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub(crate) struct AnthropicProvider {
    endpoint: String,
    headers: HeaderMap,
    http: HttpClient,
    stream: bool,
}

impl AnthropicProvider {
    pub fn new(config: &ProviderConfig, base_url: &str) -> Result<Self> {
        let endpoint = format!("{}/v1/messages", base_url.trim_end_matches('/'));

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&config.api_key).map_err(|_| {
                Error::Provider(
                    "api_key 含有 HTTP 头不允许的字符（常见是复制时带进了换行/空格）。".into(),
                )
            })?,
        );
        headers.insert("anthropic-version", HeaderValue::from_static(ANTHROPIC_VERSION));

        Ok(Self {
            endpoint,
            headers,
            http: HttpClient::new(config.timeout_secs, config.max_retries)?,
            stream: config.stream,
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let body = build_body(&request);
        let value = self
            .http
            .post_json(&self.endpoint, self.headers.clone(), &body)
            .await?;
        parse_response(&value)
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        tx: mpsc::Sender<StreamChunk>,
    ) -> Result<Completion> {
        if !self.stream {
            let completion = self.complete(request).await?;
            pump_text(&completion, &tx).await;
            return Ok(completion);
        }

        let mut body = build_body(&request);
        body["stream"] = json!(true);

        let response = self
            .http
            .post_stream(&self.endpoint, self.headers.clone(), &body)
            .await?;

        // 有的网关无视 stream:true，直接回一整段 JSON。按非流式处理，别报成"流里没内容"。
        if !is_event_stream(&response) {
            let text = response.text().await.unwrap_or_default();
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                Error::Provider(format!(
                    "请求已要求流式，但响应既不是 SSE 也不是 JSON（{e}）。响应片段：{}",
                    excerpt(&text)
                ))
            })?;
            let completion = parse_response(&value)?;
            pump_text(&completion, &tx).await;
            return Ok(completion);
        }

        let max_tokens = request.max_tokens;
        let mut sse = SseStream::new(response);
        let mut acc = AnthropicStream::default();
        while let Some(event) = sse.next().await? {
            if let Some(chunk) = acc.push(&event)? {
                if tx.send(chunk).await.is_err() {
                    break;
                }
            }
        }
        acc.finish(max_tokens)
    }
}

/// 把 SSE 事件增量拼回一条完整消息。
///
/// Anthropic 的流是「块」式：`content_block_start` 开一个块，
/// 若干 `*_delta` 往里追加，`content_block_stop` 收口。
/// 工具参数走 `input_json_delta`，是**被切成片段的 JSON 字符串**，
/// 必须全部拼完才能解析——这是流式最容易写错的地方。
#[derive(Default)]
struct AnthropicStream {
    blocks: Vec<BlockAccum>,
    stop_reason: Option<StopReason>,
}

enum BlockAccum {
    Text {
        index: usize,
        text: String,
    },
    /// 扩展思考（`thinking` / `redacted_thinking`）。只用于展示与诊断，
    /// 拼完不进入消息——它带着签名，回放要求与原始请求完全一致，存下来只会出问题。
    Thinking {
        index: usize,
        text: String,
    },
    Tool {
        index: usize,
        id: String,
        name: String,
        partial: String,
    },
}

impl BlockAccum {
    fn index(&self) -> usize {
        match self {
            Self::Text { index, .. } | Self::Thinking { index, .. } | Self::Tool { index, .. } => {
                *index
            }
        }
    }
}

impl AnthropicStream {
    fn push(&mut self, event: &SseEvent) -> Result<Option<StreamChunk>> {
        match event.event.as_deref() {
            // 流里的错误事件是 HTTP 200 下发的，不处理就会静默变成空回答。
            Some("error") => Err(Error::Provider(format!(
                "anthropic 在流中返回错误：{}",
                vendor_message(&event.data)
            ))),
            Some("content_block_start") => {
                let value = parse_data(&event.data)?;
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                let block = &value["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => self.blocks.push(BlockAccum::Tool {
                        index,
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        partial: String::new(),
                    }),
                    Some("thinking") | Some("redacted_thinking") => {
                        // 块开始时就带着已生成的思考前缀，别丢。
                        self.blocks.push(BlockAccum::Thinking {
                            index,
                            text: block["thinking"].as_str().unwrap_or_default().to_string(),
                        })
                    }
                    // 其余块（例如 server_tool_use）不消费，但也不能让 index 错位，
                    // 所以文本块即使为空也占一个位置。
                    _ => self.blocks.push(BlockAccum::Text {
                        index,
                        text: block["text"].as_str().unwrap_or_default().to_string(),
                    }),
                }
                Ok(None)
            }
            Some("content_block_delta") => {
                let value = parse_data(&event.data)?;
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        if text.is_empty() {
                            return Ok(None);
                        }
                        if let Some(BlockAccum::Text { text: buf, .. }) = self.find_mut(index) {
                            buf.push_str(text);
                        }
                        Ok(Some(StreamChunk::TextDelta(text.to_string())))
                    }
                    Some("input_json_delta") => {
                        let partial = delta["partial_json"].as_str().unwrap_or_default();
                        if let Some(BlockAccum::Tool { partial: buf, .. }) = self.find_mut(index) {
                            buf.push_str(partial);
                        }
                        Ok(None)
                    }
                    Some("thinking_delta") => {
                        let text = delta["thinking"].as_str().unwrap_or_default();
                        if text.is_empty() {
                            return Ok(None);
                        }
                        if let Some(BlockAccum::Thinking { text: buf, .. }) = self.find_mut(index) {
                            buf.push_str(text);
                        }
                        Ok(Some(StreamChunk::ThinkingDelta(text.to_string())))
                    }
                    // signature_delta 之类：本来就不展示，也不报错。
                    _ => Ok(None),
                }
            }
            Some("message_delta") => {
                let value = parse_data(&event.data)?;
                if value.pointer("/delta/stop_reason").and_then(|v| v.as_str()) == Some("max_tokens") {
                    self.stop_reason = Some(StopReason::MaxTokens);
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn find_mut(&mut self, index: usize) -> Option<&mut BlockAccum> {
        self.blocks.iter_mut().find(|block| block.index() == index)
    }

    fn finish(self, max_tokens: u32) -> Result<Completion> {
        if self.blocks.is_empty() {
            return Err(Error::Provider(
                "anthropic 流式响应里没有任何内容块就结束了。\
                 常见原因：网关把 SSE 缓冲后一次性丢弃，或连接被中间设备掐断。"
                    .into(),
            ));
        }

        // 先记下思考有多长：下面会把 blocks 消费掉。
        let reasoned: usize = self
            .blocks
            .iter()
            .map(|block| match block {
                BlockAccum::Thinking { text, .. } => text.chars().count(),
                _ => 0,
            })
            .sum();

        let mut blocks = Vec::with_capacity(self.blocks.len());
        for acc in self.blocks {
            match acc {
                BlockAccum::Text { text, .. } => {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text { text });
                    }
                }
                // 思考不进入消息，见 `BlockAccum::Thinking` 的注释。
                BlockAccum::Thinking { .. } => {}
                BlockAccum::Tool {
                    id, name, partial, ..
                } => {
                    let input = parse_tool_input(&name, &partial)?;
                    blocks.push(ContentBlock::ToolCall(ToolCall { id, name, input }));
                }
            }
        }

        // 只有思考、没有正文也没有工具：额度被思考吃光了。
        // 说清楚，否则用户只会看到"模型什么都没说"。
        if blocks.is_empty() && reasoned > 0 {
            return Err(Error::Provider(format!(
                "这一轮把 {max_tokens} tokens 的额度全用在思考上了\
                 （已思考 {reasoned} 字，正文还没开始就结束）。\
                 把 max_tokens 调大，或关掉扩展思考。"
            )));
        }

        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                blocks,
            },
            stop_reason: self.stop_reason.unwrap_or(StopReason::EndTurn),
        })
    }
}

fn parse_tool_input(name: &str, partial: &str) -> Result<Value> {
    if partial.trim().is_empty() {
        // 无参工具不会发 input_json_delta，这是合法的空输入而不是错误。
        return Ok(json!({}));
    }
    serde_json::from_str(partial).map_err(|e| {
        Error::Provider(format!(
            "模型返回的工具 {name} 的参数不是合法 JSON（{e}）。\
             如果是流式，可能是参数片段没收全。原文：{}",
            excerpt(partial)
        ))
    })
}

fn parse_data(data: &str) -> Result<Value> {
    serde_json::from_str(data).map_err(|e| {
        Error::Provider(format!(
            "anthropic 流中的事件不是合法 JSON（{e}）。原文：{}",
            excerpt(data)
        ))
    })
}

/// 错误事件里挖厂商原话。挖不到就退回原文片段。
fn vendor_message(data: &str) -> String {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(|v| v.as_str())
                .map(|text| text.to_string())
        })
        .unwrap_or_else(|| excerpt(data))
}

fn build_body(request: &CompletionRequest) -> Value {
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "messages": convert_messages(&request.messages),
    });

    if !request.system.trim().is_empty() {
        body["system"] = json!(request.system);
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(request.tools.iter().map(convert_tool).collect::<Vec<_>>());
    }
    body
}

/// `ToolSpec.kind` 是我们内部的策略字段，绝不能发给厂商——
/// 多一个未知字段 Anthropic 会直接 400。
fn convert_tool(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.input_schema,
    })
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();

    for message in messages {
        let blocks: Vec<Value> = message.blocks.iter().filter_map(convert_block).collect();
        if blocks.is_empty() {
            continue;
        }
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        // 相邻同 role 合并：我们内部把工具结果放在 user 消息里，
        // 连续两轮工具调用就会出现 user → user，而 Messages API 要求交替。
        let merged = out
            .last_mut()
            .filter(|last| last["role"] == json!(role))
            .and_then(|last| last["content"].as_array_mut());

        match merged {
            Some(array) => array.extend(blocks),
            None => out.push(json!({ "role": role, "content": blocks })),
        }
    }

    out
}

fn convert_block(block: &ContentBlock) -> Option<Value> {
    match block {
        // 空文本块会被拒（"text content blocks must be non-empty"）。
        ContentBlock::Text { text } if text.is_empty() => None,
        ContentBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
        ContentBlock::ToolCall(call) => Some(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": call.input,
        })),
        ContentBlock::ToolResult(result) => Some(json!({
            "type": "tool_result",
            "tool_use_id": result.id,
            // 工具可能什么都不输出，但空 content 会被拒。
            "content": if result.output.is_empty() { "(无输出)" } else { result.output.as_str() },
            "is_error": !result.ok,
        })),
    }
}

fn parse_response(value: &Value) -> Result<Completion> {
    let content = value
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            Error::Provider(format!(
                "anthropic 响应缺少 content 数组。响应片段：{}",
                excerpt(&value.to_string())
            ))
        })?;

    let mut blocks = Vec::with_capacity(content.len());
    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text { text: text.into() });
                    }
                }
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                if id.is_empty() || name.is_empty() {
                    return Err(Error::Provider(
                        "anthropic 返回了缺少 id 或 name 的 tool_use 块。".into(),
                    ));
                }
                blocks.push(ContentBlock::ToolCall(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    input: block.get("input").cloned().unwrap_or_else(|| json!({})),
                }));
            }
            // 扩展思考的产物，我们的循环用不上，跳过而不是当成错误。
            Some("thinking") | Some("redacted_thinking") => {}
            _ => {}
        }
    }

    let stop_reason = match value.get("stop_reason").and_then(|v| v.as_str()) {
        Some("max_tokens") => StopReason::MaxTokens,
        // tool_use 归到 EndTurn：循环靠"有没有工具调用"决定继不继续，
        // 不靠 stop_reason，这里不该造一个新状态出来。
        _ => StopReason::EndTurn,
    };

    Ok(Completion {
        message: Message {
            role: Role::Assistant,
            blocks,
        },
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use goldenagent_core::{ToolKind, ToolResult};

    /// 测试里用的输出额度，选 512 是为了让报错里的数字好认。
    const TEST_MAX_TOKENS: u32 = 512;

    fn request_with(system: &str, messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest {
            model: "claude-sonnet-4-5".into(),
            system: system.into(),
            messages,
            tools: vec![],
            max_tokens: 1024,
        }
    }

    #[test]
    fn system_goes_top_level_and_is_omitted_when_empty() {
        let request = request_with("你是助手", vec![Message::user("hi")]);
        let body = build_body(&request);
        assert_eq!(body["system"], json!("你是助手"));
        assert!(body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("system").is_none()));

        let empty = request_with("", vec![Message::user("hi")]);
        assert!(build_body(&empty).get("system").is_none());
    }

    /// ToolSpec.kind 是内部字段，漏出去会被 400。
    #[test]
    fn tool_spec_does_not_leak_internal_kind() {
        let spec = ToolSpec::new(
            "read_file",
            "读文件",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            ToolKind::Read,
        );
        let converted = convert_tool(&spec);
        assert!(converted.get("kind").is_none(), "kind 不能发给厂商: {converted}");
        assert_eq!(converted["name"], json!("read_file"));
        assert_eq!(converted["input_schema"]["type"], json!("object"));
    }

    /// 连续两轮工具调用会出现相邻 user 消息，必须合并成一条 content 数组。
    #[test]
    fn adjacent_user_messages_are_merged() {
        let messages = vec![
            Message::user("查两个文件"),
            Message::assistant(vec![
                ContentBlock::ToolCall(ToolCall { id: "t1".into(), name: "read_file".into(), input: json!({}) }),
                ContentBlock::ToolCall(ToolCall { id: "t2".into(), name: "read_file".into(), input: json!({}) }),
            ]),
            Message::tool_results(vec![
                ToolResult { id: "t1".into(), ok: true, output: "A".into() },
                ToolResult { id: "t2".into(), ok: false, output: "".into() },
            ]),
        ];

        let converted = convert_messages(&messages);
        assert_eq!(converted.len(), 3, "user / assistant / user，不应多出一条: {converted:?}");
        assert_eq!(converted[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(converted[2]["content"][0]["type"], json!("tool_result"));
        assert_eq!(converted[2]["content"][0]["is_error"], json!(false));
        assert_eq!(converted[2]["content"][1]["is_error"], json!(true));
        // 空输出要有占位，否则 Anthropic 拒绝空 content
        assert_eq!(converted[2]["content"][1]["content"], json!("(无输出)"));
    }

    #[test]
    fn empty_messages_are_dropped() {
        let messages = vec![
            Message { role: Role::User, blocks: vec![] },
            Message::user("hi"),
        ];
        assert_eq!(convert_messages(&messages).len(), 1);
    }

    #[test]
    fn parses_text_and_tool_use_blocks() {
        let value = json!({
            "content": [
                { "type": "thinking", "thinking": "先看看目录" },
                { "type": "text", "text": "我看一下。" },
                { "type": "tool_use", "id": "toolu_1", "name": "list_dir", "input": {"path": "."} }
            ],
            "stop_reason": "tool_use"
        });

        let completion = parse_response(&value).unwrap();
        assert_eq!(completion.message.blocks.len(), 2, "thinking 应被跳过");
        assert_eq!(completion.message.text(), "我看一下。");
        let calls = completion.message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_dir");
        assert_eq!(calls[0].input["path"], json!("."));
        assert_eq!(completion.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn maps_max_tokens_stop_reason() {
        let value = json!({ "content": [{"type":"text","text":"cut"}], "stop_reason": "max_tokens" });
        assert_eq!(parse_response(&value).unwrap().stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn missing_content_is_a_clear_error() {
        let error = parse_response(&json!({ "id": "msg_1" })).unwrap_err().to_string();
        assert!(error.contains("content"), "{error}");
    }

    // ───────────────────────────── 流式 ─────────────────────────────

    fn ev(name: &str, data: Value) -> SseEvent {
        SseEvent {
            event: Some(name.into()),
            data: data.to_string(),
        }
    }

    fn text_delta(index: u64, text: &str) -> SseEvent {
        ev(
            "content_block_delta",
            json!({"index": index, "delta": {"type": "text_delta", "text": text}}),
        )
    }

    /// 文本增量要原样交出去，顺序不能乱，拼接结果要和整段一致。
    #[test]
    fn stream_assembles_text_and_emits_each_delta() {
        let mut acc = AnthropicStream::default();

        acc.push(&ev("message_start", json!({"type": "message_start"}))).unwrap();
        acc.push(&ev(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        ))
        .unwrap();

        let mut emitted = Vec::new();
        for piece in ["我先", "看一下", "目录。"] {
            if let Some(StreamChunk::TextDelta(text)) = acc.push(&text_delta(0, piece)).unwrap() {
                emitted.push(text);
            }
        }
        acc.push(&ev("content_block_stop", json!({"index": 0}))).unwrap();
        acc.push(&ev("message_delta", json!({"delta": {"stop_reason": "end_turn"}}))).unwrap();

        assert_eq!(emitted, vec!["我先", "看一下", "目录。"]);

        let completion = acc.finish(TEST_MAX_TOKENS).unwrap();
        assert_eq!(completion.message.text(), "我先看一下目录。");
        assert_eq!(completion.stop_reason, StopReason::EndTurn);
    }

    /// 工具参数是被切成片段的 JSON 字符串，必须拼完再解析。
    #[test]
    fn stream_reassembles_fragmented_tool_arguments() {
        let mut acc = AnthropicStream::default();
        acc.push(&ev(
            "content_block_start",
            json!({
                "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}}
            }),
        ))
        .unwrap();

        for piece in ["{\"pa", "th\": \"Car", "go.toml\"}"] {
            acc.push(&ev(
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": piece}}),
            ))
            .unwrap();
        }

        let completion = acc.finish(TEST_MAX_TOKENS).unwrap();
        let calls = completion.message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].input["path"], json!("Cargo.toml"));
    }

    /// 无参工具不会发 input_json_delta，不能因此报错。
    #[test]
    fn stream_tool_without_arguments_defaults_to_empty_object() {
        let mut acc = AnthropicStream::default();
        acc.push(&ev(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "list_dir"}}),
        ))
        .unwrap();

        assert_eq!(acc.finish(TEST_MAX_TOKENS).unwrap().message.tool_calls()[0].input, json!({}));
    }

    /// 流里的 error 事件是 HTTP 200 下发的，不认就会变成一段空白回答。
    #[test]
    fn stream_error_event_surfaces_vendor_message() {
        let mut acc = AnthropicStream::default();
        let error = acc
            .push(&ev(
                "error",
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            ))
            .unwrap_err()
            .to_string();

        assert!(error.contains("Overloaded"), "{error}");
    }

    #[test]
    fn stream_without_any_block_is_an_explicit_error() {
        let error = AnthropicStream::default().finish(TEST_MAX_TOKENS).unwrap_err().to_string();
        assert!(error.contains("没有任何内容块"), "{error}");
    }

    /// max_tokens 在流里是 message_delta 才告知的。
    #[test]
    fn stream_maps_max_tokens_stop_reason() {
        let mut acc = AnthropicStream::default();
        acc.push(&ev(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": "剪断"}}),
        ))
        .unwrap();
        acc.push(&ev("message_delta", json!({"delta": {"stop_reason": "max_tokens"}})))
            .unwrap();

        assert_eq!(acc.finish(TEST_MAX_TOKENS).unwrap().stop_reason, StopReason::MaxTokens);
    }

    // ───────────────────────── 扩展思考 ─────────────────────────

    /// 思考块要发出去给人看，但不能混进消息正文。
    #[test]
    fn stream_emits_thinking_separately_from_text() {
        let mut acc = AnthropicStream::default();
        acc.push(&ev(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        ))
        .unwrap();

        let chunk = acc
            .push(&ev(
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "先想"}}),
            ))
            .unwrap();
        match chunk {
            Some(StreamChunk::ThinkingDelta(text)) => assert_eq!(text, "先想"),
            other => panic!("思考应作为独立片段发出，实际: {other:?}"),
        }

        acc.push(&ev("content_block_stop", json!({"index": 0}))).unwrap();
        acc.push(&ev(
            "content_block_start",
            json!({"index": 1, "content_block": {"type": "text", "text": ""}}),
        ))
        .unwrap();
        acc.push(&ev(
            "content_block_delta",
            json!({"index": 1, "delta": {"type": "text_delta", "text": "答案"}}),
        ))
        .unwrap();

        assert_eq!(
            acc.finish(TEST_MAX_TOKENS).unwrap().message.text(),
            "答案",
            "思考不能进消息"
        );
    }

    /// 只有思考、没有正文：额度被思考吃光，要说清是额度问题。
    #[test]
    fn thinking_only_stream_is_reported_as_budget_exhausted() {
        let mut acc = AnthropicStream::default();
        acc.push(&ev(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "thinking", "thinking": "想了很久"}}),
        ))
        .unwrap();
        acc.push(&ev("message_delta", json!({"delta": {"stop_reason": "max_tokens"}})))
            .unwrap();

        let error = acc.finish(TEST_MAX_TOKENS).unwrap_err().to_string();
        assert!(error.contains("思考"), "{error}");
        assert!(error.contains("512"), "{error}");
        assert!(!error.contains("网关"), "这不是网关的问题: {error}");
    }
}
