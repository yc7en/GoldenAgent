//! GoldenAgent 桌面外壳。
//!
//! 这个 crate 只做三件事：
//!
//! 1. 建 `AppState`（持 `AgentService`）；
//! 2. 把前端 `invoke` 原样转成 service 调用；
//! 3. 把 service 的事件流包成 envelope 后 `emit("agent://event")`。
//!
//! **这里不许出现业务判断**：不放权限逻辑、不放重试策略、不读会话文件。
//! 想加功能，加在下面某一层里，别加在这儿——否则半年后它就会长成第二个引擎。
//!
//! 唯一的例外是 `reveal_session` 的**路径来源**：它不接受前端传路径，
//! 只接受 id，路径由 app 层算。否则这里就成了「前端让后端打开任意目录」的越权面。

use std::sync::Arc;

use goldenagent_app::{
    AgentConfig, AgentService, Event, EventSink, Message, Mode, Outcome, ProviderInput,
    ProviderSetup, SessionHits, SessionItem, SpaceInfo,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_opener::OpenerExt;

/// 后端 → 前端唯一的事件通道名。前端 `listen` 这个名字。
pub const EVENT_CHANNEL: &str = "agent://event";

struct AppState {
    service: Arc<AgentService>,
}

/// 事件信封。
///
/// `core::Event` 本身一个字段都没改（A-3 不破），这里只加一层壳：
/// 前端按 `session_id` 过滤，就不必再靠「busy 时禁止切会话」来保证不串台。
/// `seq` 用来发现丢帧——`emit` 失败时不会静默。
#[derive(Clone, Serialize)]
struct EventEnvelope {
    /// 这条事件属于哪个会话。极端情况下为 `null`（无落盘能力的纯内存装配）。
    session_id: Option<String>,
    seq: u64,
    event: Event,
}

#[derive(Serialize)]
struct Snapshot {
    mode: String,
    provider: String,
    model: String,
    workspace: String,
    /// 当前活动空间；`null` = 「任务」。
    active_space_id: Option<String>,
    messages: usize,
    pending_permissions: usize,
    /// 当前会话 id；新会话还没说过话时是 null。
    session_id: Option<String>,
    /// `ready` / `unconfigured` / `degraded`。
    ///
    /// 三态而不是一个 `note: string | null`：「没配」和「配错」在界面上
    /// 要给出完全不同的下一步动作，合并成一个字段就没法区分。
    provider_state: String,
    /// 仅 `degraded` 时有值。
    provider_reason: Option<String>,
    /// 空间清单读取时遇到的问题（读坏、版本过高）。不阻断使用。
    spaces_warning: Option<String>,
    /// 应用版本，显示在设置面板里。
    version: String,
}

/// 会话列表项。刻意不直接把 `SessionItem` 序列化出去——
/// 这一层的字段名就是前后端契约，改它要连着 `src/types.ts` 一起改。
#[derive(Serialize)]
struct SessionDto {
    id: String,
    title: String,
    messages: usize,
    updated_at_ms: u64,
    /// `null` = 「任务」。
    space_id: Option<String>,
    /// 该会话所属空间的工作目录。菜单里的路径小字与「打开文件夹」都看它。
    workspace: String,
    pinned: bool,
    archived: bool,
}

impl From<SessionItem> for SessionDto {
    fn from(item: SessionItem) -> Self {
        Self {
            id: item.id,
            title: item.title,
            messages: item.messages,
            updated_at_ms: item.updated_at_ms,
            space_id: item.space_id,
            workspace: item.workspace.display().to_string(),
            pinned: item.pinned,
            archived: item.archived,
        }
    }
}

#[derive(Serialize)]
struct SpaceDto {
    id: String,
    name: String,
    path: String,
    sessions: usize,
    /// 目录还在不在。盘符没挂载时是 `false`，界面要标灰。
    available: bool,
    created_at_ms: u64,
}

impl From<SpaceInfo> for SpaceDto {
    fn from(space: SpaceInfo) -> Self {
        Self {
            id: space.id,
            name: space.name,
            path: space.path.display().to_string(),
            sessions: space.sessions,
            available: space.available,
            created_at_ms: space.created_at_ms,
        }
    }
}

#[derive(Serialize)]
struct SearchHitDto {
    msg: usize,
    block: usize,
    role: String,
    snippet: String,
}

#[derive(Serialize)]
struct SessionHitsDto {
    session_id: String,
    title: String,
    space_id: Option<String>,
    updated_at_ms: u64,
    hits: Vec<SearchHitDto>,
}

impl From<SessionHits> for SessionHitsDto {
    fn from(group: SessionHits) -> Self {
        Self {
            session_id: group.session_id,
            title: group.title,
            space_id: group.space_id,
            updated_at_ms: group.updated_at_ms,
            hits: group
                .hits
                .into_iter()
                .map(|hit| SearchHitDto {
                    msg: hit.msg,
                    block: hit.block,
                    role: hit.role,
                    snippet: hit.snippet,
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct ChatReport {
    outcome: &'static str,
    turns: u32,
}

#[derive(Serialize)]
struct ModeList {
    current: String,
    all: Vec<String>,
}

#[derive(Serialize)]
struct DeleteReport {
    /// 搬回「任务」的会话数。
    moved: usize,
}

/// 发一句话，等这一轮跑完。
///
/// 过程不靠返回值传递，全靠事件。返回值只说明"为什么结束"。
#[tauri::command]
async fn chat(
    app: AppHandle,
    state: State<'_, AppState>,
    prompt: String,
) -> Result<ChatReport, String> {
    let service = Arc::clone(state.inner().service());

    // 事件信封需要会话 id，而 id 本来是"第一次落盘时才生成"。
    // 这里提前把 id 定下来（**不建文件**），整轮的事件才有一个稳定归属；
    // 否则前几个 delta 的 session_id 是 null，前端没法用同一个键过滤。
    let session_id = service.ensure_session_id();

    let (sink, mut rx) = EventSink::channel();

    // 事件泵：把 mpsc 收到的东西包成 envelope 再发给前端。
    let emitter = app.clone();
    let pump = tauri::async_runtime::spawn(async move {
        let mut seq = 0u64;
        while let Some(event) = rx.recv().await {
            let envelope = EventEnvelope {
                session_id: session_id.clone(),
                seq,
                event,
            };
            seq += 1;
            let _ = emitter.emit(EVENT_CHANNEL, &envelope);
        }
    });

    let result = service.chat(&prompt, &sink).await;

    // 关掉发送端，事件泵才会退出。漏了这一步这个任务会一直挂着。
    drop(sink);
    let _ = pump.await;

    match result {
        Ok(Outcome::Completed { turns }) => Ok(ChatReport {
            outcome: "completed",
            turns,
        }),
        Ok(Outcome::MaxTurns) => Ok(ChatReport {
            outcome: "max_turns",
            turns: 0,
        }),
        Ok(Outcome::Cancelled) => Ok(ChatReport {
            outcome: "cancelled",
            turns: 0,
        }),
        Err(error) => Err(error.to_string()),
    }
}

#[tauri::command]
async fn snapshot(state: State<'_, AppState>) -> Result<Snapshot, String> {
    let service = Arc::clone(state.inner().service());
    let provider_state = service.provider_state();
    Ok(Snapshot {
        mode: service.mode().as_str().to_string(),
        provider: service.provider_id().to_string(),
        model: service.model(),
        workspace: service.workspace().display().to_string(),
        active_space_id: service.active_space(),
        messages: service.message_count().await,
        pending_permissions: service.pending_permissions(),
        session_id: service.current_session(),
        provider_reason: provider_state.reason().map(str::to_string),
        provider_state: provider_state.as_str().to_string(),
        spaces_warning: service.spaces_warning(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[tauri::command]
fn modes(state: State<'_, AppState>) -> ModeList {
    ModeList {
        current: state.service.mode().as_str().to_string(),
        all: Mode::ALL.iter().map(|m| m.as_str().to_string()).collect(),
    }
}

#[tauri::command]
fn set_mode(state: State<'_, AppState>, mode: String) -> Result<String, String> {
    let parsed = Mode::parse(&mode).ok_or_else(|| format!("未知权限档位: {mode}"))?;
    state.service.set_mode(parsed);
    Ok(parsed.as_str().to_string())
}

#[tauri::command]
fn cancel(state: State<'_, AppState>) -> bool {
    state.service.cancel_current()
}

/// 前端对授权请求的答复。超时未答复按拒绝处理，所以这里返回 false 不算异常。
#[tauri::command]
fn resolve_permission(state: State<'_, AppState>, id: String, allowed: bool) -> bool {
    state.service.resolve_permission(&id, allowed)
}

#[tauri::command]
async fn history(state: State<'_, AppState>) -> Result<Vec<Message>, String> {
    let service = Arc::clone(state.inner().service());
    Ok(service.history().await)
}

/// 会话列表。`space_id` 缺省 = 全部空间；`include_archived` 缺省 = 不含归档。
#[tauri::command]
async fn list_sessions(
    state: State<'_, AppState>,
    space_id: Option<String>,
    include_archived: Option<bool>,
) -> Result<Vec<SessionDto>, String> {
    let service = Arc::clone(state.inner().service());
    let sessions = service
        .list_sessions(space_id.as_deref(), include_archived.unwrap_or(false))
        .await
        .map_err(|e| e.to_string())?;
    Ok(sessions.into_iter().map(SessionDto::from).collect())
}

/// 打开历史会话并把它整段历史交回前端。前端拿到就能直接渲染，不必再调 `history`。
#[tauri::command]
async fn open_session(state: State<'_, AppState>, id: String) -> Result<Vec<Message>, String> {
    let service = Arc::clone(state.inner().service());
    service.open_session(&id).await.map_err(|e| e.to_string())?;
    Ok(service.history().await)
}

/// 新建会话。`space_id` 缺省 = 「任务」。
#[tauri::command]
async fn new_session(
    state: State<'_, AppState>,
    space_id: Option<String>,
) -> Result<(), String> {
    let service = Arc::clone(state.inner().service());
    service.new_session_in(space_id.as_deref()).await;
    Ok(())
}

#[tauri::command]
async fn rename_session(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<String, String> {
    let service = Arc::clone(state.inner().service());
    service
        .rename_session(&id, &title)
        .await
        .map_err(|e| e.to_string())
}

/// **永久删除**。会话菜单里没有它，只在归档区作为「这条真不要了」暴露。
#[tauri::command]
async fn delete_session(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let service = Arc::clone(state.inner().service());
    service
        .delete_session(&id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn spaces(state: State<'_, AppState>) -> Result<Vec<SpaceDto>, String> {
    let service = Arc::clone(state.inner().service());
    Ok(service
        .spaces()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(SpaceDto::from)
        .collect())
}

#[tauri::command]
async fn create_space(
    state: State<'_, AppState>,
    name: String,
    path: String,
) -> Result<SpaceDto, String> {
    let service = Arc::clone(state.inner().service());
    service
        .create_space(&name, std::path::Path::new(&path))
        .await
        .map(SpaceDto::from)
        .map_err(|e| e.to_string())
}

/// 改名 / 换目录。两项都可缺省，只传要改的那个。
#[tauri::command]
async fn update_space(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    path: Option<String>,
) -> Result<SpaceDto, String> {
    let service = Arc::clone(state.inner().service());
    let path = path.as_deref().map(std::path::Path::new);
    service
        .update_space(&id, name.as_deref(), path)
        .await
        .map(SpaceDto::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_space(state: State<'_, AppState>, id: String) -> Result<DeleteReport, String> {
    let service = Arc::clone(state.inner().service());
    let moved = service.delete_space(&id).await.map_err(|e| e.to_string())?;
    Ok(DeleteReport { moved })
}

#[tauri::command]
async fn pin_session(
    state: State<'_, AppState>,
    id: String,
    pinned: bool,
) -> Result<bool, String> {
    let service = Arc::clone(state.inner().service());
    service
        .pin_session(&id, pinned)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn archive_session(
    state: State<'_, AppState>,
    id: String,
    archived: bool,
) -> Result<bool, String> {
    let service = Arc::clone(state.inner().service());
    service
        .archive_session(&id, archived)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn search_sessions(
    state: State<'_, AppState>,
    query: String,
    include_archived: Option<bool>,
) -> Result<Vec<SessionHitsDto>, String> {
    let service = Arc::clone(state.inner().service());
    Ok(service
        .search_sessions(&query, include_archived.unwrap_or(false))
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(SessionHitsDto::from)
        .collect())
}

/// 打开会话所属的工作目录。
///
/// 入参是 id 不是路径：路径由 app 层算。这样即使前端被注入，
/// 也没法让后端打开 `C:\Windows`——它只能打开「这条会话所在的目录」。
#[tauri::command]
async fn reveal_session(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    let service = Arc::clone(state.inner().service());
    let path = service
        .reveal_session(&id)
        .await
        .map_err(|e| e.to_string())?;
    let display = path.display().to_string();

    // 目录可能还没建（默认工作区首次启动）。先建再开，
    // 否则资源管理器会直接弹「找不到这个项目」。
    let _ = std::fs::create_dir_all(&path);
    app.opener()
        .open_path(display.clone(), None::<&str>)
        .map_err(|e| format!("打开 {} 失败: {e}", display))?;
    Ok(display)
}

#[derive(Serialize)]
struct ProviderOptionDto {
    id: String,
    default_model: String,
    needs_key: bool,
}

/// provider 面板的全部数据。
///
/// 这里逐字段搬运而不直接序列化 app 层的结构：字段名就是前后端契约，
/// 哪天 app 层改了内部名字，不该顺手把前端一起打断。
#[derive(Serialize)]
struct ProviderSetupDto {
    state: String,
    source: String,
    reason: Option<String>,
    kind: String,
    model: String,
    base_url: String,
    max_tokens: u32,
    config_path: String,
    env_key: Option<String>,
    template: String,
    catalog: Vec<ProviderOptionDto>,
}

impl From<ProviderSetup> for ProviderSetupDto {
    fn from(setup: ProviderSetup) -> Self {
        let state = setup.state.as_str().to_string();
        let reason = setup.state.reason().map(str::to_string);
        Self {
            state,
            source: setup.source.as_str().to_string(),
            reason,
            kind: setup.kind,
            model: setup.model,
            base_url: setup.base_url,
            max_tokens: setup.max_tokens,
            config_path: setup.config_path.display().to_string(),
            env_key: setup.env_key,
            template: setup.template,
            catalog: setup
                .catalog
                .into_iter()
                .map(|option| ProviderOptionDto {
                    id: option.id.to_string(),
                    default_model: option.default_model.to_string(),
                    needs_key: option.needs_key,
                })
                .collect(),
        }
    }
}

/// 表单提交上来的字段。`api_key` 缺省 = 保留原来那份，见 app 层同名结构。
#[derive(Deserialize)]
struct ProviderInputDto {
    kind: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

#[derive(Serialize)]
struct AppInfo {
    version: String,
    identifier: String,
    workspace: String,
    /// 工作区是怎么定的：`env` / `cwd` / `home`。
    workspace_origin: String,
    config_path: String,
    /// 更新检查地址。一期没有发布渠道，只能通过环境变量指过来；
    /// 为空时界面不显示"检查更新"——比编一个假链接诚实。
    update_url: Option<String>,
}

#[tauri::command]
fn provider_setup(state: State<'_, AppState>) -> ProviderSetupDto {
    state.service.provider_setup().into()
}

/// 保存 provider 配置并立即生效。
///
/// 不要求重启：填完 key 还要重启一次，是最容易被当成"保存没成功"的交互。
#[tauri::command]
fn save_provider(
    state: State<'_, AppState>,
    input: ProviderInputDto,
) -> Result<ProviderSetupDto, String> {
    let service = state.service();
    service
        .save_provider(&ProviderInput {
            kind: input.kind,
            model: input.model,
            api_key: input.api_key,
            base_url: input.base_url,
            timeout_secs: input.timeout_secs,
            max_retries: input.max_retries,
            stream: input.stream,
            max_tokens: input.max_tokens,
        })
        .map(ProviderSetupDto::from)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn app_info(app: AppHandle, state: State<'_, AppState>) -> AppInfo {
    let origin = {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        goldenagent_app::resolve_workspace(&cwd).1
    };
    AppInfo {
        version: app.package_info().version.to_string(),
        identifier: app.config().identifier.clone(),
        workspace: state.service.workspace().display().to_string(),
        workspace_origin: origin.as_str().to_string(),
        config_path: state.service.provider_setup().config_path.display().to_string(),
        update_url: std::env::var("GOLDENAGENT_UPDATE_URL")
            .ok()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty()),
    }
}

impl AppState {
    fn service(&self) -> &Arc<AgentService> {
        &self.service
    }
}

/// 工作区由 app 层决定（见 `workspace.rs` 的三条规则）。
///
/// 外壳只负责把 cwd 递过去：从开始菜单启动时 cwd 是安装目录，
/// 直接把 cwd 当工作区会让 Agent 的 jail 根变成程序自己所在的文件夹。
fn resolve_workspace() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    goldenagent_app::resolve_workspace(&cwd).0
}

/// 装配 + 启动时的三件小事：备好空间存储、收编旧版单文件会话、接上次没聊完的那条。
///
/// 放在启动里同步做完，是为了让前端挂载时拿到的 `snapshot` 就已经是"续跑后"的状态；
/// 如果改成后台任务，首屏会先闪一下空会话再跳变成历史。
fn build_service(workspace: &std::path::Path) -> AgentService {
    // 打包后首次启动时工作区可能还不存在（家目录下的默认目录）。
    // 不建的话，界面上显示的路径是个不存在的地方，第一次 list_dir 就会说"不存在"。
    // 建不出来（没权限）也不 panic：后面的报错会带着真实路径，比启动即崩好排查。
    let _ = std::fs::create_dir_all(workspace);

    let state_dir = workspace.join(".goldenagent");
    let config = AgentConfig::new(workspace).with_state_dir(&state_dir);

    // provider：环境变量 → .goldenagent/provider.json → 离线回显。
    // 装配失败不 panic，降级原因会随 snapshot 回到界面。
    let service = AgentService::auto(config, &state_dir.join("provider.json"));

    let _ = tauri::async_runtime::block_on(async {
        // 先备好清单与目录（含旧布局迁移），再收编 B10 的单文件会话，
        // 最后接上次——顺序错了 restore 会看不到刚收编进来的历史。
        if let Ok(Some(warning)) = service.bootstrap().await {
            eprintln!("[spaces] {warning}");
        }
        let _ = service.adopt_legacy(state_dir.join("session.jsonl")).await;
        service.restore().await
    });

    service
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let workspace = resolve_workspace();
    let service = build_service(&workspace);

    let state = AppState {
        service: Arc::new(service),
    };

    tauri::Builder::default()
        // 只用 Rust 侧的 `open_path`（见 reveal_session）。前端**没有**被授予
        // opener 的 command 权限，所以它没法自己让系统打开任意路径。
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            chat,
            snapshot,
            modes,
            set_mode,
            cancel,
            resolve_permission,
            history,
            list_sessions,
            open_session,
            new_session,
            rename_session,
            delete_session,
            spaces,
            create_space,
            update_space,
            delete_space,
            pin_session,
            archive_session,
            search_sessions,
            reveal_session,
            provider_setup,
            save_provider,
            app_info
        ])
        .run(tauri::generate_context!())
        .expect("GoldenAgent 启动失败");
}
