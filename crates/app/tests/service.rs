//! 应用层验收：装配、授权往返、压缩、会话续跑。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use goldenagent_app::{AgentConfig, AgentService};
use goldenagent_core::{ContentBlock, Event, Message, StopReason};
use goldenagent_loop::{EventSink, Outcome};
use goldenagent_policy::Mode;
use goldenagent_provider::MockProvider;
use goldenagent_session::{Session, SessionStore};
use serde_json::json;

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
            "goldenagent-app-{}-{stamp}-{seq}",
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

fn write_call(id: &str, path: &str, content: &str) -> Message {
    Message::assistant(vec![ContentBlock::ToolCall(goldenagent_core::ToolCall {
        id: id.into(),
        name: "write_file".into(),
        input: json!({ "path": path, "content": content }),
    })])
}

fn config(ws: &Workspace, mode: Mode) -> AgentConfig {
    AgentConfig::new(&ws.path)
        .with_mode(mode)
        .with_model("mock")
}

#[tokio::test]
async fn text_only_chat_completes() {
    let ws = Workspace::new();
    let service = AgentService::new(
        config(&ws, Mode::ReadOnly),
        Arc::new(MockProvider::new(vec![text("你好")])),
    );
    let (sink, mut rx) = EventSink::channel();

    let outcome = service.chat("你好", &sink).await.unwrap();
    assert_eq!(outcome, Outcome::Completed { turns: 1 });

    let mut saw_finished = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(
            event,
            Event::Finished {
                reason: StopReason::EndTurn
            }
        ) {
            saw_finished = true;
        }
    }
    assert!(saw_finished);
    assert_eq!(service.message_count().await, 2);
    assert_eq!(service.provider_id(), "mock");
}

/// 授权往返：Ask 档下前端答复"允许"，文件才真的被写。
#[tokio::test]
async fn permission_roundtrip_allows_write() {
    let ws = Workspace::new();
    let service = Arc::new(AgentService::new(
        config(&ws, Mode::Ask),
        Arc::new(MockProvider::new(vec![
            write_call("c1", "approved.txt", "写好了"),
            text("完成"),
        ])),
    ));
    let (sink, mut rx) = EventSink::channel();

    let worker = {
        let service = Arc::clone(&service);
        let sink = sink;
        tokio::spawn(async move { service.chat("写个文件", &sink).await })
    };

    let id = loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("等授权事件超时")
        {
            Some(Event::PermissionRequested { id, .. }) => break id,
            Some(_) => continue,
            None => panic!("事件通道提前关闭"),
        }
    };

    assert_eq!(service.pending_permissions(), 1);
    assert!(service.resolve_permission(&id, true));
    assert_eq!(service.pending_permissions(), 0);

    let outcome = worker.await.unwrap().unwrap();
    assert_eq!(outcome, Outcome::Completed { turns: 2 });
    assert_eq!(
        std::fs::read_to_string(ws.path.join("approved.txt")).unwrap(),
        "写好了"
    );
}

/// 没人答复就是拒绝。把超时压到 50ms，验证"默认不安全"这条。
#[tokio::test]
async fn permission_timeout_is_denied() {
    let ws = Workspace::new();
    let mut cfg = config(&ws, Mode::Ask);
    cfg.approval_timeout = Duration::from_millis(50);

    let service = AgentService::new(
        cfg,
        Arc::new(MockProvider::new(vec![
            write_call("c1", "denied.txt", "x"),
            text("那算了"),
        ])),
    );
    let (sink, mut rx) = EventSink::channel();

    service.chat("写个文件", &sink).await.unwrap();

    assert!(!ws.path.join("denied.txt").exists(), "超时必须不落盘");

    let mut denied_tool = None;
    while let Ok(event) = rx.try_recv() {
        if let Event::ToolFinished { ok, output, .. } = event {
            denied_tool = Some((ok, output));
        }
    }
    let (ok, output) = denied_tool.expect("必须有 ToolFinished");
    assert!(!ok);
    assert!(output.contains("授权"), "{output}");
}

