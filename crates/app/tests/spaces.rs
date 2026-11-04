//! 空间层验收：活动工作区、会话归属、路径安全闸、运行中拒绝。
//!
//! 这些用例守的是 A-11 / A-12 / A-13 三条不变量：
//! - A-11 打开会话与切换工作区是同一件事；
//! - A-12 一轮内工作区不变，切换必须等空闲；
//! - A-13 归属由存储位置决定。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use goldenagent_app::{AgentConfig, AgentService};
use goldenagent_core::{ContentBlock, Message, Result, StopReason};
use goldenagent_provider::{Completion, CompletionRequest, MockProvider, Provider};

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
            "goldenagent-spaces-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn state(&self) -> PathBuf {
        self.path.join(".goldenagent")
    }

    fn default_ws(&self) -> PathBuf {
        self.path.join("default-ws")
    }

    /// 新建一个空目录，当作「用户的项目」。
    fn project(&self, name: &str) -> PathBuf {
        let dir = self.path.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config(&self) -> AgentConfig {
        AgentConfig::new(self.default_ws())
            .with_state_dir(self.state())
            .with_model("mock")
    }

    fn service(&self) -> AgentService {
        AgentService::new(self.config(), Arc::new(MockProvider::default()))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 直接写一份 JSONL 到指定空间的目录里（绕过服务层，模拟「历史已经存在」）。
fn seed(state: &Path, space_id: Option<&str>, id: &str, text: &str) {
    let dir = match space_id {
        Some(space) => state.join("sessions").join(space),
        None => state.join("sessions").join("_default"),
    };
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.jsonl")),
        format!(
            "{{\"role\":\"user\",\"blocks\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}\n"
        ),
    )
    .unwrap();
}

fn text(value: &str) -> Message {
    Message::assistant(vec![ContentBlock::Text {
        text: value.into(),
    }])
}

/// 卡在 `complete` 里的 provider：用来制造「有一轮正在跑」。
struct BlockingProvider {
    gate: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl Provider for BlockingProvider {
    fn id(&self) -> &'static str {
        "mock"
    }

    async fn complete(&self, _request: CompletionRequest) -> Result<Completion> {
        let permit = self.gate.acquire().await.expect("信号量没关");
        permit.forget();
        Ok(Completion {
            message: text("跑完了"),
            stop_reason: StopReason::EndTurn,
        })
    }
}

#[tokio::test]
async fn space_crud_and_session_ownership() {
    let ws = Workspace::new();
    let service = ws.service();
    service.bootstrap().await.unwrap();

    let project = ws.project("repo");
    let space = service.create_space("仓库", &project, &[]).await.unwrap();
    assert_eq!(space.sessions, 0);
    assert!(project.is_dir());

    let listed = service.spaces().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "仓库");
    assert!(listed[0].available);

    // 重名（忽略大小写）被拒
    assert!(service.create_space("仓库", &ws.project("other"), &[]).await.is_err());
    // 同一目录建两个空间被拒
    assert!(service.create_space("另一个", &project, &[]).await.is_err());

    // 改名
    let renamed = service
        .update_space(&space.id, Some("改名了"), None, None)
        .await
        .unwrap();
    assert_eq!(renamed.name, "改名了");
    assert_eq!(renamed.path, project);

    // 空间里的会话归属正确，workspace 由空间清单补齐
    let id = "1700000000000-aaaa";
    seed(&ws.state(), Some(&space.id), id, "空间里的会话");
    let in_space = service.list_sessions(Some(&space.id), true).await.unwrap();
    assert_eq!(in_space.len(), 1);
    assert_eq!(in_space[0].space_id.as_deref(), Some(space.id.as_str()));
    assert_eq!(in_space[0].workspace, project);

    // 删除空间：会话移回「任务」
    let moved = service.delete_space(&space.id).await.unwrap();
    assert!(moved >= 1);
    assert!(service.spaces().await.unwrap().is_empty());

    let all = service.list_sessions(None, true).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].space_id, None, "会话应变成「任务」");
}

#[tokio::test]
async fn opening_a_session_activates_its_space() {
    let ws = Workspace::new();
    let service = ws.service();
    service.bootstrap().await.unwrap();

    let project = ws.project("repo");
    let space = service.create_space("仓库", &project, &[]).await.unwrap();

    let id = "1700000000001-bbbb";
    seed(&ws.state(), Some(&space.id), id, "打开我就该切空间");

    assert_eq!(service.workspace(), ws.default_ws(), "启动时在默认工作区");

    service.open_session(id).await.unwrap();
    assert_eq!(service.active_space().as_deref(), Some(space.id.as_str()));
    assert_eq!(service.workspace(), project, "A-11：工作区必须跟着会话走");

    // 回「任务」
    service.new_session_in(None).await;
    assert_eq!(service.active_space(), None);
    assert_eq!(service.workspace(), ws.default_ws());
}

