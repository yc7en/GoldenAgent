//! B15 · 端到端对等验证。
//!
//! 这里跑的不是单元测试，是**整条链路**：
//!
//! ```text
//! AgentService → AgentLoop → Anthropic provider → 本地假厂商（HTTP + SSE）
//!      ↑                                              │
//!      └────────────── 事件序列 ─────────────────────┘
//! ```
//!
//! 每个场景都断言**完整事件序列**，而不只是最终状态。原因很实际：
//! 最终状态对的时候，中间可能是"工具走了两遍再回来"或者"先报错再成功"，
//! 只比对结果永远发现不了。顺序错了就是错了。
//!
//! 假厂商是确定性的：请求按顺序消费脚本回复，不存在网络抖动，
//! 所以这个文件在 CI 里能稳定跑。

use std::sync::Arc;

use goldenagent_app::{AgentConfig, AgentService, EventSink};
use goldenagent_core::{Event, Message, StopReason};
use goldenagent_loop::Outcome;
use goldenagent_policy::Mode;
use goldenagent_provider::ProviderConfig;
use goldenagent_session::{Session, SessionStore};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

// ───────────────────────────── 假厂商 ─────────────────────────────

enum Reply {
    /// 正常把帧写完，然后结束 chunked 流。
    Sse(Vec<String>),
    /// 写完这些帧就断开，**不**发结束块——模拟"流说到一半断了"。
    SseCut(Vec<String>),
    /// 先等一会儿再开始吐，用来留出取消的时间窗。
    SseDelayed(u64, Vec<String>),
}

struct Vendor {
    base_url: String,
    /// 每次请求的 body，按到达顺序。
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Vendor {
    async fn start(replies: Vec<Reply>) -> Self {
        bypass_proxy_for_localhost();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定本地端口");
        let addr = listener.local_addr().expect("取端口");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&requests);

        tokio::spawn(async move {
            let mut script = replies.into_iter();

            // 每个请求一次连接（响应里带 connection: close），所以顺序消费脚本即可。
            while let Ok((mut socket, _)) = listener.accept().await {
                let Some(reply) = script.next() else {
                    break;
                };
                let Some(body) = read_request(&mut socket).await else {
                    continue;
                };
                recorder.lock().await.push(body);

                match reply {
                    Reply::Sse(frames) => write_sse(&mut socket, &frames, true).await,
                    Reply::SseCut(frames) => write_sse(&mut socket, &frames, false).await,
                    Reply::SseDelayed(ms, frames) => {
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                        write_sse(&mut socket, &frames, true).await;
                    }
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            requests,
        }
    }

    async fn bodies(&self) -> Vec<Value> {
        self.requests.lock().await.clone()
    }
}

async fn read_request(socket: &mut TcpStream) -> Option<Value> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        let read = socket.read(&mut buf).await.ok()?;
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..read]);

        let text = String::from_utf8_lossy(&raw);
        if let Some(split) = text.find("\r\n\r\n") {
            let length = text[..split]
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if raw.len() >= split + 4 + length {
                let body = &raw[split + 4..];
                return serde_json::from_slice(body).ok();
            }
        }
    }
}

async fn write_sse(socket: &mut TcpStream, frames: &[String], complete: bool) {
    let header = "HTTP/1.1 200 OK\r\n\
                  content-type: text/event-stream\r\n\
                  cache-control: no-cache\r\n\
                  connection: close\r\n\
                  transfer-encoding: chunked\r\n\r\n";
    if socket.write_all(header.as_bytes()).await.is_err() {
        return;
    }

    for frame in frames {
        let mut framed = format!("{:x}\r\n", frame.len());
        framed.push_str(frame);
        framed.push_str("\r\n");
        if socket.write_all(framed.as_bytes()).await.is_err() {
            return;
        }
        let _ = socket.flush().await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    if complete {
        let _ = socket.write_all(b"0\r\n\r\n").await;
        let _ = socket.flush().await;
    }
}

fn bypass_proxy_for_localhost() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // 本机测试不该被用户的 HTTP_PROXY 劫持成 502
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
    });
}

// ───────────────────────────── 帧构造 ─────────────────────────────

