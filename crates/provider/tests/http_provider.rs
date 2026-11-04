//! 真实 provider 的端到端测试：**不碰外网**。
//!
//! 做法是起一个本地 TCP 假服务，按 HTTP/1.1 返回预设 JSON，
//! 然后把 `base_url` 指过去。这样能验证到真实链路的每一环：
//! 序列化 → 发请求 → 反序列化 → 错误分类，而不用 API key、不受网络影响。
//!
//! 断言的重点不是"能不能跑"，而是**发出去的 body 长相对不对**——
//! 协议字段错了，真厂商会 400，而单测里的构造函数永远发现不了。

use std::net::SocketAddr;
use std::sync::Arc;

use goldenagent_core::ContentBlock;
use goldenagent_core::{Message, ToolResult, ToolSpec, ToolKind};
use goldenagent_provider::{CompletionRequest, ProviderConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// 极简 HTTP/1.1 假服务：按顺序回放 `responses`，记录收到的原始请求。
async fn spawn_server(responses: Vec<(u16, String)>) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    bypass_proxy_for_localhost();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定本地端口");
    let addr = listener.local_addr().expect("取端口");
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&log);

    tokio::spawn(async move {
        let mut cursor = 0usize;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };

            let (status, body) = responses
                .get(cursor)
                .cloned()
                .unwrap_or_else(|| responses.last().expect("至少一个响应").clone());
            if cursor + 1 < responses.len() {
                cursor += 1;
            }

            // 读到 header 结束 + content-length 指定的 body 为止。
            let mut raw = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text[..split]
                        .lines()
                        .find_map(|line| {
                            let lower = line.to_ascii_lowercase();
                            lower
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if raw.len() >= split + 4 + length {
                        break;
                    }
                }
            }

            recorder
                .lock()
                .await
                .push(String::from_utf8_lossy(&raw).to_string());

            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    (addr, log)
}

fn request(system: &str, messages: Vec<Message>, tools: Vec<ToolSpec>) -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        system: system.into(),
        messages,
        tools,
        max_tokens: 256,
    }
}

fn tool_specs() -> Vec<ToolSpec> {
    vec![ToolSpec::new(
        "read_file",
        "读一个文件",
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
        ToolKind::Read,
    )]
}

/// 取出假服务收到的第 `index` 个请求的 body（已解析成 JSON）。
fn body_of(raw_request: &str) -> serde_json::Value {
    let split = raw_request.find("\r\n\r\n").expect("请求要有头和体");
    serde_json::from_str(&raw_request[split + 4..]).expect("请求体应是 JSON")
}

fn header_of(raw_request: &str, name: &str) -> Option<String> {
    raw_request.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        // 只对头名忽略大小写，值必须原样返回——否则 Bearer 会被写成 bearer。
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

/// 测试进程会继承 shell 里的 HTTP_PROXY；本地假服务不该走代理，
/// 否则请求会被代理转发出去，拿回一个 502 而不是我们预设的响应。
fn bypass_proxy_for_localhost() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
    });
}

// ───────────────────────────── Anthropic ─────────────────────────────

