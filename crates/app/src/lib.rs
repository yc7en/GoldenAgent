//! 第 4 层：应用服务。
//!
//! 这一层只做装配和生命周期，不含业务判断：
//!
//! - 把 provider / tools / policy / config / workspace 拼成一个 [`AgentLoop`]；
//! - 持有会话，负责落盘与续跑；
//! - 实现 [`Approver`]，把授权变成"事件出去、答案回来"；
//! - 对外只暴露 `chat()` / `cancel()` / `set_mode()` 这类动作。
//!
//! 外壳（Tauri / CLI）只认识这一层。换壳不用改这里，改这里不用动壳。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;

use goldenagent_core::{Error, Result};
use goldenagent_loop::{AgentLoop, Cancellation, Config};
use goldenagent_policy::Policy;
use goldenagent_provider::{MockProvider, Provider, ProviderConfig};
use goldenagent_session::{Session, SessionStore};
use goldenagent_tools::Registry;
use tokio::sync::Mutex;

mod approver;
mod provider;
mod workspace;

pub use approver::EventApprover;
pub use provider::{
    ProviderInput, ProviderOption, ProviderSetup, ProviderSource, ProviderState,
};
pub use workspace::{resolve as resolve_workspace, WorkspaceOrigin};
// 外壳只需要认识这几个名字，不必自己再去依赖 core / loop / policy / session。
pub use goldenagent_core::{Event, Message};
pub use goldenagent_loop::{EventSink, Outcome};
pub use goldenagent_policy::Mode;
pub use goldenagent_session::SessionSummary;

/// 一次会话的装配参数。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 工具能碰的根目录。同时是路径越界的判定基准。
    pub workspace: PathBuf,
    pub model: String,
    pub system: String,
    pub max_tokens: u32,
    pub max_turns: u32,
    /// token 阈值，超过就压缩历史。`None` 关闭。
    pub compact_at: Option<usize>,
    pub compact_keep: usize,
    pub mode: Mode,
    /// 授权等待上限。超时按拒绝处理。
    pub approval_timeout: Duration,
    /// 会话目录。`None` 表示纯内存，不落盘、也不提供历史列表。
    pub session_dir: Option<PathBuf>,
}