fn frame(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn text_frames(chunks: &[&str]) -> Vec<String> {
    let mut frames = vec![
        frame("message_start", json!({"type": "message_start"})),
        frame(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
    ];
    for chunk in chunks {
        frames.push(frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": chunk}}),
        ));
    }
    frames.push(frame("content_block_stop", json!({"index": 0})));
    frames.push(frame(
        "message_delta",
        json!({"delta": {"stop_reason": "end_turn"}}),
    ));
    frames.push(frame("message_stop", json!({"type": "message_stop"})));
    frames
}

/// 一轮里调用多个工具：每个 `tool_use` 占一个内容块，
/// 参数走 `input_json_delta`（真实厂商也是这么切片的）。
fn tool_frames(calls: &[(&str, &str, &str)]) -> Vec<String> {
    let mut frames = vec![frame("message_start", json!({"type": "message_start"}))];

    for (index, (id, name, args)) in calls.iter().enumerate() {
        frames.push(frame(
            "content_block_start",
            json!({
                "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name}
            }),
        ));
        // 故意把参数 JSON 对半切开，验证 provider 的拼接
        let (head, tail) = args.split_at(args.len() / 2);
        for piece in [head, tail] {
            frames.push(frame(
                "content_block_delta",
                json!({
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": piece}
                }),
            ));
        }
        frames.push(frame("content_block_stop", json!({"index": index})));
    }

    frames.push(frame(
        "message_delta",
        json!({"delta": {"stop_reason": "tool_use"}}),
    ));
    frames.push(frame("message_stop", json!({"type": "message_stop"})));
    frames
}

// ───────────────────────────── 测试夹具 ─────────────────────────────

struct Workspace {
    path: std::path::PathBuf,
}

impl Workspace {
    fn new() -> Self {
        // 纳秒时间戳在 Windows 上并不保证线程间唯一（时钟粒度 100ns，
        // 并行跑的用例可能取到同一个值），撞上就会互相删目录——
        // 表现为"偶发地读不到刚写的文件"。加一个进程内序号，彻底断开。
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "goldenagent-scenario-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn service(ws: &Workspace, vendor: &Vendor, mode: Mode) -> AgentService {
    let provider = ProviderConfig::anthropic("claude-test", "sk-test")
        .with_base_url(&vendor.base_url)
        .with_max_retries(0)
        .build()
        .expect("构建 provider");

    AgentService::new(AgentConfig::new(&ws.path).with_mode(mode), provider)
}

/// 带应用状态目录的装配：技能与空间都挂在这上面（`<state>/skills`）。
fn service_with_state(ws: &Workspace, vendor: &Vendor, mode: Mode) -> AgentService {
    let provider = ProviderConfig::anthropic("claude-test", "sk-test")
        .with_base_url(&vendor.base_url)
        .with_max_retries(0)
        .build()
        .expect("构建 provider");

    let state = ws.path.join(".goldenagent");
    AgentService::new(
        AgentConfig::new(&ws.path)
            .with_mode(mode)
            .with_state_dir(&state),
        provider,
    )
}

/// 跑一轮，把事件按顺序收全。
///
/// 返回 `Result` 而不是直接 unwrap：provider 出错时整轮就是 `Err`，
/// 但事件流照样要能断言（`error` 与 `finished` 都得在）。
async fn run(service: &AgentService, prompt: &str) -> (Vec<Event>, Result<Outcome, String>) {
    let (sink, mut rx) = EventSink::channel();
    let outcome = service.chat(prompt, &sink).await.map_err(|e| e.to_string());

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (events, outcome)
}

fn kinds(events: &[Event]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            Event::TurnStarted { .. } => "turn_started",
            Event::TextDelta { .. } => "text_delta",
            Event::ThinkingDelta { .. } => "thinking_delta",
            Event::ToolStarted { .. } => "tool_started",
            Event::ToolFinished { .. } => "tool_finished",
            Event::PermissionRequested { .. } => "permission_requested",
            Event::CompactionStarted { .. } => "compaction_started",
            Event::ContextCompacted { .. } => "context_compacted",
            Event::CompactionFailed { .. } => "compaction_failed",
            Event::TurnFinished { .. } => "turn_finished",
            Event::Finished { .. } => "finished",
            Event::TaskStarted { .. } => "task_started",
            Event::TaskCompleted { .. } => "task_completed",
            Event::Error { .. } => "error",
        })
        .collect()
}

fn text_of(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            Event::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_results(events: &[Event]) -> Vec<(bool, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ToolFinished { ok, output, .. } => Some((*ok, output.clone())),
            _ => None,
        })
        .collect()
}

// ───────────────────────────── 场景 ─────────────────────────────

/// 场景 1：纯文本。片段要按到达顺序拼回原文。
#[tokio::test]
async fn scenario_text_only() {
    let ws = Workspace::new();
    let vendor = Vendor::start(vec![Reply::Sse(text_frames(&["你好", "，我是助手。"]))]).await;
    let service = service(&ws, &vendor, Mode::ReadOnly);

    let (events, outcome) = run(&service, "你好").await;

    assert_eq!(
        kinds(&events),
        vec![
            "turn_started",
            "text_delta",
            "text_delta",
            "turn_finished",
            "finished"
        ]
    );
    assert_eq!(text_of(&events), "你好，我是助手。");
    assert_eq!(outcome.unwrap(), Outcome::Completed { turns: 1 });

    // 请求体必须真的要求流式，且带上工具声明
    let bodies = vendor.bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["stream"], json!(true));
    assert!(bodies[0]["tools"].as_array().is_some_and(|t| !t.is_empty()));
}