/// 切档位后，下一轮立刻按新档位判定，不需要重建 service。
#[tokio::test]
async fn mode_switch_is_picked_up_next_turn() {
    let ws = Workspace::new();
    let service = AgentService::new(
        config(&ws, Mode::Ask),
        Arc::new(MockProvider::new(vec![
            write_call("c1", "auto.txt", "直接写"),
            text("好了"),
        ])),
    );
    assert_eq!(service.mode(), Mode::Ask);
    assert!(service.set_mode(Mode::WorkspaceWrite));

    let (sink, mut rx) = EventSink::channel();
    service.chat("写文件", &sink).await.unwrap();

    let mut asked = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, Event::PermissionRequested { .. }) {
            asked = true;
        }
    }
    assert!(!asked, "WorkspaceWrite 档不该再询问");
    assert!(ws.path.join("auto.txt").exists());
}

/// 超阈值时事件流里必须出现 ContextCompacted，且压缩后这一轮照常完成。
#[tokio::test]
async fn compaction_event_appears_and_turn_still_completes() {
    let ws = Workspace::new();
    let session_dir = ws.path.join("sessions");

    // 造一段足够长的历史，再让它从磁盘恢复
    let history = Session::with_messages(
        (0..20)
            .map(|i| Message::user(format!("第 {i} 条历史记录，内容刻意写得长一些以便超过阈值")))
            .collect(),
    );
    let store = SessionStore::new(&session_dir);
    let seeded = SessionStore::new_id();
    store.persist(&seeded, &history).await.unwrap();

    let mut cfg = config(&ws, Mode::ReadOnly).with_session_dir(&session_dir);
    cfg.compact_at = Some(120);
    cfg.compact_keep = 4;

    let service = AgentService::new(cfg, Arc::new(MockProvider::new(vec![text("收到")])));
    assert!(service.restore().await.unwrap());
    assert_eq!(service.message_count().await, 20);

    let (sink, mut rx) = EventSink::channel();
    let outcome = service.chat("继续", &sink).await.unwrap();

    assert_eq!(outcome, Outcome::Completed { turns: 1 });

    let mut removed = None;
    while let Ok(event) = rx.try_recv() {
        if let Event::ContextCompacted { removed: n } = event {
            removed = Some(n);
        }
    }
    let removed = removed.expect("必须发 ContextCompacted");
    assert!(removed > 0, "removed 必须大于 0");
    assert!(service.message_count().await <= 6);
}

#[tokio::test]
async fn session_persists_across_service_instances() {
    let ws = Workspace::new();
    let session_dir = ws.path.join("sessions");

    let first = AgentService::new(
        config(&ws, Mode::ReadOnly).with_session_dir(&session_dir),
        Arc::new(MockProvider::new(vec![text("第一次回复")])),
    );
    let (sink, _rx) = EventSink::channel();
    first.chat("第一句", &sink).await.unwrap();

    let listed = first.list_sessions(None, false).await.unwrap();
    assert_eq!(listed.len(), 1, "聊过一次就该有一条会话");
    assert_eq!(listed[0].messages, 2);

    let second = AgentService::new(
        config(&ws, Mode::ReadOnly).with_session_dir(&session_dir),
        Arc::new(MockProvider::new(vec![text("第二次回复")])),
    );
    assert!(second.restore().await.unwrap());
    let history = second.history().await;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].text(), "第一句");
    assert_eq!(history[1].text(), "第一次回复");
}

