//! provider 装配链路：环境变量 / 配置文件 / 失败降级。
//!
//! 这里不碰网络——只验证"配置怎么变成一个 provider"，以及出错时
//! 用户能不能从 `provider_note()` 看出发生了什么。
//!
//! 注意：`AgentService::auto` 会读环境变量，所以每个用例先清掉
//! 相关变量，保证结果只由测试自己写的文件决定。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use goldenagent_app::{
    AgentConfig, AgentService, ProviderInput, ProviderSource, ProviderState,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 这几个用例都在改动进程级环境变量，而测试默认并行跑。
/// 不加锁就会出现"我清空、你在写"的竞争，表现为随机失败。
/// 锁只保护本文件内的用例，不影响其他 crate 的并行度。
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_guard() -> MutexGuard<'static, ()> {
    // 前一个用例 panic 中毒了也照常拿锁：环境变量已在本函数里重设，无需 reset。
    ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "goldenagent-provider-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn provider_json(&self) -> PathBuf {
        self.path.join("provider.json")
    }

    fn write(&self, content: &str) -> PathBuf {
        let path = self.provider_json();
        std::fs::write(&path, content).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 环境变量优先级高于文件，不清掉就测不出文件这条路径。
fn clear_provider_env() {
    for key in [
        "GOLDENAGENT_PROVIDER",
        "GOLDENAGENT_API_KEY",
        "GOLDENAGENT_MODEL",
        "GOLDENAGENT_BASE_URL",
        "GOLDENAGENT_TIMEOUT_SECS",
        "GOLDENAGENT_MAX_RETRIES",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
    ] {
        std::env::remove_var(key);
    }
}

#[test]
fn reads_provider_config_from_file() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.write(r#"{"kind":"anthropic","model":"my-model","api_key":"sk-test"}"#);

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);

    assert_eq!(service.provider_id(), "anthropic");
    assert_eq!(service.model(), "my-model");
    assert!(service.provider_note().is_none(), "正常装配不该有 note");
}

#[test]
fn empty_model_uses_vendor_default() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.write(r#"{"kind":"openai","api_key":"sk-test"}"#);

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);

    assert_eq!(service.provider_id(), "openai");
    assert_eq!(service.model(), "gpt-4o-mini", "没写 model 要用默认值");
}

/// 没配 ≠ 出错：这是全新用户第一次启动的默认状态，不该弹提示。
///
/// 但状态必须是 `Unconfigured` 而不是 `Ready`——界面靠它决定
/// "引导去配"还是"什么都不说"。
#[test]
fn missing_file_is_plain_offline_without_noise() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.provider_json(); // 不创建

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);

    assert_eq!(service.provider_id(), "mock");
    assert!(
        service.provider_note().is_none(),
        "没配 provider 不是错误，不该有降级原因"
    );
    assert_eq!(service.provider_state(), ProviderState::Unconfigured);
    assert_eq!(service.provider_setup().source, ProviderSource::None);
}

/// 写坏了要说清是哪个文件坏了，而不是静默变回声。
#[test]
fn broken_json_falls_back_and_says_which_file() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.write("{ this is not json }");

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);

    assert_eq!(service.provider_id(), "mock");
    assert_eq!(service.provider_state().as_str(), "degraded");

    let note = service.provider_note().expect("应留下降级原因");
    assert!(note.contains("解析失败"), "{note}");
    assert!(note.contains("provider.json"), "要指明是哪个文件: {note}");

    // "退到离线回显"这句由界面说（README §5 的三态约定），
    // 后端只负责把原因说准——两处都说会让同一件事有两种措辞。
    assert_eq!(service.provider_setup().state.as_str(), "degraded");
}

/// 配了 anthropic 但没填 key：必须退到离线，并且提示该改哪儿。
#[test]
fn empty_api_key_falls_back_with_actionable_hint() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.write(r#"{"kind":"anthropic","model":"m","api_key":""}"#);

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);

    assert_eq!(service.provider_id(), "mock");
    let note = service.provider_note().expect("应留下降级原因");
    assert!(note.contains("缺少 API key"), "{note}");
    assert!(note.contains("ANTHROPIC_API_KEY"), "要给出可执行的下一步: {note}");
}

