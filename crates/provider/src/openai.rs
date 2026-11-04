//! OpenAI 兼容接口（`POST {base}/chat/completions`）。
//!
//! 覆盖 OpenAI 官方以及一切兼容它的服务：DeepSeek、Moonshot、vLLM、
//! Ollama 的 `/v1`、各类自建网关。差异都在这一个文件里消化。
//!
//! 与 Anthropic 的两处结构差异值得记住：
//!
//! - `system` 是 `messages` 里的第一条，不是顶层字段；
//! - 工具结果不是"块"，而是独立的 `role: "tool"` 消息，一个结果一条，
//!   且必须紧跟带 `tool_calls` 的 assistant 消息。

use async_trait::async_trait;
use goldenagent_core::{ContentBlock, Error, Message, Result, Role, StopReason, ToolCall, ToolSpec};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::config::ProviderConfig;
use crate::http::{excerpt, is_event_stream, HttpClient};
use crate::sse::{SseEvent, SseStream};
use crate::{pump_text, Completion, CompletionRequest, Provider, StreamChunk};

pub(crate) struct OpenAiProvider {
    endpoint: String,
    headers: HeaderMap,
    http: HttpClient,
    stream: bool,
}

impl OpenAiProvider {
    pub fn new(config: &ProviderConfig, base_url: &str) -> Result<Self> {
        let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));

        let mut headers = HeaderMap::new();
        let bearer = HeaderValue::from_str(&format!("Bearer {}", config.api_key)).map_err(|_| {
            Error::Provider(
                "api_key 含有 HTTP 头不允许的字符（常见是复制时带进了换行/空格）。".into(),
            )
        })?;
        headers.insert(AUTHORIZATION, bearer);

        Ok(Self {
            endpoint,
            headers,
            http: HttpClient::new(config.timeout_secs, config.max_retries)?,
            stream: config.stream,
        })
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &'static str {
        "openai"
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let body = build_body(&request);
        let value = self
            .http
            .post_json(&self.endpoint, self.headers.clone(), &body)
            .await?;
        parse_response(&value, request.max_tokens)
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

        // 兼容实现里"忽略 stream:true 直接回 JSON"的不在少数（vLLM 关流、各类网关）。
        if !is_event_stream(&response) {
            let text = response.text().await.unwrap_or_default();
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                Error::Provider(format!(
                    "请求已要求流式，但响应既不是 SSE 也不是 JSON（{e}）。响应片段：{}",
                    excerpt(&text)
                ))
            })?;
            let completion = parse_response(&value, request.max_tokens)?;
            pump_text(&completion, &tx).await;
            return Ok(completion);
        }

        let max_tokens = request.max_tokens;
        let mut sse = SseStream::new(response);
        let mut acc = OpenAiStream::default();
        while let Some(event) = sse.next().await? {
            if let Some(chunk) = acc.push(&event)? {
                if tx.send(chunk).await.is_err() {
                    break;
                }
            }
            if acc.is_done() {
                break;
            }
        }
        acc.finish(max_tokens)
    }
}

/// 把 SSE 增量拼回一条完整消息。
///
/// 与 Anthropic 的差别：这里没有"块开始/结束"，全靠 `delta` 累加；
/// 工具参数同样是被切片段的字符串，按 `index` 归并，
/// 且**只有第一片带 `id` 和 `name`**，后面全是参数碎片。
#[derive(Default)]
struct OpenAiStream {
    text: String,
    /// 思考过程。只用于界面展示与错误诊断，不进入消息。
    thinking: String,
    calls: Vec<ToolAccum>,
    finish: Option<String>,
    done: bool,
}

#[derive(Default)]
struct ToolAccum {
    id: String,
    name: String,
    args: String,
}

impl OpenAiStream {
    fn is_done(&self) -> bool {
        self.done
    }

