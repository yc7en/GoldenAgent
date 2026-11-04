//! provider 的装配状态与配置读写。
//!
//! 这一层要回答三个问题，而且**必须分开回答**：
//!
//! 1. 现在能用真模型吗？（[`ProviderState`]）
//! 2. 配置是从哪儿来的？（[`ProviderSource`]）
//! 3. 要改的话，改哪个文件、写成什么样？（[`ProviderSetup::config_path`] / `template`）
//!
//! 合并成一个"离线回显"标签是不行的：首次安装的用户看到"离线回显"，
//! 会以为软件坏了；而把 key 写错的用户看到同一句话，会以为要重装。
//! 两者的下一步动作完全不同，所以状态必须能区分。

use std::path::{Path, PathBuf};

use goldenagent_core::{Error, Result};
use goldenagent_provider::{
    ProviderConfig, ProviderKind, DEFAULT_MAX_TOKENS, MAX_MAX_TOKENS, MIN_MAX_TOKENS,
};

/// provider 能不能用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderState {
    /// 配好了，请求能发出去。
    Ready,
    /// 既没有配置文件也没有环境变量。**这是首次启动的正常状态**，不是错误。
    Unconfigured,
    /// 配了但用不了（JSON 坏了、缺 key、路径不可读）。必须带人能看懂的原因。
    Degraded { reason: String },
}

impl ProviderState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unconfigured => "unconfigured",
            Self::Degraded { .. } => "degraded",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Degraded { reason } => Some(reason),
            _ => None,
        }
    }
}

/// 配置从哪儿来。环境变量优先于文件——临时测试和 CI 走变量，桌面双击走文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSource {
    Env,
    File,
    None,
}

impl ProviderSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::File => "file",
            Self::None => "none",
        }
    }
}

/// 界面下拉里的一项。默认模型名跟在这里，省得前端再抄一遍——
/// 抄一遍的下场是厂商换默认模型时只有后端改了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOption {
    pub id: &'static str,
    pub default_model: &'static str,
    /// 要不要 API Key。`mock` 不要，其余都要——表单据此决定是否必填。
    pub needs_key: bool,
}

/// 支持的厂商清单。
pub fn catalog() -> Vec<ProviderOption> {
    ProviderKind::ALL
        .iter()
        .map(|kind| ProviderOption {
            id: kind.as_str(),
            default_model: kind.default_model(),
            needs_key: !kind.api_key_env().is_empty(),
        })
        .collect()
}

/// 交给界面的全部 provider 信息。
///
/// **刻意不含 `api_key`**：这份结构会被序列化、被打印、被贴进工单。
/// 只给"有没有 key"，不给 key 本身。
#[derive(Debug, Clone)]
pub struct ProviderSetup {
    pub state: ProviderState,
    pub source: ProviderSource,
    /// 当前生效的厂商（`mock` / `anthropic` / `openai`）。
    pub kind: String,
    /// 当前生效的模型名。
    pub model: String,
    /// 空串表示用厂商默认地址。
    pub base_url: String,
    /// 单次回答的输出上限。界面要显示它——推理模型的用户
    /// 看到"额度全用在思考上"的报错时，得知道去哪儿改。
    pub max_tokens: u32,
    /// 允许的取值范围。界面拿它渲染输入框的 `min` / `max`，
    /// 而不是自己写死一份——两处常量各说一套时，用户会撞上一个
    /// "表单接受、后端改掉"的黑洞（32768 那次就是这么来的）。
    pub max_tokens_min: u32,
    pub max_tokens_max: u32,
    /// 配置文件路径。界面要把它显示出来——用户得知道该改哪个文件。
    pub config_path: PathBuf,
    /// 从环境变量探测到的 key 变量名（不是值）。`None` 表示没探测到。
    pub env_key: Option<String>,
    /// 可直接落盘的配置模板。
    pub template: String,
    /// 可选厂商清单。
    pub catalog: Vec<ProviderOption>,
}

/// 界面提交上来的配置。字段都是弱类型（来自表单），在这里一次性转强类型。
#[derive(Debug, Clone, Default)]
pub struct ProviderInput {
    pub kind: String,
    pub model: String,
    /// `None` = 保留原来那份 key。
    ///
    /// 界面拿不到已存的 key（不该拿），所以"只改模型"时它只能发 `None`。
    /// 若把 `None` 当空串处理，用户改个模型名就会把 key 冲掉，
    /// 下次启动直接降级——这种坑不该让用户踩。
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub timeout_secs: Option<u64>,
    pub max_retries: Option<u32>,
    pub stream: Option<bool>,
    /// `None` = 不改。表单没这一项时不能把用户手写的值冲成默认值。
    pub max_tokens: Option<u32>,
}

/// 解析结果。`state` 里只有 `Ready` / `Unconfigured` / `Degraded`，
/// 真正的"能不能发请求"要等 [`ProviderConfig::build`] 才知道。
#[derive(Debug)]
pub(crate) struct Resolved {
    pub config: ProviderConfig,
    pub source: ProviderSource,
    pub state: ProviderState,
}

