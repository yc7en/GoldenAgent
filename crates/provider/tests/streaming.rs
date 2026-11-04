//! 流式的端到端验收：**不碰外网**。
//!
//! 起一个本地假服务，按 `transfer-encoding: chunked` 一段一段吐 SSE 帧，
//! 然后把 `base_url` 指过去。这样能验到真实链路：
//! 分片到达 → 字节级拼帧 → 协议语义组装 → 片段推给循环。
//!
//! 单测能验"组装逻辑对不对"，但验不了"分片真的会跨边界"——
//! 所以这里刻意让一个帧跨越两次 chunk 写入。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use goldenagent_core::Message;
use goldenagent_provider::{CompletionRequest, Provider, ProviderConfig, StreamChunk};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// 假的 SSE 服务：把 `pieces` 逐段写出去（每次写入是一个 chunk，中间有间隔）。
async fn spawn_sse(pieces: Vec<String>) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    spawn(pieces, |mut socket, pieces| async move {
        let header = "HTTP/1.1 200 OK\r\n\
                      content-type: text/event-stream\r\n\
                      cache-control: no-cache\r\n\
                      connection: close\r\n\
                      transfer-encoding: chunked\r\n\r\n";
        let _ = socket.write_all(header.as_bytes()).await;

        for piece in pieces {
            // 每个分片按 chunked 编码包一层：<十六进制长度>\r\n<数据>\r\n
            let mut framed = format!("{:x}\r\n", piece.len());
            framed.push_str(&piece);
            framed.push_str("\r\n");
            let _ = socket.write_all(framed.as_bytes()).await;
            let _ = socket.flush().await;
            // 留一点间隔，保证客户端真的分多次读到
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let _ = socket.write_all(b"0\r\n\r\n").await;
        let _ = socket.flush().await;
    })
    .await
}

/// 假服务：普通 JSON 响应（用来模拟"无视 stream:true 的网关"）。
async fn spawn_json(body: String) -> SocketAddr {
    let (addr, _log) = spawn(vec![body], |mut socket, pieces| async move {
        let body = pieces.into_iter().next().unwrap_or_default();
        let header = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(header.as_bytes()).await;
        let _ = socket.write_all(body.as_bytes()).await;
        let _ = socket.flush().await;
    })
    .await;
    addr
}

async fn spawn<F, Fut>(
    pieces: Vec<String>,
    respond: F,
) -> (SocketAddr, Arc<Mutex<Vec<String>>>)
where
    F: FnOnce(tokio::net::TcpStream, Vec<String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    bypass_proxy_for_localhost();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定本地端口");
    let addr = listener.local_addr().expect("取端口");
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&log);

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };

        // 先把请求读完，否则服务端先写会让某些平台上的客户端读不到 body
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let read = socket.read(&mut buf).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..read]);
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
        recorder.lock().await.push(String::from_utf8_lossy(&raw).to_string());

        respond(socket, pieces).await;
    });

    (addr, log)
}

fn request() -> CompletionRequest {
    request_max(128)
}

fn request_max(max_tokens: u32) -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        system: String::new(),
        messages: vec![Message::user("你好")],
        tools: vec![],
        max_tokens,
    }
}

/// 收完片段和整条消息。tx 被 `stream` 吃掉，返回时通道自然关闭。
async fn run_stream(provider: Arc<dyn Provider>) -> (Vec<String>, goldenagent_provider::Completion) {
    let (deltas, _thinking, completion) = run_stream_full(provider).await;
    (deltas, completion)
}

async fn run_stream_full(
    provider: Arc<dyn Provider>,
) -> (
    Vec<String>,
    Vec<String>,
    goldenagent_provider::Completion,
) {
    run_stream_request(provider, request()).await
}

