//! 升级回归：拿一份旧布局原地升级，历史不能少、标题不能变、还能继续聊。
//!
//! 旧布局（B10 及以前）长这样：
//!
//! ```text
//! .goldenagent/
//!   session.jsonl                  ← 更早的单文件
//!   sessions/1762…-a1b2.jsonl      ← 直接躺在 sessions 根下
//!   sessions/1762…-a1b2.title
//! ```
//!
//! 升级后要变成分空间布局，且**旧文件要么搬走、要么保持可读**，
//! 不能出现「程序说没有历史，但文件还在磁盘上」这种状态。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use goldenagent_app::{AgentConfig, AgentService};
use goldenagent_loop::EventSink;
use goldenagent_provider::MockProvider;

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
            "goldenagent-upgrade-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn state(&self) -> PathBuf {
        self.path.join(".goldenagent")
    }

    fn service(&self) -> AgentService {
        AgentService::new(
            AgentConfig::new(self.path.join("ws"))
                .with_state_dir(self.state())
                .with_model("mock"),
            Arc::new(MockProvider::default()),
        )
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write_session(dir: &std::path::Path, id: &str, lines: &[&str]) {
    std::fs::create_dir_all(dir).unwrap();
    let body = lines
        .iter()
        .map(|text| {
            format!("{{\"role\":\"user\",\"blocks\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.join(format!("{id}.jsonl")), format!("{body}\n")).unwrap();
}

#[tokio::test]
async fn old_flat_layout_upgrades_in_place() {
    let ws = Workspace::new();
    let state = ws.state();
    let sessions_dir = state.join("sessions");

    // 旧布局：两条会话直接躺在 sessions/ 根下，其中一条带手工标题。
    write_session(&sessions_dir, "1700000000000-aaaa", &["旧会话一", "第二句"]);
    write_session(&sessions_dir, "1700000001000-bbbb", &["旧会话二"]);
    std::fs::write(sessions_dir.join("1700000001000-bbbb.title"), "我起的名\n").unwrap();

    // 更早的单文件会话：`adopt_legacy` 负责收编。
    std::fs::write(
        state.join("session.jsonl"),
        "{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"最早那条\"}]}\n",
    )
    .unwrap();

    let service = ws.service();
    service.bootstrap().await.unwrap();
    service
        .adopt_legacy(state.join("session.jsonl"))
        .await
        .unwrap();
    assert!(service.restore().await.unwrap(), "升级后必须能接上次");

    let listed = service.list_sessions(None, true).await.unwrap();
    assert_eq!(listed.len(), 3, "两条扁平会话 + 一条单文件会话，一条都不能少");
    assert!(
        listed.iter().all(|item| item.space_id.is_none()),
        "迁移后都归「任务」"
    );
    assert!(
        listed.iter().any(|item| item.title == "我起的名"),
        "手工标题不能被自动标题冲掉"
    );

    // 文件确实搬进了 _default，根目录不再有 jsonl
    let default_dir = sessions_dir.join("_default");
    assert!(default_dir.join("1700000000000-aaaa.jsonl").exists());
    assert!(!sessions_dir.join("1700000000000-aaaa.jsonl").exists());
    assert!(state.join("spaces.json").exists(), "启动要补出空间清单");

    // 还能继续聊：打开一条旧会话，消息数对得上，再发一句不会丢历史。
    service.open_session("1700000000000-aaaa").await.unwrap();
    assert_eq!(service.message_count().await, 2);
    let (sink, _rx) = EventSink::channel();
    service.chat("接着聊", &sink).await.unwrap();
    assert_eq!(service.message_count().await, 4, "历史 2 条 + 新问 + 新答");
}

#[tokio::test]
async fn upgrade_runs_twice_without_duplicating() {
    let ws = Workspace::new();
    let sessions_dir = ws.state().join("sessions");
    write_session(&sessions_dir, "1700000002000-cccc", &["只此一条"]);

    // 第一次启动：迁移 + 接上次
    let first = ws.service();
    first.bootstrap().await.unwrap();
    assert!(first.restore().await.unwrap());
    assert_eq!(first.list_sessions(None, true).await.unwrap().len(), 1);

    // 第二次启动：同一份状态再跑一遍，不能多出副本
    let second = ws.service();
    second.bootstrap().await.unwrap();
    assert!(second.restore().await.unwrap());
    let listed = second.list_sessions(None, true).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "只此一条");
}