/// 环境变量优先于文件——临时换个模型试的时候不该去改文件。
#[test]
fn env_overrides_file() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.write(r#"{"kind":"anthropic","model":"from-file","api_key":"sk-file"}"#);

    std::env::set_var("GOLDENAGENT_PROVIDER", "openai");
    std::env::set_var("OPENAI_API_KEY", "sk-env");
    std::env::set_var("GOLDENAGENT_MODEL", "from-env");

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);

    assert_eq!(service.provider_id(), "openai", "环境变量应优先");
    assert_eq!(service.model(), "from-env");
    assert!(service.provider_note().is_none());

    clear_provider_env();
}

/// 保存后立即生效，不用重启。
///
/// 这是 B16 的核心交互：首次启动的人在界面里填完 key 就该能直接对话。
/// 要求"改完重启"的软件，十次里有八次会被当成保存失败。
#[test]
fn save_provider_takes_effect_without_restart() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.provider_json();

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);
    assert_eq!(service.provider_state(), ProviderState::Unconfigured);

    let setup = service
        .save_provider(&ProviderInput {
            kind: "openai".into(),
            model: "gpt-4o-mini".into(),
            api_key: Some("sk-test".into()),
            base_url: Some("http://127.0.0.1:1/v1".into()),
            ..Default::default()
        })
        .expect("保存应成功");

    assert_eq!(setup.state, ProviderState::Ready);
    assert_eq!(setup.source, ProviderSource::File);
    assert_eq!(setup.kind, "openai");
    assert_eq!(service.provider_id(), "openai");
    assert_eq!(service.model(), "gpt-4o-mini");
    assert!(path.exists(), "必须真的落盘");
}

/// 只改模型时不填 key：必须保留旧 key。
///
/// 界面拿不到已存的 key（后端不下发），所以表单里留空只能表达"不改"。
/// 如果把留空当空串，用户改个模型名就会把 key 冲掉。
#[test]
fn save_provider_keeps_key_when_field_is_empty() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.provider_json();

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);
    service
        .save_provider(&ProviderInput {
            kind: "anthropic".into(),
            model: "claude-sonnet-4-5".into(),
            api_key: Some("sk-ant-keep".into()),
            ..Default::default()
        })
        .unwrap();

    service
        .save_provider(&ProviderInput {
            kind: "anthropic".into(),
            model: "claude-opus-4-1".into(),
            api_key: None,
            ..Default::default()
        })
        .expect("只改模型应成功");

    assert_eq!(service.model(), "claude-opus-4-1");
    assert_eq!(service.provider_state(), ProviderState::Ready);

    // 直接读盘确认 key 还在——这是唯一能证明"没被冲掉"的地方。
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("sk-ant-keep"), "key 被冲掉了: {raw}");
}

/// 非法厂商要在保存时报错，而不是写出一份下次启动就坏的文件。
#[test]
fn save_provider_rejects_unknown_kind_without_writing() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.provider_json();

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);
    let error = service
        .save_provider(&ProviderInput {
            kind: "gemini".into(),
            ..Default::default()
        })
        .unwrap_err();

    assert!(error.to_string().contains("未知厂商"), "{error}");
    assert!(!path.exists(), "失败不该留下文件");
    assert_eq!(service.provider_state(), ProviderState::Unconfigured);
}

/// 面板下发的数据里不能有 key。
///
/// 这条是回归护栏：`ProviderSetup` 现在没有 key 字段，
/// 哪天有人"顺手"加上去，这里会立刻炸。
#[test]
fn provider_setup_does_not_expose_api_key() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.write(r#"{"kind":"anthropic","model":"m","api_key":"sk-secret-value"}"#);

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);
    let setup = service.provider_setup();
    let dumped = format!("{setup:?}");

    assert!(
        !dumped.contains("sk-secret-value"),
        "配置面板的数据结构不该带上密钥: {dumped}"
    );
    assert_eq!(setup.kind, "anthropic");
    assert_eq!(setup.model, "m");
}