/// 按「环境变量 → 配置文件 → 未配置」解析。
///
/// 只有"文件不存在"是 `Unconfigured`；"文件在但读不动/解析失败"一律 `Degraded`，
/// 因为这两种情况用户要做的事不同（前者去创建，后者去修）。
pub(crate) fn resolve(path: &Path) -> Resolved {
    if let Some(config) = ProviderConfig::from_env() {
        return Resolved {
            config,
            source: ProviderSource::Env,
            state: ProviderState::Ready,
        };
    }

    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<ProviderConfig>(&text) {
            Ok(config) => Resolved {
                config,
                source: ProviderSource::File,
                state: ProviderState::Ready,
            },
            Err(error) => Resolved {
                config: ProviderConfig::mock(),
                source: ProviderSource::File,
                state: ProviderState::Degraded {
                    reason: format!("{} 解析失败：{error}", path.display()),
                },
            },
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Resolved {
            config: ProviderConfig::mock(),
            source: ProviderSource::None,
            state: ProviderState::Unconfigured,
        },
        Err(error) => Resolved {
            config: ProviderConfig::mock(),
            source: ProviderSource::File,
            state: ProviderState::Degraded {
                reason: format!("读取 {} 失败：{error}", path.display()),
            },
        },
    }
}

/// 写入配置并返回解析后的结构。
///
/// 顺序是刻意定的：**先装一遍，装得起来才写盘**。
/// 反过来会留下一个下次启动必然 degraded 的文件，而那时用户已经记不清自己填了什么。
///
/// `existing` 用来承接 `api_key: None`（只改模型时保留原 key）。
/// 跨厂商不承接——Anthropic 的 key 填进 OpenAI 只会得到 401。
pub(crate) fn save(
    path: &Path,
    input: &ProviderInput,
    existing: Option<&ProviderConfig>,
) -> Result<ProviderConfig> {
    let kind = ProviderKind::parse(&input.kind).ok_or_else(|| {
        Error::Config(format!(
            "未知厂商 `{}`，可选：{}",
            input.kind,
            ProviderKind::ALL
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        ))
    })?;

    let api_key = match &input.api_key {
        Some(key) => key.trim().to_string(),
        None => existing
            .filter(|old| old.kind == kind)
            .map(|old| old.api_key.clone())
            .unwrap_or_default(),
    };

    let config = ProviderConfig {
        kind,
        model: input.model.trim().to_string(),
        api_key,
        base_url: input
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        // 界面没暴露的字段一律**沿用旧值**，不能填默认值。
        //
        // 之前这里写的是 `unwrap_or(180)` / `.unwrap_or(2)` / `.unwrap_or(true)`：
        // 表单不提交这三项，于是每点一次"保存"就把用户手改过的
        // `timeout_secs: 600`、`stream: false` 冲回默认值。
        // 表现是"我只是改了个最大输出，怎么流式自己开了"。
        timeout_secs: input
            .timeout_secs
            .or_else(|| existing.map(|old| old.timeout_secs))
            .unwrap_or(180)
            .clamp(10, 1800),
        max_retries: input
            .max_retries
            .or_else(|| existing.map(|old| old.max_retries))
            .unwrap_or(2)
            .min(6),
        // 夹住而不是拒绝：填 100 的用户是想"省点钱"，填 999999 的是想"别截断"，
        // 两种意图都能用边界值满足，报错只会让人卡在这里。
        max_tokens: input
            .max_tokens
            .map(|value| value.clamp(MIN_MAX_TOKENS, MAX_MAX_TOKENS))
            .or_else(|| existing.map(|old| old.max_tokens))
            .unwrap_or(DEFAULT_MAX_TOKENS),
        stream: input
            .stream
            .or_else(|| existing.map(|old| old.stream))
            .unwrap_or(true),
    };

    config.build()?;

    let text = serde_json::to_string_pretty(&config)
        .map_err(|error| Error::Config(format!("序列化配置失败：{error}")))?;
    write_private(path, &text)?;

    Ok(config)
}

/// 原子写 + 收紧权限。
///
/// 这份文件里有 API Key：先写 `.tmp` 再改名，避免写一半留下半份 JSON；
/// 在 Unix 上顺手设成 0600，同机其他用户读不到。
fn write_private(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::Config(format!("创建 {} 失败：{error}", parent.display())))?;
    }

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)
        .map_err(|error| Error::Config(format!("写入 {} 失败：{error}", tmp.display())))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::rename(&tmp, path)
        .map_err(|error| Error::Config(format!("保存 {} 失败：{error}", path.display())))?;

    Ok(())
}

