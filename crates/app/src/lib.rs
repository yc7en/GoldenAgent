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
use goldenagent_provider::{
    MockProvider, Provider, ProviderConfig, MAX_MAX_TOKENS, MIN_MAX_TOKENS,
};
use goldenagent_session::{Session, SessionCatalog, SessionStore, SpaceStore};
use goldenagent_tools::skill::{self, MAX_PROMPT_BYTES};
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
pub use goldenagent_session::{
    SessionHits, SessionSummary as SessionEntry, DEFAULT_SPACE_DIR, MAX_EXTRA_ROOTS,
};

/// 一次会话的装配参数。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 工具能碰的根目录。**这是默认工作区**：没有空间时（「任务」）就用它。
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
    /// 应用状态目录（`.goldenagent`）：放 `spaces.json` 与 `sessions/`。
    /// 它同时是空间路径的禁区——Agent 不该能改自己的会话与明文 key。
    pub state_dir: Option<PathBuf>,
    /// 技能目录（只读指令包）。`None` 表示这台机器不启用技能。
    ///
    /// 默认跟着状态目录走（`<state>/skills`），与 `spaces.json`、`sessions/` 并列——
    /// 它们是同一类东西：属于这台机器、不属于某个空间。
    pub skills_dir: Option<PathBuf>,
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
            state_dir: None,
            skills_dir: None,
        }
    }

    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_session_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        self.state_dir = dir.parent().map(Path::to_path_buf);
        self.session_dir = Some(dir);
        // 显式给了会话目录也顺手挂上同级的 skills/：测试与 CLI 不会去调 with_state_dir，
        // 少了这一步就会出现"应用里有技能、测试里没有"的双重行为。
        self.skills_dir = self.state_dir.as_ref().map(|s| s.join(SKILLS_DIR));
        self
    }

    /// 按应用状态目录装配：会话在 `<state>/sessions/`，空间清单在 `<state>/spaces.json`。
    pub fn with_state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        self.session_dir = Some(dir.join(goldenagent_session::SESSIONS_DIR));
        self.skills_dir = Some(dir.join(SKILLS_DIR));
        self.state_dir = Some(dir);
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

/// 技能目录名。与 `sessions/`、`spaces.json` 并列放在状态目录下——
/// 它们都属于「这台机器」，不属于某个空间。
const SKILLS_DIR: &str = "skills";

/// 把「可用技能」拼进系统提示。
///
/// 只放 id + 一句话，正文由模型自己 `use_skill` 去读：N 个技能各几 KB，
/// 全量注入会把上下文吃光，而这正是技能要解决的问题（渐进披露）。
///
/// **没有技能目录时原样返回**——不是"返回一个空段落"，
/// 保证不装技能的用户拿到的提示词与上一版逐字节相同。
fn system_with_skills(system: &str, dir: Option<&Path>) -> String {
    let Some(dir) = dir else {
        return system.to_string();
    };
    let report = skill::scan(dir);
    if report.skills.is_empty() && report.skipped.is_empty() {
        return system.to_string();
    }

    let mut out = String::with_capacity(system.len() + 512);
    out.push_str(system);

    if !report.skills.is_empty() {
        out.push_str("\n\n可以按需调用的技能（需要时先调 use_skill 读正文，name 传冒号前的那个 id）：\n");
        let mut listed = 0usize;
        for item in &report.skills {
            let line = item.outline();
            if out.len() + line.len() + 1 > MAX_PROMPT_BYTES {
                break;
            }
            out.push_str(&line);
            out.push('\n');
            listed += 1;
        }
        if listed < report.skills.len() {
            out.push_str(&format!(
                "（另有 {} 个技能因清单长度上限未列出）\n",
                report.skills.len() - listed
            ));
        }
        out.push_str("技能里写的操作同样受当前权限档约束，不会被自动放行；不确定就按它写的顺序做，别凭印象编步骤。");
    }

    // 装失败的目录也要说出来：用户放了个技能却不生效时，
    // 「为什么」比「有没有」更难查，让模型能直接答上来。
    if !report.skipped.is_empty() {
        let listed: Vec<String> = report
            .skipped
            .iter()
            .take(3)
            .map(|s| format!("{}（{}）", s.id, s.reason))
            .collect();
        out.push_str(&format!(
            "\n另有 {} 个技能目录未加载：{}。",
            report.skipped.len(),
            listed.join("；")
        ));
    }

    out
}

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