/// 场景 2：单工具。工具结果必须回灌进下一次请求。
#[tokio::test]
async fn scenario_single_tool() {
    let ws = Workspace::new();
    ws.write("a.txt", "文件里的内容\n");

    let vendor = Vendor::start(vec![
        Reply::Sse(tool_frames(&[(
            "toolu_1",
            "read_file",
            r#"{"path":"a.txt"}"#,
        )])),
        Reply::Sse(text_frames(&["读到了"])),
    ])
    .await;
    let service = service(&ws, &vendor, Mode::ReadOnly);

    let (events, outcome) = run(&service, "读一下 a.txt").await;

    assert_eq!(
        kinds(&events),
        vec![
            "turn_started",
            "tool_started",
            "tool_finished",
            "turn_finished",
            "turn_started",
            "text_delta",
            "turn_finished",
            "finished"
        ],
        "工具调用必须夹在两轮之间，且成对出现"
    );

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].0, "读文件应该成功: {}", results[0].1);
    assert!(results[0].1.contains("文件里的内容"));

    assert_eq!(outcome.unwrap(), Outcome::Completed { turns: 2 });

    // 第二次请求里必须带着工具结果，否则模型看不到文件
    let bodies = vendor.bodies().await;
    assert_eq!(bodies.len(), 2);
    let second = bodies[1].to_string();
    assert!(second.contains("文件里的内容"), "工具结果没回灌: {second}");
}