/// 同上，但把"思考"也分开收回来——两者必须能区分，否则前端没法分开展示。
async fn run_stream_request(
    provider: Arc<dyn Provider>,
    request: CompletionRequest,
) -> (
    Vec<String>,
    Vec<String>,
    goldenagent_provider::Completion,
) {
    let (tx, mut rx) = mpsc::channel::<StreamChunk>(64);
    let completion = provider.stream(request, tx).await.expect("流式调用应成功");

    let mut deltas = Vec::new();
    let mut thinking = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        match chunk {
            StreamChunk::TextDelta(text) => deltas.push(text),
            StreamChunk::ThinkingDelta(text) => thinking.push(text),
        }
    }
    (deltas, thinking, completion)
}

fn body_of(raw: &str) -> serde_json::Value {
    let split = raw.find("\r\n\r\n").expect("要有请求体");
    serde_json::from_str(&raw[split + 4..]).expect("请求体应是 JSON")
}

fn bypass_proxy_for_localhost() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
    });
}

// ───────────────────────────── Anthropic ─────────────────────────────

#[tokio::test]
async fn anthropic_stream_emits_deltas_and_asks_for_streaming() {
    // 注意第 4 个分片：一个完整帧被切成两半，横跨两次写入。
    let pieces = vec![
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\n".to_string(),
        "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_string(),
        "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"我先\"}}\n\n".to_string(),
        "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_del".to_string(),
        "ta\",\"text\":\"看一下\"}}\n\n".to_string(),
        "event: content_block_stop\ndata: {\"index\":0}\n\n".to_string(),
        "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n".to_string(),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
    ];

    let (addr, log) = spawn_sse(pieces).await;
    let provider = ProviderConfig::anthropic("test-model", "sk-test")
        .with_base_url(format!("http://{addr}"))
        .with_max_retries(0)
        .build()
        .expect("构建 provider");

    let (deltas, completion) = run_stream(provider).await;

    assert_eq!(deltas, vec!["我先", "看一下"], "跨分片的帧必须能拼回来");
    assert_eq!(completion.message.text(), "我先看一下");

    // 请求体必须明确要求流式，否则厂商会一次性返回，前端就没有逐字效果
    let body = body_of(&log.lock().await[0]);
    assert_eq!(body["stream"], serde_json::json!(true));
}

#[tokio::test]
async fn anthropic_stream_reassembles_tool_use() {
    let pieces = vec![
        "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n".to_string(),
        "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n".to_string(),
        "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"README.md\\\"}\"}}\n\n".to_string(),
        "event: content_block_stop\ndata: {\"index\":0}\n\n".to_string(),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
    ];

    let (addr, _log) = spawn_sse(pieces).await;
    let provider = ProviderConfig::anthropic("test-model", "sk-test")
        .with_base_url(format!("http://{addr}"))
        .with_max_retries(0)
        .build()
        .unwrap();

    let (_, completion) = run_stream(provider).await;
    let calls = completion.message.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].input["path"], serde_json::json!("README.md"));
}

/// 流里的错误是 HTTP 200 下发的，必须冒出来，不能变成一段空白回答。
#[tokio::test]
async fn anthropic_stream_error_event_is_reported() {
    let pieces = vec![
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"Overloaded\"}}\n\n".to_string(),
    ];

    let (addr, _log) = spawn_sse(pieces).await;
    let provider = ProviderConfig::anthropic("test-model", "sk-test")
        .with_base_url(format!("http://{addr}"))
        .with_max_retries(0)
        .build()
        .unwrap();

    let (tx, _rx) = mpsc::channel::<StreamChunk>(8);
    let error = provider
        .stream(request(), tx)
        .await
        .expect_err("流里的错误必须冒泡")
        .to_string();
    assert!(error.contains("Overloaded"), "{error}");
}

// ────────────────────────── OpenAI 兼容 ──────────────────────────

#[tokio::test]
async fn openai_stream_emits_deltas_and_reassembles_tool_calls() {
    let pieces = vec![
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"好的\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"。\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ];

    let (addr, log) = spawn_sse(pieces).await;
    let provider = ProviderConfig::openai("test-model", "sk-test")
        .with_base_url(format!("http://{addr}/v1"))
        .with_max_retries(0)
        .build()
        .unwrap();

    let (deltas, completion) = run_stream(provider).await;

    assert_eq!(deltas, vec!["好的", "。"]);
    assert_eq!(completion.message.text(), "好的。");
    let calls = completion.message.tool_calls();
    assert_eq!(calls[0].input["path"], serde_json::json!("a.txt"));

    assert_eq!(body_of(&log.lock().await[0])["stream"], serde_json::json!(true));
}