/// 当前在哪个空间干活。
///
/// `roots` 是工具 jail 允许的全部根：第一个是主目录，其余是空间附加的目录。
/// 它随空间切换，但**一轮对话内绝不变化**（A-12）：`chat()` 开头把整份根集合
/// 快照成局部变量，整轮不再看锁。
#[derive(Debug, Clone)]
struct ActiveWorkspace {
    /// `None` = 「任务」（默认空间）。
    space_id: Option<String>,
    roots: Vec<PathBuf>,
}

impl ActiveWorkspace {
    fn new(space_id: Option<String>, primary: PathBuf, extra: Vec<PathBuf>) -> Self {
        let mut roots = Vec::with_capacity(1 + extra.len());
        roots.push(primary);
        roots.extend(extra);
        Self { space_id, roots }
    }

    /// 主目录。`bash` 的 cwd、相对路径的落点都是它。
    fn primary(&self) -> PathBuf {
        self.roots.first().cloned().unwrap_or_default()
    }
}

/// 一个空间在界面上的样子。
#[derive(Debug, Clone)]
pub struct SpaceInfo {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    /// 主目录之外、同一空间内也可读写的目录。
    pub extra_paths: Vec<PathBuf>,
    pub created_at_ms: u64,
    /// 该空间下的会话数（含已归档）。
    pub sessions: usize,
    /// 目录还在不在。盘符没挂载时是 `false`。
    pub available: bool,
}

/// 一条会话在界面上的样子。比存储层多一个 `workspace`——
/// 会话层不知道工作目录，那个信息由空间清单补齐。
#[derive(Debug, Clone)]
pub struct SessionItem {
    pub id: String,
    pub title: String,
    pub messages: usize,
    pub updated_at_ms: u64,
    pub space_id: Option<String>,
    pub workspace: PathBuf,
    pub pinned: bool,
    pub archived: bool,
}

pub struct AgentService {
    provider: RwLock<ProviderRuntime>,
    tools: Arc<Registry>,
    config: AgentConfig,
    /// 权限模式可以被前端随时改，所以不放进 AgentLoop。
    mode: RwLock<Mode>,
    session: Mutex<Session>,
    /// 会话目录（按空间分区）。`None` = 纯内存模式（测试、临时会话）。
    catalog: Option<SessionCatalog>,
    /// 空间清单。与 `catalog` 同生同灭。
    space_store: Option<SpaceStore>,
    /// 启动时清单读取遇到的问题。读坏不是致命错误，但界面要知道。
    spaces_warning: StdMutex<Option<String>>,
    /// 当前会话 id。**第一次落盘时才生成**——否则用户每点一次"新对话"
    /// 就在列表里留下一个空壳。
    ///
    /// 与 `session` 一起用时，永远先锁 `session` 再锁这个，不要反过来。
    session_id: StdMutex<Option<String>>,
    /// 当前正在跑的那一轮的取消标记。它同时是"忙不忙"的唯一判据。
    current_cancel: StdMutex<Option<Cancellation>>,
    /// 活动工作区。锁序：**active → session → session_id**，新的锁放最外层。
    active: RwLock<ActiveWorkspace>,
    approver: Arc<EventApprover>,
}