/// 场景 3：一轮里多个工具，两对事件都要出现且各自配对。
#[tokio::test]
async fn scenario_multiple_tools_in_one_turn() {
    let ws = Workspace::new();
    ws.write("a.txt", "AAA\n");
    ws.write("b.txt", "BBB\n");

    let vendor = Vendor::start(vec![
        Reply::Sse(tool_frames(&[
            ("toolu_1", "read_file", r#"{"path":"a.txt"}"#),
            ("toolu_2", "read_file", r#"{"path":"b.txt"}"#),
        ])),
        Reply::Sse(text_frames(&["都读完了"])),
    ])
    .await;
    let service = service(&ws, &vendor, Mode::ReadOnly);

    let (events, _) = run(&service, "读两个文件").await;

    assert_eq!(
        kinds(&events),
        vec![
            "turn_started",
            "tool_started",
            "tool_finished",
            "tool_started",
            "tool_finished",
            "turn_finished",
            "turn_started",
            "text_delta",
            "turn_finished",
            "finished"
        ]
    );

    let results = tool_results(&events);
    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(|(ok, _)| *ok),
        "两个读取都该成功，实际: {results:?}"
    );
    assert!(results[0].1.contains("AAA"));
    assert!(results[1].1.contains("BBB"));
}

/// 场景 4：工具被拒。拒绝也必须成对发事件，且文件不能被写出来。
#[tokio::test]
async fn scenario_tool_denied_by_policy() {
    let ws = Workspace::new();
    let vendor = Vendor::start(vec![
        Reply::Sse(tool_frames(&[(
            "toolu_1",
            "write_file",
            r#"{"path":"x.txt","content":"不该被写"}"#,
        )])),
        Reply::Sse(text_frames(&["那就算了"])),
    ])
    .await;
    let service = service(&ws, &vendor, Mode::ReadOnly);

    let (events, _) = run(&service, "写个文件").await;

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(!results[0].0, "只读档必须拒绝");
    assert!(results[0].1.contains("只读"), "拒绝原因要说清楚: {}", results[0].1);
    assert!(!ws.path.join("x.txt").exists(), "拒绝之后不许落盘");

    // 没有弹授权条——只读档是直接拒，不是"先问问"
    assert!(!kinds(&events).contains(&"permission_requested"));
}

/// 场景 5：超轮。模型一直要调工具，max_turns 到顶要明确说"是超轮"。
#[tokio::test]
async fn scenario_max_turns() {
    let ws = Workspace::new();
    ws.write("a.txt", "x");

    let vendor = Vendor::start(vec![
        Reply::Sse(tool_frames(&[("toolu_1", "read_file", r#"{"path":"a.txt"}"#)])),
        Reply::Sse(tool_frames(&[("toolu_2", "read_file", r#"{"path":"a.txt"}"#)])),
    ])
    .await;

    let provider = ProviderConfig::anthropic("claude-test", "sk-test")
        .with_base_url(&vendor.base_url)
        .with_max_retries(0)
        .build()
        .unwrap();
    let mut config = AgentConfig::new(&ws.path).with_mode(Mode::ReadOnly);
    config.max_turns = Some(1);
    let service = AgentService::new(config, provider);

    let (events, outcome) = run(&service, "一直读").await;

    assert_eq!(outcome.unwrap(), Outcome::MaxTurns);
    assert_eq!(
        events.last(),
        Some(&Event::Finished {
            reason: StopReason::MaxTurns
        })
    );
}

/// 场景 6：上下文压缩。事件里必须出现 ContextCompacted，且之后照常完成。
#[tokio::test]
async fn scenario_context_compaction() {
    let ws = Workspace::new();
    let session_dir = ws.path.join(".sessions");
    let store = SessionStore::new(&session_dir);
    let id = SessionStore::new_id();

    let history = Session::with_messages(
        (0..20)
            .map(|i| Message::user(format!("第 {i} 条历史，写得长一点好超过阈值")))
            .collect(),
    );
    store.persist(&id, &history).await.unwrap();

    let vendor = Vendor::start(vec![Reply::Sse(text_frames(&["收到"]))]).await;
    let provider = ProviderConfig::anthropic("claude-test", "sk-test")
        .with_base_url(&vendor.base_url)
        .with_max_retries(0)
        .build()
        .unwrap();

    let mut config = AgentConfig::new(&ws.path)
        .with_mode(Mode::ReadOnly)
        .with_session_dir(&session_dir);
    config.compact_at = Some(120);
    config.compact_keep = 4;
    // 本场景验的是"压缩发生过、且本轮照常完成"，摘要路径由 service 用例覆盖；
    // 开着摘要会把假厂商那一条脚本回复吃掉。
    config.compact_summary = false;
    let service = AgentService::new(config, provider);

    assert!(service.restore().await.unwrap());
    assert_eq!(service.message_count().await, 20);

    let (events, outcome) = run(&service, "继续").await;

    let compacted = events
        .iter()
        .find_map(|event| match event {
            Event::ContextCompacted { removed } => Some(*removed),
            _ => None,
        })
        .expect("必须发 context_compacted");
    assert!(compacted > 0);
    assert_eq!(outcome.unwrap(), Outcome::Completed { turns: 1 });
}

/// 场景 7：流说到一半就断了（没有结束块）。
///
/// 这种情况必须报错，不能"返回一条空消息"——空回答会被当成模型没话说，
/// 而且会把上一轮的上下文悄悄接上去，用户根本看不出中间断过。
#[tokio::test]
async fn scenario_stream_cut_midway() {
    let ws = Workspace::new();
    let vendor = Vendor::start(vec![Reply::SseCut(vec![
        frame("message_start", json!({"type": "message_start"})),
        frame(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "说到一半"}}),
        ),
    ])])
    .await;
    let service = service(&ws, &vendor, Mode::ReadOnly);

    let (events, outcome) = run(&service, "说点什么").await;

    assert_eq!(
        kinds(&events),
        vec!["turn_started", "text_delta", "error", "finished"],
        "断了就是断了，不该假装走完一轮"
    );

    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Error { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "要有且只有一条错误");
    assert!(
        errors[0].contains("中断"),
        "错误要说清是「流被掐断」: {}",
        errors[0]
    );

    assert_eq!(
        events.last(),
        Some(&Event::Finished {
            reason: StopReason::Error
        })
    );
    assert!(outcome.is_err(), "整轮应以错误结束");
}

/// 场景 7b：流正常结束，但一个内容块都没有。
///
/// 和 7 是两种不同的病：这里网络是好的，是厂商回了条空消息。
/// 报错文案也必须不同——"流断了"和"流里没内容"要能分开，
/// 否则排查方向会整个跑偏。
#[tokio::test]
async fn scenario_stream_completes_without_content() {
    let ws = Workspace::new();
    let vendor = Vendor::start(vec![Reply::Sse(vec![
        frame("message_start", json!({"type": "message_start"})),
        frame("message_stop", json!({"type": "message_stop"})),
    ])])
    .await;
    let service = service(&ws, &vendor, Mode::ReadOnly);

    let (events, outcome) = run(&service, "说点什么").await;

    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Error { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("没有任何内容块"),
        "空回答必须点明原因: {}",
        errors[0]
    );
    assert!(outcome.is_err());
}

/// 场景 8：取消。取消不是错误——不该发 Event::Error。
#[tokio::test]
async fn scenario_cancel_while_waiting() {
    let ws = Workspace::new();
    let vendor = Vendor::start(vec![Reply::SseDelayed(600, text_frames(&["慢回答"]))]).await;
    let service = Arc::new(service(&ws, &vendor, Mode::ReadOnly));

    let (sink, mut rx) = EventSink::channel();
    let worker = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.chat("慢一点", &sink).await })
    };

    // 等它真的开始等模型了再取消
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(service.cancel_current(), "这时候应该有正在跑的轮次");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("取消后必须很快返回")
        .unwrap()
        .expect("取消不该让整轮变成 Err");

    assert_eq!(outcome, Outcome::Cancelled);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(!kinds(&events).contains(&"error"), "取消不是错误");
    assert_eq!(
        events.last(),
        Some(&Event::Finished {
            reason: StopReason::Cancelled
        })
    );
}

// ─────────────────── 推理模型：思考不是回答 ───────────────────

/// 思考块 + 正文块，按真实厂商的帧序拼出来。
fn thinking_frames(thinking: &[&str], answer: &[&str], stop: &str) -> Vec<String> {
    let mut frames = vec![frame("message_start", json!({"type": "message_start"}))];
    frames.push(frame(
        "content_block_start",
        json!({"index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
    ));
    for piece in thinking {
        frames.push(frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": piece}}),
        ));
    }
    frames.push(frame("content_block_stop", json!({"index": 0})));

    if !answer.is_empty() {
        frames.push(frame(
            "content_block_start",
            json!({"index": 1, "content_block": {"type": "text", "text": ""}}),
        ));
        for piece in answer {
            frames.push(frame(
                "content_block_delta",
                json!({"index": 1, "delta": {"type": "text_delta", "text": piece}}),
            ));
        }
        frames.push(frame("content_block_stop", json!({"index": 1})));
    }

    frames.push(frame(
        "message_delta",
        json!({"delta": {"stop_reason": stop}}),
    ));
    frames.push(frame("message_stop", json!({"type": "message_stop"})));
    frames
}

/// 场景 9：思考要出现在事件流里，但**不能**混进消息。
///
/// 这条是线上事故的回归：模型先想 800 多 token 才开口，
/// 原实现一个片段都不往外发，界面几十秒空白，然后报"流里没内容"。
#[tokio::test]
async fn scenario_thinking_streams_before_the_answer() {
    let ws = Workspace::new();
    let vendor = Vendor::start(vec![Reply::Sse(thinking_frames(
        &["先确认", "一下路径"],
        &["答案在", "这。"],
        "end_turn",
    ))])
    .await;
    let service = service(&ws, &vendor, Mode::ReadOnly);

    let (events, outcome) = run(&service, "想清楚再答").await;
    outcome.expect("有正文就不该整轮失败");

    assert_eq!(
        kinds(&events),
        vec![
            "turn_started",
            "thinking_delta",
            "thinking_delta",
            "text_delta",
            "text_delta",
            "turn_finished",
            "finished",
        ],
        "思考必须先于正文到达，且与正文是两种事件"
    );
    assert_eq!(text_of(&events), "答案在这。", "正文只算正文");

    let history = service.history().await;
    let assistant = history
        .iter()
        .rev()
        .find(|message| message.role == goldenagent_core::Role::Assistant)
        .expect("应有助手消息");
    assert_eq!(
        assistant.text(),
        "答案在这。",
        "思考不能进历史——下一轮会被当成模型说过的话发回去"
    );
}

/// 场景 10：思考把额度吃光，正文一个字都没有。
/// 报错必须指向"额度"，不能把用户引去查网关。
#[tokio::test]
async fn scenario_thinking_that_exhausts_budget_explains_itself() {
    let ws = Workspace::new();
    let vendor = Vendor::start(vec![Reply::Sse(thinking_frames(
        &["想了很久", "还没想完"],
        &[],
        "max_tokens",
    ))])
    .await;
    let service = service(&ws, &vendor, Mode::ReadOnly);

    let (events, outcome) = run(&service, "很难的问题").await;

    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Error { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("思考"), "{}", errors[0]);
    assert!(errors[0].contains("8192"), "要说出用的是哪档额度: {}", errors[0]);
    assert!(
        !errors[0].contains("网关不支持"),
        "这次跟网关没关系: {}",
        errors[0]
    );
    assert!(outcome.is_err());
}

/// 场景 11：配置文件里的 max_tokens 必须真的走到请求体里。
///
/// 只改一个字段却在运行时仍是 4096，等于没改——这条链路跨了
/// 配置文件 → provider 装配 → AgentService → AgentLoop → 请求体。
#[tokio::test]
async fn scenario_max_tokens_from_config_reaches_the_request() {
    let ws = Workspace::new();
    let vendor = Vendor::start(vec![Reply::Sse(text_frames(&["收到"]))]).await;

    let config_path = ws.path.join(".goldenagent/provider.json");
    ws.write(
        ".goldenagent/provider.json",
        &json!({
            "kind": "anthropic",
            "model": "claude-test",
            "api_key": "sk-test",
            "base_url": vendor.base_url,
            "max_tokens": 16000
        })
        .to_string(),
    );

    let service = AgentService::auto(AgentConfig::new(&ws.path), &config_path);
    let (_, outcome) = run(&service, "你好").await;
    outcome.expect("正常一轮");

    let bodies = vendor.bodies().await;
    assert_eq!(
        bodies[0]["max_tokens"],
        json!(16000),
        "配置里的额度没传到请求体，等于白配"
    );
}

/// 场景 13：技能进系统提示，模型按名字读得到正文。
///
/// 这条要同时证明两件事，少一件都等于没做到：
/// 1. **清单进了提示**——否则模型不知道有这个技能；
/// 2. **正文没进提示**——否则"按需加载"就是一句空话，32 个技能会把上下文吃光。
#[tokio::test]
async fn scenario_skill_is_listed_then_read_on_demand() {
    let ws = Workspace::new();
    ws.write(
        ".goldenagent/skills/pdf/SKILL.md",
        "---\nname: PDF 处理\ndescription: 处理 PDF 时用\n---\n第一步：只提取文本\n",
    );

    let vendor = Vendor::start(vec![
        Reply::Sse(tool_frames(&[(
            "toolu_1",
            "use_skill",
            r#"{"name":"pdf"}"#,
        )])),
        Reply::Sse(text_frames(&["按技能做了"])),
    ])
    .await;
    // ReadOnly 档：读技能不该要授权，但也不能因此获得别的能力。
    let service = service_with_state(&ws, &vendor, Mode::ReadOnly);

    let (events, outcome) = run(&service, "用 pdf 技能处理一下").await;
    outcome.expect("正常一轮");

    let bodies = vendor.bodies().await;
    let system = bodies[0]["system"].to_string();
    assert!(
        system.contains("pdf（PDF 处理）：处理 PDF 时用"),
        "技能清单没进系统提示: {system}"
    );
    assert!(
        !system.contains("第一步：只提取文本"),
        "正文被内联进提示了，按需加载没做到: {system}"
    );

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].0, "use_skill 该成功: {results:?}");
    assert!(
        results[0].1.contains("第一步：只提取文本"),
        "工具没读到正文: {results:?}"
    );
}

/// 场景 11（B20）：主 Agent 委派只读子代理，子代理在独立瞬时上下文中完成搜索并蒸馏回传。
///
/// 契约断言：
/// 1. 主 Agent 成功触发 invoke_subagent；
/// 2. 子代理使用独立的内存会话探索，子代理产生的中间工具（如 read_file）与事件不污染主会话；
/// 3. 主 Agent 仅收到包含结论摘要的单一 ToolResult，并据此给出最终回复；
/// 4. 主会话历史中严格保持 4 条消息（User -> Assistant(invoke_subagent) -> ToolResult -> Assistant），高信噪比。
#[tokio::test]
async fn scenario_subagent_researches_code_and_distills_result() {
    let ws = Workspace::new();
    ws.write("nested/config.toml", "server_port = 8080\nenable_tls = true\n");

    let vendor = Vendor::start(vec![
        // 1. 主 Agent 第 1 轮：识别出需要深入排查配置，委派只读调研子代理
        Reply::Sse(tool_frames(&[(
            "call_sub_1",
            "invoke_subagent",
            r#"{"role":"config_auditor","prompt":"排查 nested/config.toml 中的端口号与 TLS 开关","summary":"审计服务配置"}"#,
        )])),
        // 2. 子代理第 1 轮：在独立临时会话中调用 read_file 读取文件
        Reply::Sse(tool_frames(&[(
            "call_read_1",
            "read_file",
            r#"{"path":"nested/config.toml"}"#,
        )])),
        // 3. 子代理第 2 轮：分析文件内容，输出最终蒸馏结论
        Reply::Sse(text_frames(&[
            "调研结论：服务端口配置为 8080，TLS 加密处于启用状态 (true)。",
        ])),
        // 4. 主 Agent 第 2 轮：收到子代理蒸馏摘要，向用户给出最终回答
        Reply::Sse(text_frames(&[
            "根据审计子代理的汇报，端口为 8080，TLS 已启用。",
        ])),
    ])
    .await;

    let service = service(&ws, &vendor, Mode::ReadOnly);
    let (events, outcome) = run(&service, "帮我排查一下服务配置").await;
    assert_eq!(outcome.unwrap(), Outcome::Completed { turns: 2 });

    // 验证事件流：主事件流只包含 invoke_subagent，绝不包含子代理内部的 read_file
    let event_kinds = kinds(&events);
    assert_eq!(
        event_kinds,
        vec![
            "turn_started",
            "tool_started",
            "tool_finished",
            "turn_finished",
            "turn_started",
            "text_delta",
            "turn_finished",
            "finished"
        ],
        "主事件流中不能泄露子代理内部的 read_file 事件"
    );

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].0, "invoke_subagent 必须成功: {:?}", results[0]);
    assert!(
        results[0].1.contains("服务端口配置为 8080"),
        "工具结果中必须包含子代理蒸馏摘要: {}",
        results[0].1
    );

    // 验证历史记录（高信噪比）：主历史绝无子代理内部的只读日志
    let history = service.history().await;
    assert_eq!(
        history.len(),
        4,
        "主会话必须严格只有 4 条消息，绝不被子代理中间步骤污染"
    );

    // 确认第 2 条是主 Agent 调用 invoke_subagent
    let calls = history[1].tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "invoke_subagent");

    // 确认第 3 条是 invoke_subagent 的蒸馏结果
    let res = history[2].get_tool_results();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id, "call_sub_1");
    assert!(res[0].output.contains("服务端口配置为 8080"));

    // 确认最终回答
    assert!(history[3].text().contains("根据审计子代理的汇报"));
}