/// 可直接粘成 `provider.json` 的模板。
///
/// 用真实字段而不是说明性伪代码：用户复制过去改两个值就能跑，
/// 不必先去读文档确认字段名。
pub(crate) fn template_for(kind: ProviderKind) -> String {    let sample = ProviderConfig {
        kind,
        model: kind.default_model().to_string(),
        api_key: if kind == ProviderKind::Mock {
            String::new()
        } else {
            "在这里填你的 API Key".to_string()
        },
        ..ProviderConfig::default()
    };

    serde_json::to_string_pretty(&sample).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        // 理由同 scenarios.rs：时间戳不够唯一，序号才够。
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "goldenagent-provider-{tag}-{}-{}-{seq}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_unconfigured_not_degraded() {
        let dir = temp_dir("missing");
        let resolved = resolve(&dir.join("provider.json"));

        assert_eq!(resolved.source, ProviderSource::None);
        assert_eq!(resolved.state, ProviderState::Unconfigured);
        assert!(
            resolved.state.reason().is_none(),
            "没配置不该有错误原因——它不是错误"
        );
    }

    #[test]
    fn broken_json_is_degraded_with_reason() {
        let dir = temp_dir("broken");
        let path = dir.join("provider.json");
        std::fs::write(&path, "{ 这不是 json").unwrap();

        let resolved = resolve(&path);
        assert_eq!(resolved.state.as_str(), "degraded");
        assert!(
            resolved.state.reason().unwrap().contains("解析失败"),
            "原因要指出是解析的问题: {:?}",
            resolved.state.reason()
        );
    }

    #[test]
    fn save_writes_file_without_leaking_key_into_setup() {
        let dir = temp_dir("save");
        let path = dir.join("nested/provider.json");

        let config = save(
            &path,
            &ProviderInput {
                kind: "openai".into(),
                model: "gpt-4o-mini".into(),
                api_key: Some("sk-test-123".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        assert_eq!(config.kind, ProviderKind::OpenAi);
        assert!(path.exists(), "必须真的落盘");

        // 回读一遍，确认落盘的能被原样读回来。
        let resolved = resolve(&path);
        assert_eq!(resolved.state, ProviderState::Ready);
        assert_eq!(resolved.config.api_key, "sk-test-123");
    }

    #[test]
    fn saving_without_key_keeps_the_old_one() {
        let dir = temp_dir("keep-key");
        let path = dir.join("provider.json");

        let first = save(
            &path,
            &ProviderInput {
                kind: "anthropic".into(),
                model: "claude-sonnet-4-5".into(),
                api_key: Some("sk-ant-keep".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // 只改模型，不填 key —— 界面拿不到旧 key，只能这样表达"不动它"。
        let second = save(
            &path,
            &ProviderInput {
                kind: "anthropic".into(),
                model: "claude-opus-4-1".into(),
                api_key: None,
                ..Default::default()
            },
            Some(&first),
        )
        .unwrap();

        assert_eq!(second.model, "claude-opus-4-1");
        assert_eq!(second.api_key, "sk-ant-keep", "改模型不该冲掉 key");
    }

    #[test]
    fn switching_vendor_does_not_reuse_key() {
        let dir = temp_dir("switch");
        let path = dir.join("provider.json");

        let anthropic = save(
            &path,
            &ProviderInput {
                kind: "anthropic".into(),
                api_key: Some("sk-ant-xxx".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // 换成 openai 但不填 key：不该把 Anthropic 的 key 挪过去，
        // 那只会换来一个 401，比直接说"缺 key"难查得多。
        let error = save(
            &path,
            &ProviderInput {
                kind: "openai".into(),
                api_key: None,
                ..Default::default()
            },
            Some(&anthropic),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("key") || error.to_string().contains("API"),
            "{error}"
        );
    }

    #[test]
    fn save_rejects_config_that_cannot_build_without_writing() {
        let dir = temp_dir("reject");
        let path = dir.join("provider.json");

        let error = save(
            &path,
            &ProviderInput {
                kind: "anthropic".into(),
                api_key: Some(String::new()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();

        assert_eq!(path.exists(), false, "装不起来就不该留下文件");
        assert!(
            error.to_string().contains("key") || error.to_string().contains("API"),
            "错误要说清是缺 key: {error}"
        );
    }

    #[test]
    fn unknown_kind_is_rejected_with_options() {
        let dir = temp_dir("kind");
        let error = save(
            &dir.join("provider.json"),
            &ProviderInput {
                kind: "gemini".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();

        let text = error.to_string();
        assert!(text.contains("未知厂商"), "{text}");
        assert!(text.contains("anthropic"), "要把可选项列出来: {text}");
    }

    #[test]
    fn template_for_real_vendor_reminds_user_to_fill_key() {
        let text = template_for(ProviderKind::Anthropic);
        assert!(text.contains("\"anthropic\""), "{text}");
        assert!(text.contains("API Key"), "模板里要提醒填 key: {text}");

        // 模板必须是合法 JSON，否则用户粘过去只会更困惑。
        let parsed: ProviderConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.kind, ProviderKind::Anthropic);
    }
}