/// 有些网关无视 `stream: true`，直接回一整段 JSON。
/// 这时候不能报"流里没内容"，要按非流式处理。
#[tokio::test]
async fn gateway_that_ignores_stream_still_works() {
    let body = serde_json::json!({
        "choices": [{ "message": { "content": "我还是一整段" }, "finish_reason": "stop" }]
    })
    .to_string();

    let addr = spawn_json(body).await;
    let provider = ProviderConfig::openai("test-model", "sk-test")
        .with_base_url(format!("http://{addr}/v1"))
        .with_max_retries(0)
        .build()
        .unwrap();

    let (deltas, completion) = run_stream(provider).await;

    assert_eq!(completion.message.text(), "我还是一整段");
    // 退化成"一次发完"，但片段必须发出去，否则界面一片空白
    assert_eq!(deltas, vec!["我还是一整段"]);
}

/// 配置里关掉流式，行为要与"网关不支持"一致：能跑，只是没有逐字效果。
#[tokio::test]
async fn stream_can_be_disabled_by_config() {    let body = serde_json::json!({
        "choices": [{ "message": { "content": "非流式" }, "finish_reason": "stop" }]
    })
    .to_string();

    let addr = spawn_json(body).await;
    let provider = ProviderConfig::openai("test-model", "sk-test")
        .with_base_url(format!("http://{addr}/v1"))
        .with_max_retries(0)
        .with_stream(false)
        .build()
        .unwrap();

    let (deltas, completion) = run_stream(provider).await;
    assert_eq!(completion.message.text(), "非流式");
    assert_eq!(deltas, vec!["非流式"]);
}

// ───────────────────── 推理模型（reasoning_content） ─────────────────────

/// 真机上撞到的那条链路：先来一串思考，再来正文。
/// 思考要能到界面上，正文要能进消息，两者不能混。
#[tokio::test]
async fn openai_reasoning_stream_is_kept_apart_from_the_answer() {
    let pieces = vec![
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null,\"reasoning_content\":\"先看\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"content\":null,\"reasoning_content\":\"一眼\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"答案是\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"2\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ];

    let (addr, log) = spawn_sse(pieces).await;
    let provider = ProviderConfig::openai("test-model", "sk-test")
        .with_base_url(format!("http://{addr}/v1"))
        .with_max_retries(0)
        .build()
        .unwrap();

    let (deltas, thinking, completion) = run_stream_request(provider, request_max(16000)).await;

    assert_eq!(thinking, vec!["先看", "一眼"], "思考要单独发出去");
    assert_eq!(deltas, vec!["答案是", "2"]);
    assert_eq!(
        completion.message.text(),
        "答案是2",
        "思考不能混进回答——它会被当成模型说过的话发回去"
    );

    // 额度要按配置发出去，不是写死的 4096。
    assert_eq!(
        body_of(&log.lock().await[0])["max_tokens"],
        serde_json::json!(16000)
    );
}

/// 思考把额度吃光、正文一个字都没有：错误必须点明是额度问题。
#[tokio::test]
async fn reasoning_that_consumes_every_token_is_reported_as_such() {
    let pieces = vec![
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"想\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"了\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
    ];

    let (addr, _log) = spawn_sse(pieces).await;
    let provider = ProviderConfig::openai("test-model", "sk-test")
        .with_base_url(format!("http://{addr}/v1"))
        .with_max_retries(0)
        .build()
        .unwrap();

    let (tx, _rx) = mpsc::channel::<StreamChunk>(8);
    let error = provider
        .stream(request_max(1024), tx)
        .await
        .expect_err("只有思考没有正文，不能当成功")
        .to_string();

    assert!(error.contains("思考"), "{error}");
    assert!(error.contains("1024"), "要说清额度上限: {error}");
    assert!(!error.contains("网关不支持"), "别误导人去查网关: {error}");
}
