//! 循环行为验收。
//!
//! 断言的粒度是**事件序列**，不是"跑完没报错"——
//! 事件序列是这个系统唯一对外的行为契约，它错了前端就一定错。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use goldenagent_core::{ContentBlock, Event, Message, StopReason, ToolCall};
use goldenagent_loop::{
    AgentLoop, ApprovalRequest, Approver, Cancellation, Config, EventSink,
    Outcome,
};
use goldenagent_policy::{Mode, Policy};
use goldenagent_provider::{
    Completion, CompletionRequest, MockProvider, Provider, StreamChunk,
};
use goldenagent_session::Session;
use goldenagent_tools::Registry;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;

// ---------------------------------------------------------------- 测试脚手架

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Workspace {
    path: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "goldenagent-loop-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn text(value: &str) -> Message {
    Message::assistant(vec![ContentBlock::Text {
        text: value.into(),
    }])
}

fn call(id: &str, name: &str, input: Value) -> Message {
    Message::assistant(vec![ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        input,
    })])
}

fn build(
    provider: Arc<dyn Provider>,
    workspace: &Workspace,
    mode: Mode,
    config: Config,
) -> AgentLoop {
    AgentLoop::new(
        provider,
        Arc::new(Registry::with_defaults()),
        Policy::new(mode, &workspace.path),
        config,
    )
}

fn drain(rx: &mut UnboundedReceiver<Event>) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

/// 事件序列的"简写名"，让断言能一眼看出顺序对不对。
fn shape(events: &[Event]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            Event::TurnStarted { .. } => "turn_started",
            Event::TextDelta { .. } => "text_delta",
            Event::ThinkingDelta { .. } => "thinking_delta",
            Event::ToolStarted { .. } => "tool_started",
            Event::ToolFinished { .. } => "tool_finished",
            Event::PermissionRequested { .. } => "permission_requested",
            Event::ContextCompacted { .. } => "context_compacted",
            Event::TurnFinished { .. } => "turn_finished",
            Event::Finished { .. } => "finished",
            Event::Error { .. } => "error",
        })
        .collect()
}

fn find_finished(events: &[Event]) -> Option<(bool, String)> {
    events.iter().rev().find_map(|event| match event {
        Event::ToolFinished { ok, output, .. } => Some((*ok, output.clone())),
        _ => None,
    })
}

struct FixedApprover {
    allow: bool,
    seen: Mutex<Vec<ApprovalRequest>>,
}

impl FixedApprover {
    fn new(allow: bool) -> Self {
        Self {
            allow,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<ApprovalRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Approver for FixedApprover {
    async fn approve(&self, request: ApprovalRequest) -> bool {
        self.seen.lock().unwrap().push(request);
        self.allow
    }
}

struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    fn id(&self) -> &'static str {
        "failing"
    }

    async fn complete(&self, _request: CompletionRequest) -> goldenagent_core::Result<Completion> {
        Err(goldenagent_core::Error::Provider("模拟网络中断".into()))
    }
}

/// 逐片吐字的 provider：验证循环把片段原样转成事件，不丢不重不乱序。
struct ChunkedProvider {
    pieces: Vec<&'static str>,
}

#[async_trait]
impl Provider for ChunkedProvider {
    fn id(&self) -> &'static str {
        "chunked"
    }

    async fn complete(&self, _request: CompletionRequest) -> goldenagent_core::Result<Completion> {
        Err(goldenagent_core::Error::Provider("这个 provider 只走流式".into()))
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        tx: mpsc::Sender<StreamChunk>,
    ) -> goldenagent_core::Result<Completion> {
        for piece in &self.pieces {
            if tx
                .send(StreamChunk::TextDelta((*piece).to_string()))
                .await
                .is_err()
            {
                break;
            }
        }
        Ok(Completion {
            message: text(&self.pieces.concat()),
            stop_reason: StopReason::EndTurn,
        })
    }
}

/// 慢速 provider：每 20ms 吐一片，一共 50 片（约 1 秒）。
/// 用来验证"用户按停止"不必等模型把话说完。
struct SlowProvider;