    fn push(&mut self, event: &SseEvent) -> Result<Option<StreamChunk>> {
        // 结束标记。别指望它一定出现——所以要同时看 finish_reason。
        if event.data.trim() == "[DONE]" {
            self.done = true;
            return Ok(None);
        }

        let value: Value = serde_json::from_str(&event.data).map_err(|e| {
            Error::Provider(format!(
                "openai 兼容接口的流事件不是合法 JSON（{e}）。原文：{}",
                excerpt(&event.data)
            ))
        })?;

        // 有的实现把错误塞在 200 的流里。
        if let Some(message) = value.pointer("/error/message").and_then(|v| v.as_str()) {
            return Err(Error::Provider(format!("厂商在流中返回错误：{message}")));
        }

        // usage-only 之类的块没有 choices，跳过而不是报错。
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(None);
        };

        if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            self.finish = Some(reason.to_string());
            if reason == "content_filter" && self.text.is_empty() {
                // 非流式版本会补一句解释，流式也必须发出去——
                // 否则界面上就是一片空白，用户不知道发生了什么。
                let notice = "（响应被厂商的内容策略拦截）".to_string();
                self.text.push_str(&notice);
                return Ok(Some(StreamChunk::TextDelta(notice)));
            }
        }

        let delta = &choice["delta"];

        // 思考过程。DeepSeek 系（`reasoning_content`）与部分网关（`reasoning`）
        // 都会先吐一段思考再给正文；不想让它出现在回答里，但必须看见它，
        // 否则"正文迟迟不来"和"卡死了"在界面上长得一模一样。
        let thinking = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if !thinking.is_empty() {
            self.thinking.push_str(thinking);
            return Ok(Some(StreamChunk::ThinkingDelta(thinking.to_string())));
        }

        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                self.text.push_str(text);
                return Ok(Some(StreamChunk::TextDelta(text.to_string())));
            }
        }

        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for (position, call) in calls.iter().enumerate() {
                // 少数实现不发 index（老版本 vLLM），只能按数组位置当序号。
                let index = call
                    .get("index")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(position);
                while self.calls.len() <= index {
                    self.calls.push(ToolAccum::default());
                }
                let slot = &mut self.calls[index];

                if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        slot.id = id.to_string();
                    }
                }
                if let Some(name) = call.pointer("/function/name").and_then(|v| v.as_str()) {
                    if !name.is_empty() {
                        slot.name = name.to_string();
                    }
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(|v| v.as_str()) {
                    slot.args.push_str(args);
                }
            }
        }

        Ok(None)
    }

    fn finish(self, max_tokens: u32) -> Result<Completion> {
        if self.text.is_empty() && self.calls.is_empty() {
            // 两种"空"必须分开说：一种是连思考都没有（网关/连接的问题），
            // 一种是思考把预算吃光了（配置的问题）。混成一句话的话，
            // 用户会照着"网关不支持 SSE"去查一个根本没坏的网关。
            if !self.thinking.is_empty() {
                return Err(Error::Provider(format!(
                    "这一轮把 {max_tokens} tokens 的额度全用在思考上了\
                     （已思考 {} 字，正文还没开始就结束）。\
                     把 max_tokens 调大，或换一个不做长思考的模型。",
                    self.thinking.chars().count()
                )));
            }
            return Err(Error::Provider(
                "openai 兼容接口的流式响应里没有任何内容就结束了。\
                 常见原因：网关不支持 SSE、把流缓冲后丢弃，或连接被掐断。"
                    .into(),
            ));
        }

        let mut blocks = Vec::new();
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text { text: self.text });
        }

        for call in self.calls {
            if call.id.is_empty() || call.name.is_empty() {
                return Err(Error::Provider(
                    "openai 兼容接口的流式工具调用缺少 id 或 function.name。\
                     多半是该实现没有按规范分片下发。"
                        .into(),
                ));
            }
            let input = if call.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.args).map_err(|e| {
                    Error::Provider(format!(
                        "模型返回的工具 {} 的参数不是合法 JSON（{e}）。\
                         如果是流式，可能是参数片段没收全。原文：{}",
                        call.name,
                        excerpt(&call.args)
                    ))
                })?
            };
            blocks.push(ContentBlock::ToolCall(ToolCall {
                id: call.id,
                name: call.name,
                input,
            }));
        }

        let stop_reason = match self.finish.as_deref() {
            Some("length") => StopReason::MaxTokens,
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
}