#[tokio::test]
async fn space_path_cannot_point_at_state_dir() {
    let ws = Workspace::new();
    let service = ws.service();
    service.bootstrap().await.unwrap();

    let state = ws.state();
    assert!(
        service.create_space("状态目录", &state, &[]).await.is_err(),
        "指向 .goldenagent 必须被拒"
    );
    assert!(
        service
            .create_space("会话目录", &state.join("sessions"), &[])
            .await
            .is_err(),
        "指向状态目录的子目录也要被拒"
    );

    // 正常的项目目录不受影响
    assert!(service.create_space("正常", &ws.project("ok"), &[]).await.is_ok());
}

#[tokio::test]
async fn switching_is_rejected_while_running() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let service = Arc::new(AgentService::new(
        ws.config(),
        Arc::new(BlockingProvider { gate: gate.clone() }),
    ));
    service.bootstrap().await.unwrap();

    let space = service.create_space("仓库", &ws.project("repo"), &[]).await.unwrap();
    let id = "1700000000002-cccc";
    seed(&ws.state(), Some(&space.id), id, "运行中别切我");

    let (sink, _rx) = goldenagent_loop::EventSink::channel();
    let runner = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.chat("你好", &sink).await })
    };

    // 等它真的进入「忙」——不睡固定时长，避免慢机器上误判。
    let mut busy = false;
    for _ in 0..200 {
        if service.is_busy() {
            busy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(busy, "chat 应该已经把 cancel 标记挂上");

    assert!(
        service.open_session(id).await.is_err(),
        "A-12：运行中切会话必须报错"
    );
    assert!(
        service.set_active_space(Some(&space.id)).await.is_err(),
        "运行中切空间必须报错"
    );
    assert!(service.delete_space(&space.id).await.is_err());

    gate.add_permits(1);
    let outcome = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("chat 应能结束")
        .unwrap()
        .unwrap();
    assert_eq!(outcome, goldenagent_loop::Outcome::Completed { turns: 1 });
    assert!(!service.is_busy(), "跑完必须释放 cancel 标记");
}

#[tokio::test]
async fn archiving_current_session_starts_a_new_one_in_same_space() {
    let ws = Workspace::new();
    let service = ws.service();
    service.bootstrap().await.unwrap();

    let project = ws.project("repo");
    let space = service.create_space("仓库", &project, &[]).await.unwrap();
    let id = "1700000000003-dddd";
    seed(&ws.state(), Some(&space.id), id, "归档我");

    service.open_session(id).await.unwrap();
    assert_eq!(service.current_session().as_deref(), Some(id));

    service.archive_session(id, true).await.unwrap();
    assert_eq!(service.current_session(), None, "归档当前会话要切新会话");
    assert_eq!(
        service.active_space().as_deref(),
        Some(space.id.as_str()),
        "新会话留在同一个空间"
    );

    // 归档的不再出现在默认列表，但仍在归档列表里
    assert!(service.list_sessions(None, false).await.unwrap().is_empty());
    let all = service.list_sessions(None, true).await.unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].archived);

    // 恢复就是删标记
    service.archive_session(id, false).await.unwrap();
    assert_eq!(service.list_sessions(None, false).await.unwrap().len(), 1);
}

#[tokio::test]
async fn pin_rename_and_search_work_across_spaces() {
    let ws = Workspace::new();
    let service = ws.service();
    service.bootstrap().await.unwrap();

    let space = service.create_space("仓库", &ws.project("repo"), &[]).await.unwrap();
    seed(&ws.state(), None, "1700000000004-eeee", "在任务里找 target 目录");
    seed(&ws.state(), Some(&space.id), "1700000000005-ffff", "在空间里找 target 文件");

    let hits = service.search_sessions("target", false).await.unwrap();
    assert_eq!(hits.len(), 2, "两个空间的会话都要能搜到");
    assert!(hits
        .iter()
        .any(|hit| hit.space_id.as_deref() == Some(space.id.as_str())));

    service
        .pin_session("1700000000004-eeee", true)
        .await
        .unwrap();
    let pinned = service
        .list_sessions(None, false)
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.id == "1700000000004-eeee")
        .unwrap();
    assert!(pinned.pinned);

    let renamed = service
        .rename_session("1700000000005-ffff", "改个名")
        .await
        .unwrap();
    assert_eq!(renamed, "改个名");

    // 「打开文件夹」由后端算路径：会话 → 它所属空间的工作目录
    let revealed = service.reveal_session("1700000000005-ffff").await.unwrap();
    assert_eq!(revealed, ws.project("repo"));
}