#[tokio::test]
async fn anthropic_round_trip_keeps_protocol_shape() {
    let reply = serde_json::json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "content": [
            { "type": "text", "text": "我先看一下目录。" },
            { "type": "tool_use", "id": "toolu_9", "name": "read_file", "input": { "path": "Cargo.toml" } }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 12, "output_tokens": 34 }
    })
    .to_string();

    let (addr, log) = spawn_server(vec![(200, reply)]).await;

    let provider = ProviderConfig::anthropic("test-model", "sk-ant-test")
        .with_base_url(format!("http://{addr}"))
        .with_max_retries(0)
        .build()
        .expect("构建 anthropic provider");

    let completion = provider
        .complete(request("你是助手", vec![Message::user("看看 Cargo.toml")], tool_specs()))
        .await
        .expect("调用应成功");

    // 1. 响应解析
    assert_eq!(completion.message.text(), "我先看一下目录。");
    let calls = completion.message.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "toolu_9");
    assert_eq!(calls[0].input["path"], serde_json::json!("Cargo.toml"));

    // 2. 请求长相
    let raw = log.lock().await[0].clone();
    assert!(raw.starts_with("POST /v1/messages "), "路径应命中 Messages API: {raw}");
    assert_eq!(header_of(&raw, "x-api-key").as_deref(), Some("sk-ant-test"));
    assert_eq!(header_of(&raw, "anthropic-version").as_deref(), Some("2023-06-01"));

    let body = body_of(&raw);
    assert_eq!(body["model"], serde_json::json!("test-model"));
    assert_eq!(body["system"], serde_json::json!("你是助手"));
    assert_eq!(body["max_tokens"], serde_json::json!(256));

    // 工具声明里绝不能出现我们自己的 kind 字段
    let tool = &body["tools"][0];
    assert_eq!(tool["name"], serde_json::json!("read_file"));
    assert!(tool.get("kind").is_none(), "kind 是内部字段，漏出去会被 400: {tool}");
    assert_eq!(tool["input_schema"]["type"], serde_json::json!("object"));
}

#[tokio::test]
async fn anthropic_401_is_reported_as_a_key_problem() {
    let reply = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    let (addr, _log) = spawn_server(vec![(401, reply.to_string())]).await;

    let provider = ProviderConfig::anthropic("test-model", "bad-key")
        .with_base_url(format!("http://{addr}"))
        .with_max_retries(2)
        .build()
        .unwrap();

    let error = provider
        .complete(request("", vec![Message::user("hi")], vec![]))
        .await
        .unwrap_err()
        .to_string();

    // 用户要能照着这句提示去修配置
    assert!(error.contains("401"), "{error}");
    assert!(error.contains("API key"), "应指出是 key 的问题: {error}");
    assert!(error.contains("invalid x-api-key"), "应带上厂商原话: {error}");
    assert!(!error.contains("已重试"), "401 不该重试: {error}");
}

#[tokio::test]
async fn anthropic_server_error_is_retried_then_succeeds() {
    let ok = serde_json::json!({
        "content": [{ "type": "text", "text": "恢复了" }],
        "stop_reason": "end_turn"
    })
    .to_string();

    let (addr, log) = spawn_server(vec![
        (500, "{}".to_string()),
        (503, "{}".to_string()),
        (200, ok),
    ])
    .await;

    let provider = ProviderConfig::anthropic("test-model", "sk-test")
        .with_base_url(format!("http://{addr}"))
        .with_max_retries(2)
        .build()
        .unwrap();

    let completion = provider
        .complete(request("", vec![Message::user("hi")], vec![]))
        .await
        .expect("重试后应成功");

    assert_eq!(completion.message.text(), "恢复了");
    assert_eq!(log.lock().await.len(), 3, "应一共发了 3 次请求");
}

#[tokio::test]
async fn retries_stop_at_the_limit() {
    let (addr, log) = spawn_server(vec![(500, "{}".to_string())]).await;
    let provider = ProviderConfig::anthropic("test-model", "sk-test")
        .with_base_url(format!("http://{addr}"))
        .with_max_retries(1)
        .build()
        .unwrap();

    let error = provider
        .complete(request("", vec![Message::user("hi")], vec![]))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("已重试 1 次"), "{error}");
    assert_eq!(log.lock().await.len(), 2, "初始 1 次 + 重试 1 次");
}

// ────────────────────────── OpenAI 兼容 ──────────────────────────