/// 场景 12（B21）：后台作业派发（Background Jobs）与响应式唤醒（Reactive Wakeup）。
///
/// 契约断言：
/// 1. 模型派发后台命令（background: true），工具当场返回 TaskId，首轮立即结束并释放 busy 状态；
/// 2. 后台任务在独立协程完成，写入 TaskManager；
/// 3. 当服务空闲时触发 wakeup，系统自动注入任务产物并驱动模型做出总结汇报。
#[tokio::test]
async fn scenario_background_job_and_reactive_wakeup() {
    let ws = Workspace::new();

    let vendor = Vendor::start(vec![
        // 1. 首轮模型调用：发起后台构建任务
        Reply::Sse(tool_frames(&[(
            "call_bash_1",
            "bash",
            r#"{"command":"echo BUILD_SUCCESS_V2","background":true,"summary":"后台构建"}"#,
        )])),
        // 2. 首轮模型接收到 TaskId，立即回复用户并释放主会话
        Reply::Sse(text_frames(&[
            "已在后台启动构建任务，您可以继续执行其他操作。",
        ])),
        // 3. 响应式唤醒轮次：模型收到后台任务产物通知，给出最终汇报
        Reply::Sse(text_frames(&[
            "刚才在后台运行的构建任务已顺利完成，产物确认为 BUILD_SUCCESS_V2。",
        ])),
    ])
    .await;

    let service = service(&ws, &vendor, Mode::Bypass);

    // 第一轮：触发后台任务派发
    let (events, outcome) = run(&service, "开始后台构建").await;
    assert_eq!(outcome.unwrap(), Outcome::Completed { turns: 2 });
    assert!(!service.is_busy(), "首轮结束后主服务必须立即释放忙碌状态");

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].1.contains("后台任务已启动 (TaskId: task_"));

    // 等待后台命令协程执行完毕
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 验证后台任务在 TaskManager 中记录的状态
    let tasks = service.tasks().list();
    assert!(!tasks.is_empty(), "任务必须登记在 TaskManager");
    assert_eq!(tasks[0].status, goldenagent_core::TaskStatus::Completed);

    // 触发响应式唤醒（Reactive Wakeup）
    let (wakeup_sink, _rx) = EventSink::channel();
    let wakeup_outcome = service.wakeup(&wakeup_sink).await.expect("唤醒成功");
    assert!(wakeup_outcome.is_some(), "有未读任务产物时必须触发唤醒");

    // 验证主会话历史中已被注入任务完成通知与最终回复
    let history = service.history().await;
    let full_history_text = history
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(full_history_text.contains("后台任务产物回传"));
    assert!(full_history_text.contains("BUILD_SUCCESS_V2"));
    assert!(full_history_text.contains("刚才在后台运行的构建任务已顺利完成"));
}