fn build_body(request: &CompletionRequest) -> Value {
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "messages": convert_messages(&request.system, &request.messages),
    });

    if !request.tools.is_empty() {
        body["tools"] = json!(request.tools.iter().map(convert_tool).collect::<Vec<_>>());
    }
    body
}

/// `kind` 不能漏出去：部分兼容实现（vLLM、严格的网关）会因未知字段直接 400。
fn convert_tool(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.input_schema,
        },
    })
}

fn convert_messages(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();

    if !system.trim().is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }

    for message in messages {
        match message.role {
            Role::User => {
                let text = message.text();
                if !text.is_empty() {
                    out.push(json!({ "role": "user", "content": text }));
                }
                // 一个 tool_result 一条消息，不能合并。
                for block in &message.blocks {
                    if let ContentBlock::ToolResult(result) = block {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": result.id,
                            "content": if result.output.is_empty() { "(无输出)" } else { result.output.as_str() },
                        }));
                    }
                }
            }
            Role::Assistant => {
                let text = message.text();
                let calls: Vec<Value> = message
                    .tool_calls()
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                // 这里是 JSON **字符串**，不是对象——这一条最容易被写错。
                                "arguments": serde_json::to_string(&call.input)
                                    .unwrap_or_else(|_| "{}".into()),
                            },
                        })
                    })
                    .collect();

                if calls.is_empty() {
                    // 不带工具调用的 assistant 消息，空文本没有意义，丢掉。
                    if !text.is_empty() {
                        out.push(json!({ "role": "assistant", "content": text }));
                    }
                } else {
                    out.push(json!({
                        "role": "assistant",
                        // 有 tool_calls 时 content 允许为 null，但空字符串不合法。
                        "content": if text.is_empty() { Value::Null } else { json!(text) },
                        "tool_calls": calls,
                    }));
                }
            }
        }
    }

    out
}