#[async_trait]
impl Provider for SlowProvider {
    fn id(&self) -> &'static str {
        "slow"
    }

    async fn complete(&self, _request: CompletionRequest) -> goldenagent_core::Result<Completion> {
        Err(goldenagent_core::Error::Provider("只走流式".into()))
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        tx: mpsc::Sender<StreamChunk>,
    ) -> goldenagent_core::Result<Completion> {
        for i in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if tx
                .send(StreamChunk::TextDelta(format!("片段{i}")))
                .await
                .is_err()
            {
                break;
            }
        }
        Ok(Completion {
            message: Message::assistant(vec![]),
            stop_reason: StopReason::EndTurn,
        })
    }
}

// ---------------------------------------------------------------- 用例

#[tokio::test]
async fn text_only_turn() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(MockProvider::new(vec![text("你好，我是离线 provider")])),
        &ws,
        Mode::ReadOnly,
        Config::default(),
    );
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    let outcome = agent
        .run(&mut session, "你好", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await
        .unwrap();

    assert_eq!(outcome, Outcome::Completed { turns: 1 });
    assert_eq!(
        shape(&drain(&mut rx)),
        vec!["turn_started", "text_delta", "turn_finished", "finished"]
    );
    assert_eq!(session.len(), 2);
}

#[tokio::test]
async fn tool_call_roundtrip_reaches_next_turn() {
    let ws = Workspace::new();
    std::fs::write(ws.path.join("a.txt"), "内容").unwrap();

    let agent = build(
        Arc::new(MockProvider::new(vec![
            call("c1", "read_file", json!({"path": "a.txt"})),
            text("读到了"),
        ])),
        &ws,
        Mode::ReadOnly,
        Config::default(),
    );
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    let outcome = agent
        .run(&mut session, "读 a.txt", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await
        .unwrap();

    assert_eq!(outcome, Outcome::Completed { turns: 2 });
    assert_eq!(
        shape(&drain(&mut rx)),
        vec![
            "turn_started",
            "tool_started",
            "tool_finished",
            "turn_finished",
            "turn_started",
            "text_delta",
            "turn_finished",
            "finished",
        ]
    );

    // 会话应是 user → assistant(tool_call) → user(tool_result) → assistant(text)
    assert_eq!(session.len(), 4);
    assert_eq!(session.messages()[2].blocks.len(), 1);
}

#[tokio::test]
async fn denied_write_is_still_observable() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(MockProvider::new(vec![
            call("c1", "write_file", json!({"path": "x.txt", "content": "x"})),
            text("好吧"),
        ])),
        &ws,
        Mode::ReadOnly,
        Config::default(),
    );
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    agent
        .run(&mut session, "写个文件", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await
        .unwrap();

    let events = drain(&mut rx);
    let (ok, output) = find_finished(&events).expect("拒绝也必须发 ToolFinished");
    assert!(!ok, "拒绝时 ok 必须为 false");
    assert!(output.contains("只读"), "{output}");
    assert!(!ws.path.join("x.txt").exists(), "拒绝后不得落盘");
}

#[tokio::test]
async fn ask_mode_needs_approval_before_running() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(MockProvider::new(vec![
            call("c1", "write_file", json!({"path": "ok.txt", "content": "写进去了"})),
            text("完成"),
        ])),
        &ws,
        Mode::Ask,
        Config::default(),
    );
    let approver = FixedApprover::new(true);
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    agent
        .run(&mut session, "写文件", &sink, &approver, &Cancellation::new())
        .await
        .unwrap();

    assert_eq!(approver.seen().len(), 1, "Ask 档必须问一次");
    assert!(approver.seen()[0].summary.contains("ok.txt"));

    let events = drain(&mut rx);
    // 授权询问必须在工具开始之前
    let asked = events
        .iter()
        .position(|e| matches!(e, Event::PermissionRequested { .. }))
        .unwrap();
    let started = events
        .iter()
        .position(|e| matches!(e, Event::ToolStarted { .. }))
        .unwrap();
    assert!(asked < started, "{:?}", shape(&events));

    let (ok, _) = find_finished(&events).unwrap();
    assert!(ok);
    assert_eq!(
        std::fs::read_to_string(ws.path.join("ok.txt")).unwrap(),
        "写进去了"
    );
}