/// 场景 16：B22 有状态交互式终端（Stateful PTY Daemon）。
/// 验证通过 terminal_create 启动 PTY 会话、terminal_write 发送命令、terminal_read 读取屏幕增量与 terminal_kill 销毁。
#[tokio::test]
async fn scenario_terminal_interactive_session() {
    let ws = Workspace::new();

    #[cfg(windows)]
    let echo_input = "echo TERM_E2E_OK\r\n";
    #[cfg(not(windows))]
    let echo_input = "echo TERM_E2E_OK\n";

    let write_arg = serde_json::json!({
        "terminal_id": "term-1",
        "input": echo_input,
        "summary": "执行命令"
    })
    .to_string();

    let vendor = Vendor::start(vec![
        // 1. 模型首先创建终端
        Reply::Sse(tool_frames(&[(
            "call_term_1",
            "terminal_create",
            r#"{"name":"e2e-term","summary":"创建测试终端"}"#,
        )])),
        // 2. 模型向终端写入 echo 命令
        Reply::Sse(tool_frames(&[("call_term_2", "terminal_write", &write_arg)])),
        // 3. 模型读取终端屏幕增量
        Reply::Sse(tool_frames(&[(
            "call_term_3",
            "terminal_read",
            r#"{"terminal_id":"term-1","timeout_ms":2000,"inactivity_ms":300,"summary":"读取终端输出"}"#,
        )])),
        // 4. 模型关闭终端并做汇报
        Reply::Sse(tool_frames(&[(
            "call_term_4",
            "terminal_kill",
            r#"{"terminal_id":"term-1","summary":"关闭终端"}"#,
        )])),
        // 5. 最终结论
        Reply::Sse(text_frames(&["终端有状态交互已全部完成并已销毁。"])),
    ])
    .await;

    let service = service(&ws, &vendor, Mode::Bypass);

    let (events, outcome) = run(&service, "在终端中跑个命令").await;
    assert_eq!(outcome.unwrap(), Outcome::Completed { turns: 5 });

    let results = tool_results(&events);
    assert_eq!(results.len(), 4);
    // 验证 terminal_create
    assert!(results[0].1.contains("term-1"));
    // 验证 terminal_write
    assert!(results[1].1.contains("bytes_written"));
    // 验证 terminal_read 抓到了输出
    assert!(results[2].1.contains("TERM_E2E_OK"));
    // 验证 terminal_kill
    assert!(results[3].1.contains("\"killed\":true"));
}

