//! GoldenAgent 桌面外壳。
//!
//! 这个 crate 只做三件事：
//!
//! 1. 建 `AppState`（持 `AgentService`）；
//! 2. 把前端 `invoke` 原样转成 service 调用；
//! 3. 把 service 的事件流原样转成 `emit("agent://event")`。
//!
//! **这里不许出现业务判断**：不放权限逻辑、不放重试策略、不读会话文件。
//! 想加功能，加在下面某一层里，别加在这儿——否则半年后它就会长成第二个引擎。

use std::sync::Arc;

use goldenagent_app::{
    AgentConfig, AgentService, EventSink, Message, Mode, Outcome, ProviderInput, ProviderSetup,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// 后端 → 前端唯一的事件通道名。前端 `listen` 这个名字。
pub const EVENT_CHANNEL: &str = "agent://event";

struct AppState {
    service: Arc<AgentService>,
}

#[derive(Serialize)]
struct Snapshot {
    mode: String,
    provider: String,
    model: String,
    workspace: String,
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
    /// 应用版本，显示在设置面板里。
    version: String,
}

/// 会话列表项。刻意不直接把 `SessionSummary` 序列化出去——
/// 这一层的字段名就是前后端契约，改它要连着 `src/types.ts` 一起改。
#[derive(Serialize)]
struct SessionDto {
    id: String,
    title: String,
    messages: usize,
    updated_at_ms: u64,
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
    let (sink, mut rx) = EventSink::channel();

    // 事件泵：把 mpsc 收到的东西原样转成前端能 listen 的事件。
    let emitter = app.clone();
    let pump = tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            let _ = emitter.emit(EVENT_CHANNEL, &event);
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
        messages: service.message_count().await,
        pending_permissions: service.pending_permissions(),
        session_id: service.current_session(),
        provider_reason: provider_state.reason().map(str::to_string),
        provider_state: provider_state.as_str().to_string(),
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

#[tauri::command]
async fn list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionDto>, String> {
    let service = Arc::clone(state.inner().service());
    let sessions = service.list_sessions().await.map_err(|e| e.to_string())?;
    Ok(sessions
        .into_iter()
        .map(|s| SessionDto {
            id: s.id,
            title: s.title,
            messages: s.messages,
            updated_at_ms: s.updated_at_ms,
        })
        .collect())
}

/// 打开历史会话并把它整段历史交回前端。前端拿到就能直接渲染，不必再调 `history`。
#[tauri::command]
async fn open_session(state: State<'_, AppState>, id: String) -> Result<Vec<Message>, String> {
    let service = Arc::clone(state.inner().service());
    service.open_session(&id).await.map_err(|e| e.to_string())?;
    Ok(service.history().await)
}

#[tauri::command]
async fn new_session(state: State<'_, AppState>) -> Result<(), String> {
    let service = Arc::clone(state.inner().service());
    service.new_session().await;
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

#[tauri::command]
async fn delete_session(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let service = Arc::clone(state.inner().service());
    service
        .delete_session(&id)
        .await
        .map_err(|e| e.to_string())
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

/// 装配 + 启动时的两件小事：收编旧版单文件会话、接上次没聊完的那条。
///
/// 放在启动里同步做完，是为了让前端挂载时拿到的 `snapshot` 就已经是"续跑后"的状态；
/// 如果改成后台任务，首屏会先闪一下空会话再跳变成历史。
fn build_service(workspace: &std::path::Path) -> AgentService {
    // 打包后首次启动时工作区可能还不存在（家目录下的默认目录）。
    // 不建的话，界面上显示的路径是个不存在的地方，第一次 list_dir 就会说"不存在"。
    // 建不出来（没权限）也不 panic：后面的报错会带着真实路径，比启动即崩好排查。
    let _ = std::fs::create_dir_all(workspace);

    let state_dir = workspace.join(".goldenagent");
    let config = AgentConfig::new(workspace).with_session_dir(state_dir.join("sessions"));

    // provider：环境变量 → .goldenagent/provider.json → 离线回显。
    // 装配失败不 panic，降级原因会随 snapshot 回到界面。
    let service = AgentService::auto(config, &state_dir.join("provider.json"));

    let _ = tauri::async_runtime::block_on(async {
        service.adopt_legacy(state_dir.join("session.jsonl")).await?;
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
            provider_setup,
            save_provider,
            app_info
        ])
        .run(tauri::generate_context!())
        .expect("GoldenAgent 启动失败");
}