/// 最大输出必须能被改掉、落盘、重启后还在。
///
/// 这条是"最大输出保存不了"的回归护栏。原来没有它，于是三种毛病一起存在：
/// 上限卡在 32768（低于主流模型的实际能力）、超限静默夹取（界面上看不出发生了什么）、
/// 以及表单不提交的字段被重置成默认值。
#[test]
fn save_provider_persists_max_tokens() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.provider_json();

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);
    let setup = service
        .save_provider(&ProviderInput {
            kind: "openai".into(),
            model: "deepseek-flash".into(),
            api_key: Some("sk-test".into()),
            base_url: Some("https://api.deepseek.com".into()),
            max_tokens: Some(16384),
            ..Default::default()
        })
        .expect("保存应成功");

    assert_eq!(setup.max_tokens, 16384, "返回值要带上刚存的值");
    assert!(
        std::fs::read_to_string(&path).unwrap().contains("16384"),
        "必须真的写进文件"
    );

    // 换一个 service 重新装配，模拟"关掉应用再打开"。
    let reopened = AgentService::auto(AgentConfig::new(&dir.path), &path);
    assert_eq!(
        reopened.provider_setup().max_tokens,
        16384,
        "重启后还该是 16384"
    );
}

/// 超上限不是"保存失败"，而是被按上限收下——并且把上限报出来。
///
/// 只断言"没有失败"是不够的：用户真正的困惑是"我填了 20000 以上，
/// 它怎么就变成 32768 了"。所以这条同时锁住上限值本身。
#[test]
fn save_provider_clamps_max_tokens_and_reports_the_cap() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.provider_json();

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);
    let setup = service
        .save_provider(&ProviderInput {
            kind: "openai".into(),
            model: "m".into(),
            api_key: Some("sk-test".into()),
            max_tokens: Some(999_999),
            ..Default::default()
        })
        .expect("超上限不该报错，只该被收下");

    assert_eq!(setup.max_tokens, setup.max_tokens_max);
    assert!(
        setup.max_tokens_max >= 131_072,
        "上限要能装下主流模型的输出能力（Claude 64K / Gemini 65K），现在是 {}",
        setup.max_tokens_max
    );

    let low = service
        .save_provider(&ProviderInput {
            kind: "openai".into(),
            model: "m".into(),
            api_key: None,
            max_tokens: Some(1),
            ..Default::default()
        })
        .expect("低于下限也不该报错");
    assert_eq!(low.max_tokens, low.max_tokens_min);
}

/// 表单没暴露的字段，保存时必须沿用旧值。
///
/// 原来的合并逻辑是 `unwrap_or(180)` / `.unwrap_or(2)` / `.unwrap_or(true)`：
/// 界面上只有厂商/模型/Key/地址/最大输出五项，于是每点一次"保存"，
/// 用户手改过的 `timeout_secs: 600` 和 `stream: false` 就被悄悄冲回默认值。
#[test]
fn save_provider_keeps_fields_the_form_does_not_send() {
    let _env = env_guard();
    clear_provider_env();
    let dir = TempDir::new();
    let path = dir.write(
        r#"{"kind":"openai","model":"m","api_key":"sk-keep",
            "timeout_secs":600,"max_retries":5,"stream":false,"max_tokens":4096}"#,
    );

    let service = AgentService::auto(AgentConfig::new(&dir.path), &path);
    service
        .save_provider(&ProviderInput {
            kind: "openai".into(),
            model: "m2".into(),
            api_key: None,
            base_url: Some("https://api.deepseek.com".into()),
            // 只改模型名，其余五项都不提交
            ..Default::default()
        })
        .expect("保存应成功");

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("\"timeout_secs\": 600"), "超时被重置了: {raw}");
    assert!(raw.contains("\"max_retries\": 5"), "重试次数被重置了: {raw}");
    assert!(raw.contains("\"stream\": false"), "流式开关被重置了: {raw}");
    assert!(raw.contains("\"max_tokens\": 4096"), "最大输出被重置了: {raw}");
}