/// 场景 17：B23 操作系统级物理沙箱与环境变量脱敏（OS Sandbox & Secret Masking）。
/// 验证宿主进程中存在的敏感环境变量（如 AWS_ACCESS_KEY_ID, MY_SECRET_TOKEN, PRIVATE_PASSWD）在执行 bash 工具时已被彻底剥离脱敏。
#[tokio::test]
async fn scenario_sandbox_secret_masking() {
    // 确保清理遗留变量
    std::env::remove_var("OPENAI_API_KEY");
    std::env::set_var("MY_SECRET_TOKEN", "sk-secret-token-should-not-leak-12345");
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE");
    std::env::set_var("PRIVATE_PASSWD", "super-secret-password-xyz");

    let ws = Workspace::new();

    #[cfg(windows)]
    let cmd = "cmd /c set";
    #[cfg(not(windows))]
    let cmd = "env";

    let bash_arg = serde_json::json!({
        "command": cmd
    })
    .to_string();

    let vendor = Vendor::start(vec![
        Reply::Sse(tool_frames(&[(
            "call_bash_1",
            "bash",
            &bash_arg,
        )])),
        Reply::Sse(text_frames(&["已执行环境探测。"])),
    ])
    .await;

    let service = service(&ws, &vendor, Mode::Bypass);

    let (events, outcome) = run(&service, "查看环境变量").await;

    // 清理测试环境变量
    std::env::remove_var("MY_SECRET_TOKEN");
    std::env::remove_var("AWS_ACCESS_KEY_ID");
    std::env::remove_var("PRIVATE_PASSWD");

    assert_eq!(outcome.unwrap(), Outcome::Completed { turns: 2 });

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    let output = &results[0].1;

    // 确保命令执行成功（包含常规系统变量如 PATH 或 SystemRoot / USER）
    #[cfg(windows)]
    assert!(output.to_lowercase().contains("path="));
    #[cfg(not(windows))]
    assert!(output.contains("PATH="));

    // 确保敏感环境变量未泄漏到子进程输出中
    assert!(!output.contains("sk-secret-token-should-not-leak"));
    assert!(!output.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(!output.contains("super-secret-password-xyz"));
    assert!(!output.contains("MY_SECRET_TOKEN"));
    assert!(!output.contains("AWS_ACCESS_KEY_ID"));
    assert!(!output.contains("PRIVATE_PASSWD"));
}

/// 场景 18：B25 大结果溢出与句柄机制（Spill & Handle Pattern）。
/// 工具产出 > 32 KiB 巨量输出时，自动拦截落盘为结构化句柄；
/// 模型在下一轮调用 inspect_spill 定向翻阅特定行范围。
#[tokio::test]
async fn scenario_tool_output_spill_and_inspect() {
    let ws = Workspace::new();

    // 预先生成一个 > 32 KiB 的大日志文件
    let mut large_text = String::new();
    for i in 1..=1200 {
        large_text.push_str(&format!("BUILD_STEP_{i:04}: compile details info\n"));
    }
    assert!(large_text.len() > 32 * 1024);
    ws.write("large_build.log", &large_text);
    ws.write(".goldenagent/spill/spill_test.log", &large_text);

    let read_arg = serde_json::json!({
        "path": "large_build.log",
        "limit": 2000
    })
    .to_string();

    let vendor = Vendor::start(vec![
        // 1. 模型尝试读取这个大文件
        Reply::Sse(tool_frames(&[(
            "call_read_1",
            "read_file",
            &read_arg,
        )])),
        // 2. 收到溢出句柄后，模型调用 inspect_spill 翻阅尾部第 1180 到 1200 行
        Reply::Sse(tool_frames(&[(
            "call_inspect_1",
            "inspect_spill",
            r#"{"file":".goldenagent/spill/spill_test.log","start_line":1180,"count":20,"summary":"查阅构建末尾日志"}"#,
        )])),
        // 3. 模型给出结论
        Reply::Sse(text_frames(&["已精准定位到末尾构建日志。"])),
    ])
    .await;

    let service = service(&ws, &vendor, Mode::Bypass);

    let (events, outcome) = run(&service, "读取大型日志并排查末尾错误").await;
    assert_eq!(outcome.unwrap(), Outcome::Completed { turns: 3 });

    let results = tool_results(&events);
    assert_eq!(results.len(), 2);

    // 第一个工具结果已被自动拦截为 Spill 句柄 JSON
    let first_result = &results[0].1;
    assert!(first_result.contains("\"status\": \"spilled\""));
    assert!(first_result.contains("\"file\":"));
    assert!(first_result.contains("BUILD_STEP_0001:"));
    assert!(first_result.contains("BUILD_STEP_1200:"));
    assert!(first_result.contains("\"total_lines\": 1200"));

    // 第二个工具结果是 inspect_spill，返回了指定行号切片
    let second_result = &results[1].1;
    assert!(second_result.contains("1180 | "));
    assert!(second_result.contains("1199 | "));
}