#[tokio::test]
async fn ask_mode_denied_skips_execution() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(MockProvider::new(vec![
            call("c1", "write_file", json!({"path": "no.txt", "content": "x"})),
            text("那算了"),
        ])),
        &ws,
        Mode::Ask,
        Config::default(),
    );
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    agent
        .run(&mut session, "写文件", &sink, &FixedApprover::new(false), &Cancellation::new())
        .await
        .unwrap();

    let events = drain(&mut rx);
    let (ok, output) = find_finished(&events).unwrap();
    assert!(!ok);
    assert!(output.contains("授权"), "{output}");
    assert!(!ws.path.join("no.txt").exists());
}

/// A-6：Bypass 放开的是权限，不是路径越界。
#[tokio::test]
async fn bypass_does_not_escape_workspace() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(MockProvider::new(vec![
            call("c1", "read_file", json!({"path": "../outside.txt"})),
            text("读不到"),
        ])),
        &ws,
        Mode::Bypass,
        Config::default(),
    );
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    agent
        .run(&mut session, "读外面的文件", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await
        .unwrap();

    let (ok, output) = find_finished(&drain(&mut rx)).unwrap();
    assert!(!ok);
    assert!(output.contains("越界"), "{output}");
}

#[tokio::test]
async fn unknown_tool_fails_loudly() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(MockProvider::new(vec![
            call("c1", "rm_rf", json!({"path": "/"})),
            text("没有这个工具"),
        ])),
        &ws,
        Mode::Bypass,
        Config::default(),
    );
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    agent
        .run(&mut session, "删库", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await
        .unwrap();

    let events = drain(&mut rx);
    let (ok, output) = find_finished(&events).unwrap();
    assert!(!ok);
    assert!(output.contains("未知工具"), "{output}");
    // 未知工具也要成对出现，前端才能收掉那张卡片
    assert_eq!(
        shape(&events)
            .into_iter()
            .filter(|s| s.starts_with("tool_"))
            .collect::<Vec<_>>(),
        vec!["tool_started", "tool_finished"]
    );
}