fn parse_response(value: &Value, max_tokens: u32) -> Result<Completion> {
    let choice = value
        .pointer("/choices/0")
        .ok_or_else(|| {
            Error::Provider(format!(
                "openai 兼容响应缺少 choices[0]。响应片段：{}",
                excerpt(&value.to_string())
            ))
        })?;
    let message = choice.get("message").ok_or_else(|| {
        Error::Provider("openai 兼容响应的 choices[0] 里没有 message。".into())
    })?;

    let mut blocks = Vec::new();

    if let Some(text) = message.get("content").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            blocks.push(ContentBlock::Text { text: text.into() });
        }
    }

    if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for call in calls {
            let id = call.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            let name = call.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or_default();
            if id.is_empty() || name.is_empty() {
                return Err(Error::Provider(
                    "openai 兼容响应里的 tool_calls 缺少 id 或 function.name。".into(),
                ));
            }
            let raw_arguments = call
                .pointer("/function/arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");

            // 参数是字符串形式的 JSON。解析失败要停下来并说清原因，
            // 不能静默变成 {} ——那会让工具拿空参数跑出一个更难懂的错误。
            let input: Value = serde_json::from_str(raw_arguments).map_err(|e| {
                Error::Provider(format!(
                    "模型返回的工具 {name} 的参数不是合法 JSON（{e}）。原文：{}",
                    excerpt(raw_arguments)
                ))
            })?;

            blocks.push(ContentBlock::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                input,
            }));
        }
    }

    let stop_reason = match choice.get("finish_reason").and_then(|v| v.as_str()) {
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => {
            if blocks.is_empty() {
                blocks.push(ContentBlock::Text {
                    text: "（响应被厂商的内容策略拦截）".into(),
                });
            }
            StopReason::EndTurn
        }
        _ => StopReason::EndTurn,
    };

    // 非流式这条路上同样会撞到"思考用尽额度"：推理模型回一整包 JSON，
    // `content` 是 null，思考全在 `reasoning_content` 里。
    // 不讲清楚的话，用户只会看到"模型什么都没说"。
    if blocks.is_empty() && stop_reason == StopReason::MaxTokens {
        let reasoned = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if !reasoned.is_empty() {
            return Err(Error::Provider(format!(
                "这一轮把 {max_tokens} tokens 的额度全用在思考上了\
                 （已思考 {} 字，正文还没开始就结束）。\
                 把 max_tokens 调大，或换一个不做长思考的模型。",
                reasoned.chars().count()
            )));
        }
    }

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

    /// 测试里用的输出额度。选 512 是为了让报错信息里的数字好认。
    const TEST_MAX_TOKENS: u32 = 512;

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: "gpt-4o-mini".into(),
            system: "你是助手".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 512,
        }
    }

    #[test]
    fn system_is_first_message_not_top_level() {
        let body = build_body(&request());
        assert!(body.get("system").is_none());
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert_eq!(body["messages"][0]["content"], json!("你是助手"));
        assert_eq!(body["messages"][1]["role"], json!("user"));
    }

    #[test]
    fn tool_results_become_separate_tool_messages() {
        let mut req = request();
        req.messages = vec![
            Message::user("读两个文件"),
            Message::assistant(vec![
                ContentBlock::ToolCall(ToolCall { id: "c1".into(), name: "read_file".into(), input: json!({"path":"a"}) }),
                ContentBlock::ToolCall(ToolCall { id: "c2".into(), name: "read_file".into(), input: json!({"path":"b"}) }),
            ]),
            Message::tool_results(vec![
                ToolResult { id: "c1".into(), ok: true, output: "AAA".into() },
                ToolResult { id: "c2".into(), ok: true, output: "BBB".into() },
            ]),
        ];

        let body = build_body(&req);
        let messages = body["messages"].as_array().unwrap();
        // system / user / assistant / tool / tool
        assert_eq!(messages.len(), 5, "{messages:?}");
        assert_eq!(messages[3]["role"], json!("tool"));
        assert_eq!(messages[3]["tool_call_id"], json!("c1"));
        assert_eq!(messages[4]["tool_call_id"], json!("c2"));

        // assistant 的 arguments 必须是字符串
        let args = &messages[2]["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_string(), "arguments 必须是 JSON 字符串: {args}");
        assert_eq!(args.as_str().unwrap(), r#"{"path":"a"}"#);
    }

    #[test]
    fn assistant_with_tool_calls_uses_null_content_when_empty() {
        let mut req = request();
        req.messages = vec![Message::assistant(vec![ContentBlock::ToolCall(ToolCall {
            id: "c1".into(),
            name: "list_dir".into(),
            input: json!({}),
        })])];

        let body = build_body(&req);
        let assistant = &body["messages"][1];
        assert_eq!(assistant["content"], Value::Null);
    }

    #[test]
    fn tool_spec_does_not_leak_kind() {
        let spec = ToolSpec::new("write_file", "写文件", json!({"type":"object"}), ToolKind::Write);
        let converted = convert_tool(&spec);
        assert!(converted.pointer("/function/kind").is_none());
        assert_eq!(converted["type"], json!("function"));
        assert_eq!(converted["function"]["parameters"]["type"], json!("object"));
    }

    #[test]
    fn parses_text_and_tool_calls() {
        let value = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "我看看。",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "list_dir", "arguments": "{\"path\":\".\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });

        let completion = parse_response(&value, TEST_MAX_TOKENS).unwrap();
        assert_eq!(completion.message.text(), "我看看。");
        let calls = completion.message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["path"], json!("."));
    }

    #[test]
    fn maps_length_to_max_tokens() {
        let value = json!({
            "choices": [{ "message": { "content": "剪断" }, "finish_reason": "length" }]
        });
        assert_eq!(parse_response(&value, TEST_MAX_TOKENS).unwrap().stop_reason, StopReason::MaxTokens);
    }

    /// 参数坏掉要报错而不是变成空对象——空参数会让工具跑出更难懂的错误。
    #[test]
    fn bad_arguments_is_an_explicit_error() {
        let value = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "{path: a}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let error = parse_response(&value, TEST_MAX_TOKENS).unwrap_err().to_string();
        assert!(error.contains("不是合法 JSON"), "{error}");
        assert!(error.contains("read_file"), "{error}");
    }

    #[test]
    fn content_filter_yields_explanation_instead_of_silence() {
        let value = json!({
            "choices": [{ "message": { "content": null }, "finish_reason": "content_filter" }]
        });
        let completion = parse_response(&value, TEST_MAX_TOKENS).unwrap();
        assert!(completion.message.text().contains("内容策略"));
    }

    // ───────────────────────────── 流式 ─────────────────────────────

    fn data(value: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: value.to_string(),
        }
    }

    /// 文本增量按到达顺序交出去，拼接结果与整段一致。
    #[test]
    fn stream_assembles_text_and_emits_each_delta() {
        let mut acc = OpenAiStream::default();

        let mut emitted = Vec::new();
        for piece in ["好的", "，我来", "看看。"] {
            let chunk = acc
                .push(&data(json!({"choices": [{"delta": {"content": piece}}]})))
                .unwrap();
            if let Some(StreamChunk::TextDelta(text)) = chunk {
                emitted.push(text);
            }
        }
        acc.push(&data(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})))
            .unwrap();

        assert_eq!(emitted, vec!["好的", "，我来", "看看。"]);
        let completion = acc.finish(TEST_MAX_TOKENS).unwrap();
        assert_eq!(completion.message.text(), "好的，我来看看。");
        assert_eq!(completion.stop_reason, StopReason::EndTurn);
    }

    /// 工具参数跨多个 chunk 分片到达，只有第一片带 id / name。
    #[test]
    fn stream_reassembles_fragmented_tool_arguments() {
        let mut acc = OpenAiStream::default();
        acc.push(&data(json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "call_1",
            "function": {"name": "read_file", "arguments": "{\"pa"}
        }]}}]})))
        .unwrap();
        acc.push(&data(json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "function": {"arguments": "th\":\"a.txt\"}"}
        }]}}]})))
        .unwrap();
        acc.push(&data(json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})))
            .unwrap();

        let completion = acc.finish(TEST_MAX_TOKENS).unwrap();
        let calls = completion.message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].input["path"], json!("a.txt"));
    }

    /// 两个工具并行调用：靠 index 归并，不能串到一起。
    #[test]
    fn stream_keeps_parallel_tool_calls_apart() {
        let mut acc = OpenAiStream::default();
        for (index, id, name, args) in [
            (0, "c1", "read_file", "{\"path\":\"a\"}"),
            (1, "c2", "read_file", "{\"path\":\"b\"}"),
        ] {
            acc.push(&data(json!({"choices": [{"delta": {"tool_calls": [{
                "index": index, "id": id,
                "function": {"name": name, "arguments": args}
            }]}}]})))
            .unwrap();
        }

        let calls = acc.finish(TEST_MAX_TOKENS).unwrap().message.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].input["path"], json!("a"));
        assert_eq!(calls[1].input["path"], json!("b"));
    }

    /// 老版本 vLLM 不发 index，只能按数组位置兜底。
    #[test]
    fn stream_falls_back_to_position_when_index_is_missing() {
        let mut acc = OpenAiStream::default();
        acc.push(&data(json!({"choices": [{"delta": {"tool_calls": [{
            "id": "c1", "function": {"name": "list_dir", "arguments": "{}"}
        }]}}]})))
        .unwrap();

        let calls = acc.finish(TEST_MAX_TOKENS).unwrap().message.tool_calls();
        assert_eq!(calls[0].name, "list_dir");
    }

    #[test]
    fn stream_done_marker_ends_the_loop() {
        let mut acc = OpenAiStream::default();
        acc.push(&data(json!({"choices": [{"delta": {"content": "hi"}}]})))
            .unwrap();
        acc.push(&SseEvent { event: None, data: "[DONE]".into() }).unwrap();
        assert!(acc.is_done());
    }

    /// usage-only 之类的块没有 choices，不能因此报错。
    #[test]
    fn stream_ignores_chunks_without_choices() {
        let mut acc = OpenAiStream::default();
        assert!(acc.push(&data(json!({"usage": {"total_tokens": 10}}))).unwrap().is_none());
    }

    /// 内容被拦截时至少要发一句话出去，否则界面一片空白。
    #[test]
    fn stream_content_filter_emits_a_visible_notice() {
        let mut acc = OpenAiStream::default();
        let chunk = acc
            .push(&data(json!({"choices": [{"delta": {}, "finish_reason": "content_filter"}]})))
            .unwrap();

        match chunk {
            Some(StreamChunk::TextDelta(text)) => assert!(text.contains("内容策略")),
            other => panic!("应发出可见提示，实际: {other:?}"),
        }
    }

    #[test]
    fn stream_error_payload_is_surfaced() {
        let mut acc = OpenAiStream::default();
        let error = acc
            .push(&data(json!({"error": {"message": "rate limit exceeded"}})))
            .unwrap_err()
            .to_string();
        assert!(error.contains("rate limit exceeded"), "{error}");
    }

    // ───────────────────────── 推理模型 ─────────────────────────

    /// 思考要单独成块：它既不能在界面上一片空白，也绝不能混进回答正文。
    #[test]
    fn stream_emits_thinking_separately_from_text() {
        let mut acc = OpenAiStream::default();

        let first = acc
            .push(&data(json!({"choices": [{"delta": {"reasoning_content": "我先想想"}}]})))
            .unwrap();
        match first {
            Some(StreamChunk::ThinkingDelta(text)) => assert_eq!(text, "我先想想"),
            other => panic!("思考应作为独立片段发出，实际: {other:?}"),
        }

        let second = acc
            .push(&data(json!({"choices": [{"delta": {"content": "答案是 2"}}]})))
            .unwrap();
        match second {
            Some(StreamChunk::TextDelta(text)) => assert_eq!(text, "答案是 2"),
            other => panic!("正文应照常发出，实际: {other:?}"),
        }

        let completion = acc.finish(TEST_MAX_TOKENS).unwrap();
        assert_eq!(
            completion.message.text(),
            "答案是 2",
            "思考不能进消息——下一轮会被当作回答发回给模型"
        );
    }

    /// 有的网关把字段叫 `reasoning`，不能因为改名就整段丢给用户。
    #[test]
    fn stream_accepts_reasoning_alias() {
        let mut acc = OpenAiStream::default();
        let chunk = acc
            .push(&data(json!({"choices": [{"delta": {"reasoning": "斟酌中"}}]})))
            .unwrap();
        assert!(matches!(chunk, Some(StreamChunk::ThinkingDelta(_))), "{chunk:?}");
    }

    /// 本次线上事故的形状：思考把额度吃光，正文一个字都没有。
    /// 必须说清"是额度不够"，而不是把人往"网关坏了"上引。
    #[test]
    fn reasoning_that_exhausts_the_budget_says_so() {
        let mut acc = OpenAiStream::default();
        for piece in ["先", "把", "问题", "拆开"] {
            acc.push(&data(json!({"choices": [{"delta": {"reasoning_content": piece}}]})))
                .unwrap();
        }
        acc.push(&data(json!({"choices": [{"delta": {}, "finish_reason": "length"}]})))
            .unwrap();

        let error = acc.finish(TEST_MAX_TOKENS).unwrap_err().to_string();
        assert!(error.contains("思考"), "要点明是思考占了额度: {error}");
        assert!(error.contains("512"), "要说出额度上限: {error}");
        assert!(error.contains("max_tokens"), "要给出下一步动作: {error}");
        assert!(
            !error.contains("网关不支持"),
            "这跟网关没关系，别误导: {error}"
        );
    }

    /// 非流式（关掉流 / 网关忽略 stream）也要给同一句诊断。
    #[test]
    fn non_stream_reasoning_only_response_is_reported() {
        let value = json!({
            "choices": [{
                "message": { "content": null, "reasoning_content": "想了很久" },
                "finish_reason": "length"
            }]
        });

        let error = parse_response(&value, TEST_MAX_TOKENS).unwrap_err().to_string();
        assert!(error.contains("思考"), "{error}");
        assert!(error.contains("512"), "{error}");
    }

    /// 真的一点内容都没有（没有思考块）时，原来那句话还得留着。
    #[test]
    fn empty_stream_without_reasoning_keeps_the_old_message() {
        let error = OpenAiStream::default()
            .finish(TEST_MAX_TOKENS)
            .unwrap_err()
            .to_string();
        assert!(error.contains("没有任何内容"), "{error}");
    }
}