impl AgentConfig {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            model: "mock".into(),
            system: DEFAULT_SYSTEM.into(),
            // 与 provider 侧的默认值保持一致：推理模型的思考也吃这份额度。
            max_tokens: goldenagent_provider::DEFAULT_MAX_TOKENS,
            max_turns: 12,
            compact_at: Some(48_000),
            compact_keep: 12,
            mode: Mode::Ask,
            approval_timeout: Duration::from_secs(120),
            session_dir: None,
        }
    }

    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_session_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.session_dir = Some(dir.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

const DEFAULT_SYSTEM: &str = "你是 GoldenAgent，一个运行在用户本机的编程助手。\
你可以：读文件、写文件、列目录、按内容搜索（grep）、按文件名查找（glob）、\
精确替换几行（apply_patch）、执行 shell 命令（bash）。\
做事之前先看再改：不确定路径就先 list_dir 或 glob，找代码先 grep，别猜。\
只改几行用 apply_patch，整份重写才用 write_file——后者会把没读到的内容一起冲掉。\
执行命令前想清楚它会不会动到工作区外的东西。回答用中文，简洁直接。";

/// provider 及它周围那些会变的东西。
///
/// 为什么打成一个包：换 provider 时这五项必须一起换（provider 实例、配置、
/// 状态、来源、生效模型名）。分开存会出现"provider 已经换了、model 还是旧的"
/// 这类中间态，而它们每一个都足以让请求发错地方。
struct ProviderRuntime {
    provider: Arc<dyn Provider>,
    config: ProviderConfig,
    /// 实际发给厂商的模型名。`new()` 时取 `AgentConfig.model`，
    /// `auto()` / `save_provider()` 时取配置里的有效模型。
    model: String,
    /// 单次回答的输出上限。跟着 provider 走而不是跟着 `AgentConfig` 走：
    /// 这是**厂商侧的额度**，改配置时就该立刻生效，不必重启。
    max_tokens: u32,
    state: ProviderState,
    source: ProviderSource,
    /// 配置文件路径。`auto()` 才有——纯离线装配不需要知道文件在哪。
    path: Option<PathBuf>,
    /// 环境变量里探测到的 key 变量名（不是值）。
    env_key: Option<String>,
}

pub struct AgentService {
    provider: RwLock<ProviderRuntime>,
    tools: Arc<Registry>,
    config: AgentConfig,
    /// 权限模式可以被前端随时改，所以不放进 AgentLoop。
    mode: RwLock<Mode>,
    session: Mutex<Session>,
    /// 会话目录。`None` = 纯内存模式（测试、临时会话）。
    store: Option<SessionStore>,
    /// 当前会话 id。**第一次落盘时才生成**——否则用户每点一次"新对话"
    /// 就在列表里留下一个空壳。
    ///
    /// 与 `session` 一起用时，永远先锁 `session` 再锁这个，不要反过来。
    session_id: StdMutex<Option<String>>,
    /// 当前正在跑的那一轮的取消标记。
    current_cancel: StdMutex<Option<Cancellation>>,
    approver: Arc<EventApprover>,
}

impl AgentService {
    pub fn new(config: AgentConfig, provider: Arc<dyn Provider>) -> Self {
        let approver = Arc::new(EventApprover::new(config.approval_timeout));
        let mode = config.mode;
        let store = config.session_dir.clone().map(SessionStore::new);
        let runtime = ProviderRuntime {
            provider,
            config: ProviderConfig::mock(),
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            // 直接给一个 provider 的调用方（测试、CLI）自己知道它在干什么，
            // 这里不替它判断"算不算配置好"。
            state: ProviderState::Ready,
            source: ProviderSource::None,
            path: None,
            env_key: None,
        };
        Self {
            provider: RwLock::new(runtime),
            tools: Arc::new(Registry::with_defaults()),
            config,
            mode: RwLock::new(mode),
            session: Mutex::new(Session::new()),
            store,
            session_id: StdMutex::new(None),
            current_cancel: StdMutex::new(None),
            approver,
        }
    }

    pub fn with_tools(mut self, tools: Registry) -> Self {
        self.tools = Arc::new(tools);
        self
    }

    /// 按「环境变量 → 配置文件 → 未配置」装配 provider。
    ///
    /// 环境变量优先是为了方便临时测试和 CI；桌面双击启动时进程拿不到
    /// shell 里的变量，所以有配置文件这条路。
    ///
    /// **任何一步失败都不 panic、不静默**：退回离线 provider，并把状态
    /// （`Unconfigured` 或 `Degraded { reason }`）交给界面。这两者必须分开——
    /// "没配"要引导用户去配，"配错了"要告诉用户哪里错。
    pub fn auto(mut config: AgentConfig, config_path: &Path) -> Self {
        let resolved = provider::resolve(config_path);
        let kind = resolved.config.kind;
        let source = resolved.source;

        let (provider, model, state): (Arc<dyn Provider>, String, ProviderState) =
            match resolved.config.build() {
                Ok(provider) => {
                    let model = resolved.config.effective_model().to_string();
                    (provider, model, resolved.state)
                }
                Err(error) => (
                    Arc::new(MockProvider::default()),
                    resolved.config.effective_model().to_string(),
                    ProviderState::Degraded {
                        reason: error.to_string(),
                    },
                ),
            };

        config.model = model.clone();
        // 额度跟着它将要发的请求走，所以在整份搬进 runtime 之前先取出来。
        let max_tokens = resolved.config.max_tokens;
        let mut service = Self::new(config, Arc::clone(&provider));

        // `new` 已经建了一份 runtime（它不知道配置从哪来），这里整份替换掉。
        // 独占所有权，用 `get_mut` 比 `write()` 诚实：这里没有并发。
        let runtime = ProviderRuntime {
            provider,
            config: resolved.config,
            model,
            max_tokens,
            state,
            source,
            path: Some(config_path.to_path_buf()),
            env_key: if source == ProviderSource::Env {
                Some(kind.api_key_env().to_string()).filter(|name| !name.is_empty())
            } else {
                None
            },
        };
        if let Ok(slot) = service.provider.get_mut() {
            *slot = runtime;
        }

        service
    }

    /// 装配 provider 时是否发生过降级，以及为什么。
    ///
    /// 保留这个窄接口是因为调用方多数时候只关心"有没有问题"；
    /// 要知道"到底是没配还是配错"用 [`AgentService::provider_setup`]。
    pub fn provider_note(&self) -> Option<String> {
        self.provider_state().reason().map(str::to_string)
    }

    /// 当前 provider 状态。界面靠它区分"没配"与"配错"。
    pub fn provider_state(&self) -> ProviderState {
        self.provider
            .read()
            .map(|runtime| runtime.state.clone())
            .unwrap_or(ProviderState::Unconfigured)
    }

    /// 界面要显示/编辑的全部 provider 信息（不含 api_key）。
    pub fn provider_setup(&self) -> ProviderSetup {
        let runtime = match self.provider.read() {
            Ok(runtime) => runtime,
            Err(poisoned) => poisoned.into_inner(),
        };

        ProviderSetup {
            state: runtime.state.clone(),
            source: runtime.source,
            kind: runtime.config.kind.as_str().to_string(),
            model: runtime.model.clone(),
            base_url: runtime.config.base_url.clone().unwrap_or_default(),
            max_tokens: runtime.max_tokens,
            config_path: runtime
                .path
                .clone()
                .unwrap_or_else(|| PathBuf::from(".goldenagent/provider.json")),
            env_key: runtime.env_key.clone(),
            template: provider::template_for(runtime.config.kind),
            catalog: provider::catalog(),
        }
    }

    /// 保存 provider 配置并立即生效。返回保存后的新状态。
    ///
    /// 不重启就生效是刻意的：让用户填完 key 还要重启一次，是最容易被
    /// 当成"保存失败"的交互。
    pub fn save_provider(&self, input: &ProviderInput) -> Result<ProviderSetup> {
        let (path, existing) = {
            let runtime = match self.provider.read() {
                Ok(runtime) => runtime,
                Err(poisoned) => poisoned.into_inner(),
            };
            let path = runtime.path.clone().ok_or_else(|| {
                Error::Config("当前服务没有绑定配置文件，无法保存（纯离线装配）".into())
            })?;
            (path, runtime.config.clone())
        };

        let config = provider::save(&path, input, Some(&existing))?;
        let provider = config.build()?;
        let model = config.effective_model().to_string();
        let max_tokens = config.max_tokens;
        let env_key = {
            let name = config.kind.api_key_env();
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        };

        {
            let mut runtime = match self.provider.write() {
                Ok(runtime) => runtime,
                Err(poisoned) => poisoned.into_inner(),
            };
            *runtime = ProviderRuntime {
                provider,
                config,
                model,
                max_tokens,
                state: ProviderState::Ready,
                source: ProviderSource::File,
                path: Some(path),
                env_key,
            };
        }

        Ok(self.provider_setup())
    }

    /// 实际会发给厂商的模型名。
    pub fn model(&self) -> String {
        self.provider
            .read()
            .map(|runtime| runtime.model.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().model.clone())
    }

    /// 一期默认装配：离线 provider。测试与无 key 环境用这条。
    ///
    /// 真实厂商走 [`AgentService::auto`]，`AgentService` 的其余部分与外壳都不变。
    pub fn offline(config: AgentConfig) -> Self {
        Self::new(config, Arc::new(MockProvider::default()))
    }

    pub fn workspace(&self) -> &Path {
        &self.config.workspace
    }

    pub fn provider_id(&self) -> &'static str {
        match self.provider.read() {
            Ok(runtime) => runtime.provider.id(),
            Err(poisoned) => poisoned.into_inner().provider.id(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode.read().map(|m| *m).unwrap_or_default()
    }

    /// 切换权限档位。返回是否有变化。
    pub fn set_mode(&self, mode: Mode) -> bool {
        match self.mode.write() {
            Ok(mut current) if *current != mode => {
                *current = mode;
                true
            }
            _ => false,
        }
    }

    /// 取消当前这一轮。只在轮次/工具边界生效。
    pub fn cancel_current(&self) -> bool {
        let cancel = self.current_cancel.lock().ok().and_then(|c| c.clone());
        match cancel {
            Some(cancel) => {
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// 前端对某次授权请求的答复。
    pub fn resolve_permission(&self, id: &str, allowed: bool) -> bool {
        self.approver.resolve(id, allowed)
    }

    pub fn pending_permissions(&self) -> usize {
        self.approver.pending_count()
    }

    pub async fn history(&self) -> Vec<Message> {
        self.session
            .lock()
            .await
            .messages()
            .to_vec()
    }

    pub async fn message_count(&self) -> usize {
        self.session.lock().await.len()
    }

    /// 开一段新会话：清历史、丢掉当前 id。旧会话仍在磁盘上，随时能翻回去。
    ///
    /// 不预建文件——第一次 `chat` 落盘时才真正生成 id 与文件。
    pub async fn new_session(&self) {
        self.session.lock().await.clear();
        if let Ok(mut slot) = self.session_id.lock() {
            *slot = None;
        }
    }

    /// 当前会话 id。新会话还没说过话时是 `None`。
    pub fn current_session(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|slot| slot.clone())
    }

    /// 历史会话列表，最近改过的在前。
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        self.session_store()?.list().await
    }

    /// 打开一条历史会话，返回它有多少条消息。
    ///
    /// 打开动作本身不写盘；只有下一次 `chat` 才会把（可能被压缩过的）历史写回去。
    pub async fn open_session(&self, id: &str) -> Result<usize> {
        let store = self.session_store()?;
        let loaded = store.load(id).await?;
        let count = loaded.len();

        // 锁序：先 session 再 session_id。反过来会和 chat 里的顺序相反，形成死锁。
        let mut session = self.session.lock().await;
        *session = loaded;
        drop(session);

        if let Ok(mut slot) = self.session_id.lock() {
            *slot = Some(id.to_string());
        }
        Ok(count)
    }

    pub async fn rename_session(&self, id: &str, title: &str) -> Result<String> {
        self.session_store()?.rename(id, title).await
    }

    /// 删掉一条历史会话。删的是当前会话时，顺手切到一段新会话。
    pub async fn delete_session(&self, id: &str) -> Result<bool> {
        let removed = self.session_store()?.delete(id).await?;
        if self.current_session().as_deref() == Some(id) {
            self.new_session().await;
        }
        Ok(removed)
    }

    /// 启动时接上次：打开最近一条会话。没有历史就什么都不做。
    ///
    /// 另外顺手把 B10 及以前的单文件 `session.jsonl` 收编进来——
    /// 老用户升级后历史不该凭空消失。
    pub async fn restore(&self) -> Result<bool> {
        let store = self.session_store()?;
        let latest = store.list().await?.into_iter().next();
        match latest {
            Some(summary) => {
                self.open_session(&summary.id).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// 迁移旧的单文件会话。返回迁移出来的会话 id（没有可迁移的就 `None`）。
    pub async fn adopt_legacy(&self, legacy: impl AsRef<Path>) -> Result<Option<String>> {
        self.session_store()?.adopt_legacy(legacy).await
    }

    fn session_store(&self) -> Result<&SessionStore> {
        self.store
            .as_ref()
            .ok_or_else(|| Error::Session("没有配置会话目录，历史功能不可用".into()))
    }

    /// 落盘。第一次调用时才分配 id。
    async fn persist_locked(&self, session: &Session) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };

        let id = match self.session_id.lock() {
            Ok(mut slot) => slot.get_or_insert_with(SessionStore::new_id).clone(),
            Err(_) => return Ok(()),
        };
        store.persist(&id, session).await
    }

    /// 跑一轮对话。进度只从 `events` 出去，历史只从 `session` 出来。
    pub async fn chat(&self, prompt: &str, events: &EventSink) -> Result<Outcome> {
        // 先把 provider 与模型名取出来再放开锁：整轮对话可能跑几分钟，
        // 期间不该阻塞用户去改配置。
        let (provider, model, max_tokens) = self.agent_parts();

        let policy = Policy::new(self.mode(), &self.config.workspace);
        let agent = AgentLoop::new(
            provider,
            Arc::clone(&self.tools),
            policy,
            Config {
                model,
                system: self.config.system.clone(),
                max_tokens,
                max_turns: self.config.max_turns,
                compact_at: self.config.compact_at,
                compact_keep: self.config.compact_keep,
            },
        );

        let cancel = Cancellation::new();
        if let Ok(mut slot) = self.current_cancel.lock() {
            *slot = Some(cancel.clone());
        }

        let mut session = self.session.lock().await;
        let outcome = agent
            .run(&mut session, prompt, events, self.approver.as_ref(), &cancel)
            .await;

        if let Ok(mut slot) = self.current_cancel.lock() {
            *slot = None;
        }

        // 出错也要落盘：已经跑完的轮次不该因为最后一轮失败而丢掉。
        if let Err(error) = self.persist_locked(&session).await {
            if outcome.is_ok() {
                return Err(error);
            }
        }

        outcome
    }

    /// 取当前 provider、模型名与输出额度。锁中毒时继续用里面的值——
    /// 这里没有"一半写坏"的可能：runtime 是整份替换的。
    fn agent_parts(&self) -> (Arc<dyn Provider>, String, u32) {
        match self.provider.read() {
            Ok(runtime) => (
                Arc::clone(&runtime.provider),
                runtime.model.clone(),
                runtime.max_tokens,
            ),
            Err(poisoned) => {
                let runtime = poisoned.into_inner();
                (
                    Arc::clone(&runtime.provider),
                    runtime.model.clone(),
                    runtime.max_tokens,
                )
            }
        }
    }
}