#[tokio::test]
async fn persistence_lands_in_the_active_space() {
    let ws = Workspace::new();
    let service = ws.service();
    service.bootstrap().await.unwrap();
    let space = service.create_space("仓库", &ws.project("repo"), &[]).await.unwrap();

    service.new_session_in(Some(&space.id)).await;
    let (sink, _rx) = goldenagent_loop::EventSink::channel();
    service.chat("写点什么", &sink).await.unwrap();

    let id = service.current_session().expect("落盘后应有 id");
    let file = ws
        .state()
        .join("sessions")
        .join(&space.id)
        .join(format!("{id}.jsonl"));
    assert!(file.exists(), "会话要落在活动空间的目录里，不是默认目录");
    assert!(!ws
        .state()
        .join("sessions")
        .join("_default")
        .join(format!("{id}.jsonl"))
        .exists());
}

// ---------------------------------------------------------------------------
// B18 · 多根工作区
// ---------------------------------------------------------------------------

/// 切空间时附加根要跟着一起换（A-12 的快照单位是整份根集合）。
#[tokio::test]
async fn extra_roots_switch_together_with_the_space() {
    let ws = Workspace::new();
    let service = ws.service();
    service.bootstrap().await.unwrap();

    let project = ws.project("repo");
    let assets = ws.project("assets");
    let space = service
        .create_space("仓库", &project, &[assets.clone()])
        .await
        .unwrap();

    assert_eq!(service.roots(), vec![ws.default_ws()], "启动时只有主根");

    service.set_active_space(Some(&space.id)).await.unwrap();
    assert_eq!(
        service.roots(),
        vec![project.clone(), assets.clone()],
        "主根在前、附加根在后"
    );
    assert_eq!(service.workspace(), project, "workspace() 仍只报主根");

    service.set_active_space(None).await.unwrap();
    assert_eq!(service.roots(), vec![ws.default_ws()], "回任务要清掉附加根");
}

/// 附加目录同样过状态目录安全闸。
///
/// 主目录指着项目目录、附加目录指着 `.goldenagent/sessions` 一样是
/// "Agent 能改自己的会话记录"，只是绕了一手。
#[tokio::test]
async fn extra_path_cannot_point_at_state_dir() {
    let ws = Workspace::new();
    let service = ws.service();
    service.bootstrap().await.unwrap();

    let project = ws.project("repo");
    let state = ws.state();

    let err = service
        .create_space("仓库", &project, &[state.join("sessions")])
        .await
        .expect_err("附加目录指向会话目录必须被拒");
    assert!(err.to_string().contains("状态目录"), "{err}");

    // 改空间时同样要拦。
    let space = service.create_space("仓库", &project, &[]).await.unwrap();
    assert!(service
        .update_space(&space.id, None, None, Some(&[state.clone()]))
        .await
        .is_err());
}

/// 运行中不允许改附加目录——否则这一轮会在旧根集合里跑完、下一轮换掉。
#[tokio::test]
async fn changing_extra_roots_is_rejected_while_running() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let service = Arc::new(AgentService::new(
        ws.config(),
        Arc::new(BlockingProvider { gate: gate.clone() }),
    ));
    service.bootstrap().await.unwrap();

    let project = ws.project("repo");
    let space = service.create_space("仓库", &project, &[]).await.unwrap();
    service.set_active_space(Some(&space.id)).await.unwrap();

    let busy = Arc::clone(&service);
    let (sink, _rx) = goldenagent_loop::EventSink::channel();
    let turn = tokio::spawn(async move { busy.chat("你好", &sink).await });

    tokio::time::sleep(Duration::from_millis(60)).await;
    let err = service
        .update_space(&space.id, None, None, Some(&[ws.project("late")]))
        .await
        .expect_err("运行中改附加目录必须被拒");
    assert!(err.to_string().contains("还在跑"), "{err}");

    gate.add_permits(1);
    let _ = turn.await;
}

/// 清空附加目录之后，同一空间退回单根。
#[tokio::test]
async fn clearing_extra_roots_returns_to_a_single_root() {
    let ws = Workspace::new();
    let service = ws.service();
    service.bootstrap().await.unwrap();

    let project = ws.project("repo");
    let assets = ws.project("assets");
    let space = service
        .create_space("仓库", &project, &[assets])
        .await
        .unwrap();
    service.set_active_space(Some(&space.id)).await.unwrap();
    assert_eq!(service.roots().len(), 2);

    let updated = service
        .update_space(&space.id, None, None, Some(&[]))
        .await
        .unwrap();
    assert!(updated.extra_paths.is_empty());
    assert_eq!(
        service.roots(),
        vec![project],
        "活动空间的根集合要同步收缩，而不是等下次切空间"
    );
}