#[tokio::test]
async fn openai_round_trip_uses_chat_completions_and_bearer() {
    let reply = serde_json::json!({
        "id": "chatcmpl-1",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "好的。",
                "tool_calls": [{
                    "id": "call_a",
                    "type": "function",
                    "function": { "name": "read_file", "arguments": "{\"path\":\"README.md\"}" }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
    .to_string();

    let (addr, log) = spawn_server(vec![(200, reply)]).await;

    let provider = ProviderConfig::openai("test-model", "sk-openai-test")
        // 约定：base_url 填到版本号为止
        .with_base_url(format!("http://{addr}/v1"))
        .with_max_retries(0)
        .build()
        .unwrap();

    let completion = provider
        .complete(request("你是助手", vec![Message::user("看看 README")], tool_specs()))
        .await
        .expect("调用应成功");

    assert_eq!(completion.message.text(), "好的。");
    assert_eq!(completion.message.tool_calls()[0].input["path"], serde_json::json!("README.md"));

    let raw = log.lock().await[0].clone();
    assert!(raw.starts_with("POST /v1/chat/completions "), "路径: {raw}");
    assert_eq!(
        header_of(&raw, "authorization").as_deref(),
        Some("Bearer sk-openai-test")
    );

    let body = body_of(&raw);
    // system 是 messages[0]，不是顶层字段
    assert!(body.get("system").is_none());
    assert_eq!(body["messages"][0]["role"], serde_json::json!("system"));
    // 工具声明是 function 包一层，且不含 kind
    let tool = &body["tools"][0];
    assert_eq!(tool["type"], serde_json::json!("function"));
    assert!(tool.pointer("/function/kind").is_none());
    assert_eq!(tool["function"]["name"], serde_json::json!("read_file"));
}

#[tokio::test]
async fn openai_tool_results_become_tool_role_messages() {
    let reply = serde_json::json!({
        "choices": [{ "message": { "content": "读完了" }, "finish_reason": "stop" }]
    })
    .to_string();

    let (addr, log) = spawn_server(vec![(200, reply)]).await;
    let provider = ProviderConfig::openai("test-model", "k")
        .with_base_url(format!("http://{addr}/v1"))
        .with_max_retries(0)
        .build()
        .unwrap();

    // 真实的多轮工具往返形状
    let messages = vec![
        Message::user("读两个文件"),
        Message::assistant(vec![
            ContentBlock::ToolCall(serde_json::from_value(serde_json::json!({
                "id": "c1", "name": "read_file", "input": {"path": "a"}
            })).unwrap()),
            ContentBlock::ToolCall(serde_json::from_value(serde_json::json!({
                "id": "c2", "name": "read_file", "input": {"path": "b"}
            })).unwrap()),
        ]),
        Message::tool_results(vec![
            ToolResult { id: "c1".into(), ok: true, output: "AAA".into() },
            ToolResult { id: "c2".into(), ok: true, output: "BBB".into() },
        ]),
    ];

    provider
        .complete(request("", messages, vec![]))
        .await
        .expect("调用应成功");

    let body = body_of(&log.lock().await[0].clone());
    let messages = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "tool"], "实际: {roles:?}");
    assert_eq!(messages[2]["tool_call_id"], serde_json::json!("c1"));
    assert_eq!(messages[3]["tool_call_id"], serde_json::json!("c2"));
    // arguments 必须是字符串，不是对象
    assert!(messages[1]["tool_calls"][0]["function"]["arguments"].is_string());
}

#[tokio::test]
async fn openai_400_mentions_likely_causes() {
    let reply = r#"{"error":{"message":"model `nope` does not exist","type":"invalid_request_error"}}"#;
    let (addr, _log) = spawn_server(vec![(400, reply.to_string())]).await;
    let provider = ProviderConfig::openai("nope", "k")
        .with_base_url(format!("http://{addr}/v1"))
        .with_max_retries(0)
        .build()
        .unwrap();

    let error = provider
        .complete(request("", vec![Message::user("hi")], vec![]))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("400"), "{error}");
    assert!(error.contains("模型名"), "应提示可能是模型名写错: {error}");
    assert!(error.contains("does not exist"), "应带上厂商原话: {error}");
}

#[tokio::test]
async fn connection_failure_says_how_to_fix_it() {
    bypass_proxy_for_localhost();
    // 绑一个端口立刻释放，拿到的地址基本不会有人监听
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let provider = ProviderConfig::anthropic("m", "k")
        .with_base_url(format!("http://{addr}"))
        .with_max_retries(0)
        .build()
        .unwrap();

    let error = provider
        .complete(request("", vec![Message::user("hi")], vec![]))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("无法连接"), "{error}");
    assert!(error.contains("base_url"), "应提示检查 base_url: {error}");
}