/// 新会话不预建文件：点十次"新对话"也不该在列表里留下十个空壳。
#[tokio::test]
async fn new_session_leaves_no_empty_files() {
    let ws = Workspace::new();
    let service = AgentService::new(
        config(&ws, Mode::ReadOnly).with_session_dir(ws.path.join("sessions")),
        Arc::new(MockProvider::new(vec![text("hi")])),
    );

    service.new_session().await;
    assert!(service.current_session().is_none());
    assert!(service.list_sessions(None, false).await.unwrap().is_empty());

    let (sink, _rx) = EventSink::channel();
    service.chat("你好", &sink).await.unwrap();

    let id = service.current_session().expect("聊过之后要有 id");
    service.new_session().await;
    assert!(service.current_session().is_none());
    assert_eq!(service.list_sessions(None, false).await.unwrap().len(), 1);
    assert_eq!(service.list_sessions(None, false).await.unwrap()[0].id, id);
}

/// 打开 / 改名 / 删除一整圈。
#[tokio::test]
async fn session_lifecycle_roundtrip() {
    let ws = Workspace::new();
    let dir = ws.path.join("sessions");
    let service = AgentService::new(
        config(&ws, Mode::ReadOnly).with_session_dir(&dir),
        Arc::new(MockProvider::new(vec![text("回答一"), text("回答二")])),
    );

    let (sink, _rx) = EventSink::channel();
    service.chat("第一次提问", &sink).await.unwrap();
    let first_id = service.current_session().unwrap();

    service.new_session().await;
    service.chat("第二次提问", &sink).await.unwrap();
    let second_id = service.current_session().unwrap();
    assert_ne!(first_id, second_id);

    // 列表里有两条，标题都是各自的第一句
    let listed = service.list_sessions(None, false).await.unwrap();
    assert_eq!(listed.len(), 2);
    let titles: Vec<_> = listed.iter().map(|s| s.title.as_str()).collect();
    assert!(titles.contains(&"第一次提问"), "{titles:?}");
    assert!(titles.contains(&"第二次提问"), "{titles:?}");

    // 打开第一条，历史要回来
    assert_eq!(service.open_session(&first_id).await.unwrap(), 2);
    assert_eq!(service.current_session().as_deref(), Some(first_id.as_str()));
    assert_eq!(service.history().await[0].text(), "第一次提问");

    assert_eq!(
        service.rename_session(&first_id, "改过的名字").await.unwrap(),
        "改过的名字"
    );
    assert!(service
        .list_sessions(None, false)
        .await
        .unwrap()
        .iter()
        .any(|s| s.id == first_id && s.title == "改过的名字"));

    // 删掉当前会话，要自动切到新会话
    assert!(service.delete_session(&first_id).await.unwrap());
    assert!(service.current_session().is_none());
    assert_eq!(service.message_count().await, 0);
    assert_eq!(service.list_sessions(None, false).await.unwrap().len(), 1);
}

/// 没配会话目录时，历史相关动作要明确报错，而不是假装成功。
#[tokio::test]
async fn history_actions_require_session_dir() {
    let ws = Workspace::new();
    let service = AgentService::new(
        config(&ws, Mode::ReadOnly),
        Arc::new(MockProvider::default()),
    );

    let error = service.list_sessions(None, false).await.unwrap_err();
    assert!(error.to_string().contains("会话目录"), "{error}");
    assert!(service.restore().await.is_err());
}

#[tokio::test]
async fn new_session_clears_history() {
    let ws = Workspace::new();
    let service = AgentService::new(
        config(&ws, Mode::ReadOnly),
        Arc::new(MockProvider::new(vec![text("hi")])),
    );
    let (sink, _rx) = EventSink::channel();
    service.chat("hi", &sink).await.unwrap();
    assert_eq!(service.message_count().await, 2);

    service.new_session().await;
    assert_eq!(service.message_count().await, 0);
}

#[tokio::test]
async fn cancel_on_idle_service_is_a_noop() {
    let ws = Workspace::new();
    let service = AgentService::new(
        config(&ws, Mode::ReadOnly),
        Arc::new(MockProvider::default()),
    );
    assert!(!service.cancel_current(), "没有在跑的时候取消应返回 false");
    assert_eq!(service.pending_permissions(), 0);
    assert!(service.workspace().is_dir());
}