#[tokio::test]
async fn max_turns_is_enforced() {
    let ws = Workspace::new();
    std::fs::write(ws.path.join("a.txt"), "x").unwrap();
    let provider = MockProvider::new(vec![
        call("c1", "read_file", json!({"path": "a.txt"})),
        call("c2", "read_file", json!({"path": "a.txt"})),
        call("c3", "read_file", json!({"path": "a.txt"})),
    ]);
    let config = Config {
        max_turns: 2,
        ..Config::default()
    };
    let agent = build(Arc::new(provider), &ws, Mode::ReadOnly, config);
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    let outcome = agent
        .run(&mut session, "一直读", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await
        .unwrap();

    assert_eq!(outcome, Outcome::MaxTurns);
    let events = drain(&mut rx);
    assert!(matches!(
        events.last(),
        Some(Event::Finished {
            reason: StopReason::MaxTurns
        })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::TurnStarted { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn provider_error_emits_error_then_finished() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(FailingProvider),
        &ws,
        Mode::ReadOnly,
        Config::default(),
    );
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    let result = agent
        .run(&mut session, "hi", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await;

    assert!(result.is_err());
    assert_eq!(
        shape(&drain(&mut rx)),
        vec!["turn_started", "error", "finished"]
    );
}

#[tokio::test]
async fn pre_cancelled_run_does_nothing() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(MockProvider::new(vec![text("不该出现")])),
        &ws,
        Mode::ReadOnly,
        Config::default(),
    );
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    let cancel = Cancellation::new();
    cancel.cancel();

    let outcome = agent
        .run(&mut session, "hi", &sink, &FixedApprover::new(true), &cancel)
        .await
        .unwrap();

    assert_eq!(outcome, Outcome::Cancelled);
    assert_eq!(shape(&drain(&mut rx)), vec!["finished"]);
}

#[tokio::test]
async fn compaction_is_announced() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(MockProvider::new(vec![text("好")])),
        &ws,
        Mode::ReadOnly,
        Config::default().with_compaction(20, 2),
    );
    let (sink, mut rx) = EventSink::channel();

    let mut session = Session::new();
    for i in 0..8 {
        session.push_user(format!("历史消息 {i}，内容足够长以触发阈值判断"));
        session.push(text("收到"));
    }

    agent
        .run(&mut session, "现在开始", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await
        .unwrap();

    let events = drain(&mut rx);
    let removed = events
        .iter()
        .find_map(|e| match e {
            Event::ContextCompacted { removed } => Some(*removed),
            _ => None,
        })
        .expect("必须发 ContextCompacted");
    assert!(removed > 0);
    assert_eq!(shape(&events)[0], "context_compacted", "压缩要在本轮请求之前");
    assert!(session.len() <= 3, "压缩后应只剩最近几条 + 本轮提问");
}

#[tokio::test]
async fn event_sink_close_stops_the_loop() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(MockProvider::new(vec![text("hi")])),
        &ws,
        Mode::ReadOnly,
        Config::default(),
    );
    let (sink, rx) = EventSink::channel();
    drop(rx);

    let mut session = Session::new();
    let outcome = agent
        .run(&mut session, "hi", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await
        .unwrap();

    assert_eq!(outcome, Outcome::Cancelled);
    assert_eq!(session.len(), 1, "只写了用户消息就停");
}

/// 流式片段要原样、按序地变成 `text_delta`，且**不能补发一遍全文**。
#[tokio::test]
async fn streamed_deltas_reach_the_frontend_in_order() {
    let ws = Workspace::new();
    let agent = build(
        Arc::new(ChunkedProvider {
            pieces: vec!["我", "先看", "一下"],
        }),
        &ws,
        Mode::ReadOnly,
        Config::default(),
    );
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    let outcome = agent
        .run(&mut session, "看看", &sink, &FixedApprover::new(true), &Cancellation::new())
        .await
        .unwrap();

    assert_eq!(outcome, Outcome::Completed { turns: 1 });

    let events = drain(&mut rx);
    assert_eq!(
        shape(&events),
        vec![
            "turn_started",
            "text_delta",
            "text_delta",
            "text_delta",
            "turn_finished",
            "finished",
        ],
        "三个片段应产生三条事件，不能额外补一条全文"
    );

    let joined: String = events
        .iter()
        .filter_map(|event| match event {
            Event::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(joined, "我先看一下");

    // 会话里存的是拼好的整条消息，不是碎片
    assert_eq!(session.messages()[1].text(), "我先看一下");
}

/// 用户按停止时，不该等模型把剩下几十片说完。
#[tokio::test]
async fn cancel_interrupts_a_running_stream() {
    let ws = Workspace::new();
    let agent = build(Arc::new(SlowProvider), &ws, Mode::ReadOnly, Config::default());
    let (sink, mut rx) = EventSink::channel();
    let mut session = Session::new();

    let cancel = Cancellation::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        trigger.cancel();
    });

    let started = std::time::Instant::now();
    let outcome = agent
        .run(&mut session, "说点长的", &sink, &FixedApprover::new(true), &cancel)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome, Outcome::Cancelled);
    // 完整跑完需要约 1 秒；取消应在一两百毫秒内生效
    assert!(
        elapsed < std::time::Duration::from_millis(600),
        "取消没有立刻生效，耗时 {elapsed:?}"
    );

    let events = drain(&mut rx);
    assert!(matches!(
        events.last(),
        Some(Event::Finished {
            reason: StopReason::Cancelled
        })
    ));
    assert_eq!(session.len(), 1, "取消后不该把半截回答写进会话");
}