impl AgentService {
    pub fn new(config: AgentConfig, provider: Arc<dyn Provider>) -> Self {
        let approver = Arc::new(EventApprover::new(config.approval_timeout));
        let mode = config.mode;
        let catalog = config.session_dir.clone().map(SessionCatalog::new);
        // 会话目录可以自定义（测试里是 `.sessions`），所以不能简单按
        // `<state>/sessions` 推导，否则清单说一套、会话在另一套。
        let space_store = config.state_dir.clone().map(|dir| match &config.session_dir {
            Some(sessions) => SpaceStore::with_sessions_root(dir, sessions.clone()),
            None => SpaceStore::new(dir),
        });
        let active = ActiveWorkspace::new(None, config.workspace.clone(), Vec::new());
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
            catalog,
            space_store,
            spaces_warning: StdMutex::new(None),
            session_id: StdMutex::new(None),
            current_cancel: StdMutex::new(None),
            active: RwLock::new(active),
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
            max_tokens_min: MIN_MAX_TOKENS,
            max_tokens_max: MAX_MAX_TOKENS,
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

    /// 当前**活动**工作区的主目录。没有空间时就是启动时解析出的默认工作区。
    pub fn workspace(&self) -> PathBuf {
        match self.active.read() {
            Ok(active) => active.primary(),
            Err(poisoned) => poisoned.into_inner().primary(),
        }
    }

    /// 当前活动工作区的全部根（主目录在前，附加目录在后）。
    ///
    /// 这是 A-12 快照的单位：`chat()` 取一次，整轮用它，而不是每次工具调用
    /// 都重新读锁——那样中途切空间会让同一轮的前后两步落在不同目录里。
    pub fn roots(&self) -> Vec<PathBuf> {
        match self.active.read() {
            Ok(active) => active.roots.clone(),
            Err(poisoned) => poisoned.into_inner().roots.clone(),
        }
    }

    /// 当前活动空间。`None` = 「任务」。
    pub fn active_space(&self) -> Option<String> {
        match self.active.read() {
            Ok(active) => active.space_id.clone(),
            Err(poisoned) => poisoned.into_inner().space_id.clone(),
        }
    }

    /// 是否有一轮正在跑。切空间/切会话/删空间都要先问它（A-12）。
    pub fn is_busy(&self) -> bool {
        self.current_cancel
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
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

    // ───────────────────────── 空间 ─────────────────────────

    /// 启动时备好状态目录、清单与旧布局迁移。返回清单问题（没有就是 `None`）。
    pub async fn bootstrap(&self) -> Result<Option<String>> {
        // 技能目录先建出来。空目录本身不做任何事，但它让用户在文件管理器里
        // **看到**该往哪儿放——否则"目录即安装"就等于没说安装位置。
        // 建不出来不阻断启动：扫描时目录不存在只会得到空清单。
        if let Some(dir) = &self.config.skills_dir {
            let _ = tokio::fs::create_dir_all(dir).await;
        }

        let Some(store) = &self.space_store else {
            return Ok(None);
        };
        let warning = store.bootstrap().await?;
        if let Ok(mut slot) = self.spaces_warning.lock() {
            *slot = warning.clone();
        }
        Ok(warning)
    }

    /// 清单读取时遇到的问题。界面在设置页显示它，但不阻断使用。
    pub fn spaces_warning(&self) -> Option<String> {
        self.spaces_warning
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }

    /// 全部空间（不含「任务」——它不是清单项，是默认值）。
    pub async fn spaces(&self) -> Result<Vec<SpaceInfo>> {
        let store = self.space_store()?;
        let catalog = store.catalog();
        let mut out = Vec::new();
        for space in store.list().await? {
            let sessions = catalog
                .list(Some(&space.id), true)
                .await
                .map(|list| list.len())
                .unwrap_or(0);
            out.push(SpaceInfo {
                available: SpaceStore::is_available(&space),
                sessions,
                id: space.id,
                name: space.name,
                path: space.path,
                extra_paths: space.extra_paths,
                created_at_ms: space.created_at_ms,
            });
        }
        Ok(out)
    }

    /// 新建空间。名字查重在存储层，这里只做「不能指向应用状态目录」这道安全闸。
    ///
    /// 安全闸对附加目录同样生效：主目录指着项目目录、附加目录指着
    /// `.goldenagent/sessions` 一样是 Agent 能改自己的会话记录。
    pub async fn create_space(
        &self,
        name: &str,
        path: &Path,
        extra: &[PathBuf],
    ) -> Result<SpaceInfo> {
        self.guard_space_path(path)?;
        for dir in extra {
            self.guard_space_path(dir)?;
        }
        let store = self.space_store()?;
        let space = store.create(name, path, extra).await?;
        Ok(SpaceInfo {
            available: true,
            sessions: 0,
            id: space.id,
            name: space.name,
            path: space.path,
            extra_paths: space.extra_paths,
            created_at_ms: space.created_at_ms,
        })
    }

    /// 改名 / 换目录 / 换附加目录。活动空间换了任一目录时必须空闲——否则这一轮
    /// 会在旧目录里跑完，而下一轮突然换到新目录，同一个任务的读写跨越两个地方。
    pub async fn update_space(
        &self,
        id: &str,
        name: Option<&str>,
        path: Option<&Path>,
        extra: Option<&[PathBuf]>,
    ) -> Result<SpaceInfo> {
        if path.is_some() || extra.is_some() {
            if let Some(path) = path {
                self.guard_space_path(path)?;
            }
            if let Some(extra) = extra {
                for dir in extra {
                    self.guard_space_path(dir)?;
                }
            }
            if self.active_space().as_deref() == Some(id) && self.is_busy() {
                return Err(Error::Session(
                    "这一轮还在跑，先停下再改空间目录".into(),
                ));
            }
        }

        let store = self.space_store()?;
        let space = store.update(id, name, path, extra).await?;

        // 活动空间的目录变了，活动工作区跟着变；否则界面切换了、工具还在老地方。
        if self.active_space().as_deref() == Some(id) {
            if let Ok(mut active) = self.active.write() {
                *active = ActiveWorkspace::new(
                    Some(space.id.clone()),
                    space.path.clone(),
                    space.extra_paths.clone(),
                );
            }
        }

        let sessions = store
            .catalog()
            .list(Some(id), true)
            .await
            .map(|list| list.len())
            .unwrap_or(0);
        Ok(SpaceInfo {
            available: SpaceStore::is_available(&space),
            sessions,
            id: space.id,
            name: space.name,
            path: space.path,
            extra_paths: space.extra_paths,
            created_at_ms: space.created_at_ms,
        })
    }

    /// 删除空间：会话移回「任务」，只删应用状态目录里的东西。返回搬走的会话数。
    pub async fn delete_space(&self, id: &str) -> Result<usize> {
        if self.is_busy() {
            return Err(Error::Session("这一轮还在跑，先停下再删空间".into()));
        }
        let store = self.space_store()?;
        let moved = store.delete(id).await?;

        // 正在这个空间里：切回「任务」，并把当前会话重新定位（它已经被搬过去了）。
        if self.active_space().as_deref() == Some(id) {
            if let Ok(mut active) = self.active.write() {
                *active = ActiveWorkspace::new(None, self.config.workspace.clone(), Vec::new());
            }
            if let Some(current) = self.current_session() {
                // 会话已经被搬到「任务」目录；重新打开一次，让 session_id 与归属对齐。
                if self.open_session(&current).await.is_err() {
                    self.new_session_in(None).await;
                }
            }
        }
        Ok(moved)
    }

    /// 切换活动空间。运行中拒绝（A-12）。
    pub async fn set_active_space(&self, id: Option<&str>) -> Result<()> {
        if self.is_busy() {
            return Err(Error::Session("这一轮还在跑，先停下再切空间".into()));
        }
        let roots = self.roots_for(id).await?;
        if let Ok(mut active) = self.active.write() {
            *active = ActiveWorkspace {
                space_id: id.map(str::to_string),
                roots,
            };
        }
        Ok(())
    }

    /// 「打开文件夹」的落点：由后端算，不接受前端传路径。
    ///
    /// 会话 → 它所属空间的工作目录；这样不会出现「前端传 `C:\Windows` 让后端打开」
    /// 这类越权面。
    pub async fn reveal_session(&self, id: &str) -> Result<PathBuf> {
        let located = self
            .catalog()?
            .locate(id)
            .await?
            .ok_or_else(|| Error::Session(format!("会话 {id} 不存在")))?;
        Ok(self
            .roots_for(located.space_id.as_deref())
            .await?
            .into_iter()
            .next()
            .unwrap_or_else(|| self.config.workspace.clone()))
    }

    pub async fn reveal_space(&self, id: &str) -> Result<PathBuf> {
        let space = self
            .space_store()?
            .get(id)
            .await?
            .ok_or_else(|| Error::Session(format!("空间 {id} 不存在")))?;
        Ok(space.path)
    }

    // ───────────────────────── 会话 ─────────────────────────

    /// 开一段新会话：清历史、丢掉当前 id。旧会话仍在磁盘上，随时能翻回去。
    ///
    /// 不预建文件——第一次 `chat` 落盘时才真正生成 id 与文件。
    pub async fn new_session(&self) {
        self.new_session_in(None).await
    }

    /// 在指定空间里开新会话，并把它设为活动空间。
    pub async fn new_session_in(&self, space_id: Option<&str>) {
        if let Ok(mut active) = self.active.write() {
            // 空间不存在时不要留一个指向不存在目录的活动工作区：
            // 回落到默认工作区，比让第一次工具调用报「路径不存在」好排查。
            *active = match space_id {
                None => ActiveWorkspace::new(None, self.config.workspace.clone(), Vec::new()),
                Some(id) => match self.roots_for_blocking(Some(id)) {
                    Some(roots) => ActiveWorkspace {
                        space_id: Some(id.to_string()),
                        roots,
                    },
                    None => ActiveWorkspace::new(
                        Some(id.to_string()),
                        self.config.workspace.clone(),
                        Vec::new(),
                    ),
                },
            };
        }

        self.session.lock().await.clear();
        if let Ok(mut slot) = self.session_id.lock() {
            *slot = None;
        }
    }

    /// 当前会话 id。新会话还没说过话时是 `None`。
    pub fn current_session(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|slot| slot.clone())
    }

    /// 确保当前会话有 id，返回它。
    ///
    /// 事件信封要一个稳定的会话标识，而 id 本来要等第一次落盘才生成。
    /// 这里提前定下 id 但**不建文件**：空会话仍然不会出现在列表里，
    /// 只是「id 已经许出去了」。
    pub fn ensure_session_id(&self) -> Option<String> {
        let mut slot = self.session_id.lock().ok()?;
        if slot.is_none() && self.catalog.is_some() {
            *slot = Some(SessionStore::new_id());
        }
        slot.clone()
    }

    /// 历史会话列表，最近改过的在前。
    ///
    /// `space_id == None` → 全部空间；`Some(id)` → 只看这个空间。
    pub async fn list_sessions(
        &self,
        space_id: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionItem>> {
        let catalog = self.catalog()?;
        let summaries = catalog.list(space_id, include_archived).await?;

        // 空间清单只读一次：每条会话都去查一遍会让侧栏首屏变成 N 次磁盘读。
        let paths = self.space_paths().await?;
        Ok(summaries
            .into_iter()
            .map(|summary| {
                let workspace = summary
                    .space_id
                    .as_ref()
                    .and_then(|id| paths.get(id).cloned())
                    .unwrap_or_else(|| self.config.workspace.clone());
                SessionItem {
                    id: summary.id,
                    title: summary.title,
                    messages: summary.messages,
                    updated_at_ms: summary.updated_at_ms,
                    space_id: summary.space_id,
                    workspace,
                    pinned: summary.pinned,
                    archived: summary.archived,
                }
            })
            .collect())
    }

    /// 打开一条历史会话，返回它有多少条消息。
    ///
    /// **事务化**（A-11）：定位归属 → 读盘 → 检查忙闲 → 切工作区 → 替换内存会话。
    /// 顺序是刻意的：
    /// - 读盘在忙闲检查之前，因为「运行中切换」报错时不该先做一次无谓的磁盘读；
    /// - 切工作区在替换会话之前，且读失败时**什么都不改**，绝不留半状态。
    pub async fn open_session(&self, id: &str) -> Result<usize> {
        let catalog = self.catalog()?;
        let located = catalog
            .locate(id)
            .await?
            .ok_or_else(|| Error::Session(format!("会话 {id} 不存在")))?;

        let loaded = Session::load(&located.path).await?;
        let count = loaded.len();

        if self.is_busy() {
            return Err(Error::Session(
                "这一轮还在跑，先停下或等它结束再切会话".into(),
            ));
        }

        // 空间可能已经在清单里被删、但会话文件还没搬（异常中断）。
        // 这时按默认工作区打开，至少历史还能看，不会「点得开却报错」。
        let roots = self
            .roots_for(located.space_id.as_deref())
            .await
            .unwrap_or_else(|_| vec![self.config.workspace.clone()]);

        if let Ok(mut active) = self.active.write() {
            *active = ActiveWorkspace {
                space_id: located.space_id.clone(),
                roots,
            };
        }

        // 锁序：active 已在上面写过又放开，这里是 session → session_id。
        let mut session = self.session.lock().await;
        *session = loaded;
        drop(session);

        if let Ok(mut slot) = self.session_id.lock() {
            *slot = Some(id.to_string());
        }
        Ok(count)
    }

    pub async fn rename_session(&self, id: &str, title: &str) -> Result<String> {
        self.store_for(id).await?.rename(id, title).await
    }

    /// 删掉一条历史会话（归档区的「永久删除」）。删的是当前会话时，顺手开一段新的。
    pub async fn delete_session(&self, id: &str) -> Result<bool> {
        let store = self.store_for(id).await?;
        let removed = store.delete(id).await?;
        if self.current_session().as_deref() == Some(id) {
            let space = self.active_space();
            self.new_session_in(space.as_deref()).await;
        }
        Ok(removed)
    }

    /// 置顶 / 取消置顶。只写标记，不动内存会话，运行中也允许。
    pub async fn pin_session(&self, id: &str, pinned: bool) -> Result<bool> {
        self.store_for(id).await?.set_pinned(id, pinned).await
    }

    /// 归档 / 取消归档。归档当前会话会切到同空间的一段新会话。
    pub async fn archive_session(&self, id: &str, archived: bool) -> Result<bool> {
        if archived && self.current_session().as_deref() == Some(id) && self.is_busy() {
            return Err(Error::Session("这一轮还在跑，先停下再归档当前会话".into()));
        }

        let store = self.store_for(id).await?;
        let result = store.set_archived(id, archived).await?;

        if archived && self.current_session().as_deref() == Some(id) {
            let space = self.active_space();
            self.new_session_in(space.as_deref()).await;
        }
        Ok(result)
    }

    /// 跨全部空间的正文搜索。归档会话默认不参与。
    pub async fn search_sessions(
        &self,
        query: &str,
        include_archived: bool,
    ) -> Result<Vec<SessionHits>> {
        self.catalog()?.search(query, include_archived).await
    }

    /// 启动时接上次：打开最近一条**未归档**会话（含它所在的空间，A-11）。
    pub async fn restore(&self) -> Result<bool> {
        let catalog = self.catalog()?;

        // 兜底迁移：`restore()` 是「开程序」这条路上的最后一步，也是唯一
        // 一定会被调到的地方（bootstrap 由外壳调用，测试/CLI 未必）。
        // 一次 readdir 的代价，换「旧布局一定能被看见」。
        catalog.migrate_flat().await.ok();

        let latest = catalog
            .list(None, false)
            .await?
            .into_iter()
            .find(|summary| !summary.archived);
        match latest {
            Some(summary) => {
                self.open_session(&summary.id).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// 迁移旧的单文件会话（B10 及以前）。返回迁移出来的会话 id。
    pub async fn adopt_legacy(&self, legacy: impl AsRef<Path>) -> Result<Option<String>> {
        self.catalog()?.store(None).adopt_legacy(legacy).await
    }

    // ───────────────────────── 内部 ─────────────────────────

    fn catalog(&self) -> Result<&SessionCatalog> {
        self.catalog
            .as_ref()
            .ok_or_else(|| Error::Session("没有配置会话目录，历史功能不可用".into()))
    }

    fn space_store(&self) -> Result<&SpaceStore> {
        self.space_store
            .as_ref()
            .ok_or_else(|| Error::Session("没有配置状态目录，空间功能不可用".into()))
    }

    /// 找一条会话所在的存储句柄。改名/置顶/归档/删除都要先定位。
    async fn store_for(&self, id: &str) -> Result<SessionStore> {
        let catalog = self.catalog()?;
        let located = catalog
            .locate(id)
            .await?
            .ok_or_else(|| Error::Session(format!("会话 {id} 不存在")))?;
        Ok(catalog.store(located.space_id.as_deref()))
    }

    /// 空间 id → 全部根目录（主目录在前）。`None` 就是默认工作区。
    async fn roots_for(&self, space_id: Option<&str>) -> Result<Vec<PathBuf>> {
        match space_id {
            None => Ok(vec![self.config.workspace.clone()]),
            Some(id) => {
                let space = self
                    .space_store()?
                    .get(id)
                    .await?
                    .ok_or_else(|| Error::Session(format!("空间 {id} 不存在")))?;
                Ok(space.roots())
            }
        }
    }

    /// `new_session_in` 是 async 上下文里的同步函数，不能 await，只能同步读清单。
    fn roots_for_blocking(&self, space_id: Option<&str>) -> Option<Vec<PathBuf>> {
        let id = space_id?;
        let store = self.space_store.as_ref()?;
        let file = std::fs::read_to_string(store.manifest_path()).ok()?;
        let value: serde_json::Value = serde_json::from_str(&file).ok()?;
        let entry = value
            .get("spaces")?
            .as_array()?
            .iter()
            .find(|space| space.get("id").and_then(|v| v.as_str()) == Some(id))?;

        let primary = PathBuf::from(entry.get("path")?.as_str()?);
        let mut roots = vec![primary];
        // 旧清单没有 extra_paths，读不到就是单根——与 B17 行为一致。
        if let Some(list) = entry.get("extra_paths").and_then(|v| v.as_array()) {
            roots.extend(
                list.iter()
                    .filter_map(|v| v.as_str())
                    .map(PathBuf::from),
            );
        }
        Some(roots)
    }

    /// 空间 id → 目录。侧栏一次性拼 `workspace` 用。
    async fn space_paths(&self) -> Result<std::collections::HashMap<String, PathBuf>> {
        let Some(store) = &self.space_store else {
            return Ok(Default::default());
        };
        Ok(store
            .list()
            .await?
            .into_iter()
            .map(|space| (space.id, space.path))
            .collect())
    }

    /// 空间路径的安全闸：不能指向应用状态目录或其子目录。
    ///
    /// 放行的话，Agent 就能改自己的会话、甚至 `provider.json` 里的明文 key。
    /// 盘根是另一回事（决策 2）：允许，由前端二次确认，不在这里硬禁。
    fn guard_space_path(&self, path: &Path) -> Result<()> {
        let Some(state_dir) = &self.config.state_dir else {
            return Ok(());
        };
        if is_inside(path, state_dir) {
            return Err(Error::Session(format!(
                "空间目录不能指向应用状态目录 {}（Agent 不该能改自己的会话）",
                state_dir.display()
            )));
        }
        Ok(())
    }

    /// 落盘。第一次调用时才分配 id。
    ///
    /// 落在**活动空间**的目录里：归属由位置决定（A-13），
    /// 所以这里的 space 一旦写错，会话就会「跑到另一个空间去」。
    async fn persist_locked(&self, session: &Session) -> Result<()> {
        let Some(catalog) = &self.catalog else {
            return Ok(());
        };

        let id = match self.session_id.lock() {
            Ok(mut slot) => slot.get_or_insert_with(SessionStore::new_id).clone(),
            Err(_) => return Ok(()),
        };
        let space_id = self.active_space();
        catalog.store(space_id.as_deref()).persist(&id, session).await
    }

    /// 跑一轮对话。进度只从 `events` 出去，历史只从 `session` 出来。
    pub async fn chat(&self, prompt: &str, events: &EventSink) -> Result<Outcome> {
        // 先把 provider 与模型名取出来再放开锁：整轮对话可能跑几分钟，
        // 期间不该阻塞用户去改配置。
        let (provider, model, max_tokens) = self.agent_parts();

        // 工作区在整轮内**快照**（A-12）：从这里往下只认 `roots`，
        // 不再看 `active` 锁。这样即便切换请求漏进来，也不会出现
        // 「前半段在 A 目录、后半段在 B 目录」。
        //
        // 快照的单位是**整份根集合**，不只是主目录：附加目录也是 jail 的一部分，
        // 只快照主根会让"附加目录在中途变了"这件事从 side door 溜进来。
        let roots = self.roots();
        let policy = Policy::with_roots(self.mode(), roots);

        // 技能每轮扫一次（几十个几 KB 的文件），换来的是"放进去就生效"——
        // 不需要重启应用，也不会有"清单与磁盘不一致"的中间态。
        let skills_root = self.config.skills_dir.clone();
        let system = system_with_skills(&self.config.system, skills_root.as_deref());

        let agent = AgentLoop::new(
            provider,
            Arc::clone(&self.tools),
            policy,
            Config {
                model,
                system,
                max_tokens,
                max_turns: self.config.max_turns,
                compact_at: self.config.compact_at,
                compact_keep: self.config.compact_keep,
                skills_root,
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

/// `path` 是否落在 `base` 之内（含相等）。
///
/// 能 canonicalize 就比 canonicalize 后的路径，能认 `C://a//..//b` 这类；
/// 不能（路径还不存在）就比词法路径，Windows 下忽略大小写。
fn is_inside(path: &Path, base: &Path) -> bool {
    let (path, base) = match (path.canonicalize(), base.canonicalize()) {
        (Ok(path), Ok(base)) => (path, base),
        _ => (path.to_path_buf(), base.to_path_buf()),
    };

    if cfg!(windows) {
        let path = path.to_string_lossy().to_lowercase();
        let base = base.to_string_lossy().to_lowercase();
        return path == base || path.starts_with(&format!("{}\\", base.trim_end_matches('\\')));
    }
    path == base || path.starts_with(&base)
}

#[cfg(test)]
mod skill_prompt_tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("goldenagent-prompt-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, id: &str, body: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    /// 不装技能的用户，提示词必须与上一版**逐字节相同**。
    #[test]
    fn no_skills_dir_keeps_the_prompt_untouched() {
        let base = "你是助手";
        assert_eq!(system_with_skills(base, None), base);

        let empty = temp("empty");
        assert_eq!(system_with_skills(base, Some(&empty)), base);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn skills_are_listed_but_their_body_is_not_inlined() {
        let root = temp("list");
        write_skill(
            &root,
            "pdf",
            "---\nname: PDF 处理\ndescription: 处理 PDF 时用\n---\n正文不该出现在提示里\n",
        );

        let out = system_with_skills("你是助手", Some(&root));

        assert!(out.starts_with("你是助手"), "原有提示词要保留");
        assert!(out.contains("- pdf（PDF 处理）：处理 PDF 时用"), "{out}");
        assert!(out.contains("use_skill"), "要告诉模型怎么读正文");
        assert!(!out.contains("正文不该出现在提示里"), "正文必须按需读，不能内联");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn broken_skill_dirs_are_reported_not_swallowed() {
        let root = temp("broken");
        write_skill(&root, "ok", "---\ndescription: 好的\n---\n");
        write_skill(&root, "broken", "没有 frontmatter\n");

        let out = system_with_skills("你是助手", Some(&root));

        assert!(out.contains("未加载"), "{out}");
        assert!(out.contains("broken"), "要说清是哪个目录: {out}");
        assert!(out.contains("description"), "要说清为什么: {out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 32 个满配描述会超过 8 KiB，超出部分必须被截断并且**说出来**。
    #[test]
    fn oversized_list_is_clipped_and_admits_it() {
        let root = temp("huge");
        let long = "说".repeat(190);
        for i in 0..skill::MAX_SKILLS {
            write_skill(&root, &format!("s{i:02}"), &format!("---\ndescription: {long}\n---\n"));
        }

        let out = system_with_skills("你是助手", Some(&root));

        assert!(out.len() <= skill::MAX_PROMPT_BYTES + 512, "清单不能无限长");
        assert!(out.contains("因清单长度上限未列出"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
