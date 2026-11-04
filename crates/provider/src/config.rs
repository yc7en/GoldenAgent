//! provider 的配置：从哪儿读、默认值是什么、怎么变成一个可用的 [`Provider`]。
//!
//! 这一层刻意不碰文件系统——读哪个文件是装配层（`app`）的事，
//! 这里只认环境变量与一份已经解析好的结构体。

use std::sync::Arc;

use goldenagent_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::anthropic::AnthropicProvider;
use crate::mock::MockProvider;
use crate::openai::OpenAiProvider;
use crate::Provider;

/// 支持的厂商。新增一家只在这里加一个变体，
/// 循环、事件、外壳都不用改。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// 离线回显，不需要 key。
    Mock,
    /// Anthropic Messages API（`/v1/messages`）。
    Anthropic,
    /// OpenAI 兼容接口（`/v1/chat/completions`）：
    /// OpenAI 官方、DeepSeek、Moonshot、vLLM、Ollama 的 `/v1` 都走这条。
    ///
    /// 必须显式 `rename`：`rename_all = "snake_case"` 会把这个变体写成 `open_ai`，
    /// 而所有文档和用户直觉都是 `openai`。
    #[serde(
        rename = "openai",
        alias = "open_ai",
        alias = "openai_compat",
        alias = "openai-compatible"
    )]
    OpenAi,
}

impl Default for ProviderKind {
    fn default() -> Self {
        Self::Mock
    }
}

impl ProviderKind {
    /// 全部变体，给界面做下拉用。
    ///
    /// 加厂商时不用再记得改前端——前端拿的就是这个列表。
    pub const ALL: [Self; 3] = [Self::Mock, Self::Anthropic, Self::OpenAi];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    /// 用户/配置文件写的字符串 → 变体。
    ///
    /// 与 `as_str` 严格互为逆运算，且接受 `openai_compat` 这类常见别名——
    /// 用户把 `openai-compatible` 写进配置文件是常事，报错不如认下来。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "mock" | "offline" => Some(Self::Mock),
            "anthropic" | "claude" => Some(Self::Anthropic),
            "openai" | "open_ai" | "openai_compat" | "openai-compatible" => Some(Self::OpenAi),
            _ => None,
        }
    }

    /// 这家厂商从哪个环境变量读 key。空串表示不需要。
    pub fn api_key_env(&self) -> &'static str {
        match self {
            Self::Mock => "",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
        }
    }

    /// 没显式配 model 时的兜底。改了这里记得同步文档，模型名是会被厂商下线的。
    pub fn default_model(&self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Anthropic => "claude-sonnet-4-5",
            Self::OpenAi => "gpt-4o-mini",
        }
    }

    pub fn default_base_url(&self) -> &'static str {
        match self {
            Self::Mock => "",
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAi => "https://api.openai.com/v1",
        }
    }

}

/// 装配一个真实 provider 需要知道的全部信息。
///
/// 这份结构可以直接写成 `.goldenagent/provider.json`：
///
/// ```json
/// {
///   "kind": "anthropic",
///   "model": "claude-sonnet-4-5",
///   "api_key": "sk-ant-...",
///   "base_url": null,
///   "timeout_secs": 180,
///   "max_retries": 2,
///   "max_tokens": 8192
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// 自建网关 / 代理 / 本地模型时填。`None` 用厂商默认。
    #[serde(default)]
    pub base_url: Option<String>,
    /// 单次请求超时。LLM 长回答容易超过 60s，默认给宽一点。
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 429 / 5xx / 网络抖动的重试次数。设 0 关闭。
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// 单次回答的输出上限。
    ///
    /// 推理模型（deepseek 的 reasoner 系列、各家带 thinking 的模型）
    /// **把思考也算在这份预算里**：思考还没结束就把额度耗光时，
    /// 正文一个字都不会有，只回来一个 `finish_reason: length`。
    /// 4096 对纯对话够用，对"先想一大段再动手"的模型不够——
    /// 所以默认给到 8192。
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// 是否用 SSE 流式接收。
    ///
    /// 默认开。极少数自建网关声称兼容却会把 SSE 拼成一整段再返回，
    /// 关掉它就走普通请求——功能一样，只是没有逐字效果。
    #[serde(default = "default_stream")]
    pub stream: bool,
}

fn default_timeout() -> u64 {
    180
}

fn default_retries() -> u32 {
    2
}

fn default_max_tokens() -> u32 {
    DEFAULT_MAX_TOKENS
}

fn default_stream() -> bool {
    true
}

/// 默认输出上限。调大的理由见 [`ProviderConfig::max_tokens`]。
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
/// 低于这个值几乎一定被截断，不如直接拒掉——用户会以为是模型不听话。
pub const MIN_MAX_TOKENS: u32 = 256;
/// 上限。再大只是让"截断"来得更晚，而钱是照花的。
///
/// 取 131072 是因为主流模型的实际输出上限已经越过 32768：
/// Claude 系列 64K、Gemini 2.5 65K、DeepSeek reasoner 64K。
/// 原来卡在 32768 会把"模型支持、但这里不许填"变成一个说不清的限制，
/// 而**超限时又是静默夹取**——用户填 64000、界面回显 32768，
/// 只剩"保存不了"这一个观感。上限仍然要有（挡住 99999999 这类手误），
/// 但不能低于厂商的实际能力。
pub const MAX_MAX_TOKENS: u32 = 131072;

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::Mock,
            model: String::new(),
            api_key: String::new(),
            base_url: None,
            timeout_secs: default_timeout(),
            max_retries: default_retries(),
            max_tokens: default_max_tokens(),
            stream: default_stream(),
        }
    }
}

impl ProviderConfig {
    pub fn mock() -> Self {
        Self::default()
    }

    pub fn anthropic(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            kind: ProviderKind::Anthropic,
            model: model.into(),
            api_key: api_key.into(),
            ..Self::default()
        }
    }

    pub fn openai(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            kind: ProviderKind::OpenAi,
            model: model.into(),
            api_key: api_key.into(),
            ..Self::default()
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens.clamp(MIN_MAX_TOKENS, MAX_MAX_TOKENS);
        self
    }

    /// 实际会发给厂商的模型名。
    pub fn effective_model(&self) -> &str {
        if self.model.trim().is_empty() {
            self.kind.default_model()
        } else {
            &self.model
        }
    }

    fn effective_base_url(&self) -> &str {
        match self.base_url.as_deref() {
            Some(url) if !url.trim().is_empty() => url.trim(),
            _ => self.kind.default_base_url(),
        }
    }

    /// 从环境变量读配置。一个 key 都没有时返回 `None`，交给上层去找配置文件。
    ///
    /// 识别顺序：
    /// 1. `GOLDENAGENT_PROVIDER`（`anthropic` / `openai` / `mock`）；
    /// 2. 否则看 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` 哪个存在。
    pub fn from_env() -> Option<Self> {
        let explicit = env_nonempty("GOLDENAGENT_PROVIDER").map(|raw| match raw.to_ascii_lowercase().as_str() {
            "mock" => Some(ProviderKind::Mock),
            "anthropic" => Some(ProviderKind::Anthropic),
            "openai" | "openai_compat" | "openai-compatible" => Some(ProviderKind::OpenAi),
            other => {
                eprintln!("[goldenagent] 未知的 GOLDENAGENT_PROVIDER: {other}，按自动探测处理");
                None
            }
        });

        let kind = match explicit {
            Some(Some(kind)) => kind,
            _ => {
                if env_nonempty("ANTHROPIC_API_KEY").is_some() {
                    ProviderKind::Anthropic
                } else if env_nonempty("OPENAI_API_KEY").is_some() {
                    ProviderKind::OpenAi
                } else {
                    return None;
                }
            }
        };

        let api_key = env_nonempty("GOLDENAGENT_API_KEY")
            .or_else(|| {
                let env = kind.api_key_env();
                if env.is_empty() {
                    None
                } else {
                    env_nonempty(env)
                }
            })
            .unwrap_or_default();

        Some(Self {
            kind,
            model: env_nonempty("GOLDENAGENT_MODEL").unwrap_or_default(),
            api_key,
            base_url: env_nonempty("GOLDENAGENT_BASE_URL"),
            timeout_secs: env_nonempty("GOLDENAGENT_TIMEOUT_SECS")
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(default_timeout),
            max_retries: env_nonempty("GOLDENAGENT_MAX_RETRIES")
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(default_retries),
            max_tokens: env_nonempty("GOLDENAGENT_MAX_TOKENS")
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(default_max_tokens),
            stream: match env_nonempty("GOLDENAGENT_STREAM") {
                Some(raw) => !matches!(raw.to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
                None => true,
            },
        })
    }

    /// 变成真正能发请求的 provider。
    ///
    /// 这里失败通常是配置问题（没 key、key 里有非法字符），
    /// 错误信息要能让用户自己修好，所以每条都写清"该改哪儿"。
    pub fn build(&self) -> Result<Arc<dyn Provider>> {
        match self.kind {
            ProviderKind::Mock => Ok(Arc::new(MockProvider::default())),
            ProviderKind::Anthropic => {
                self.require_api_key()?;
                let provider = AnthropicProvider::new(self, self.effective_base_url())?;
                Ok(Arc::new(provider))
            }
            ProviderKind::OpenAi => {
                self.require_api_key()?;
                let provider = OpenAiProvider::new(self, self.effective_base_url())?;
                Ok(Arc::new(provider))
            }
        }
    }

    fn require_api_key(&self) -> Result<()> {
        if self.api_key.trim().is_empty() {
            return Err(Error::Provider(format!(
                "缺少 API key：kind = {}。请在 .goldenagent/provider.json 里填 api_key，\
                 或设置环境变量 {}（或 GOLDENAGENT_API_KEY）。",
                self.kind.as_str(),
                self.kind.api_key_env()
            )));
        }
        Ok(())
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_falls_back_to_kind_default() {
        let config = ProviderConfig {
            kind: ProviderKind::Anthropic,
            model: String::new(),
            ..ProviderConfig::default()
        };
        assert_eq!(config.effective_model(), "claude-sonnet-4-5");

        let config = ProviderConfig {
            model: "my-model".into(),
            ..config
        };
        assert_eq!(config.effective_model(), "my-model");
    }

    #[test]
    fn missing_key_is_a_config_error_not_a_panic() {
        let config = ProviderConfig::anthropic("claude-sonnet-4-5", "");
        // map(|_| ()) 是为了让 unwrap_err 只依赖错误类型的 Debug，
        // Arc<dyn Provider> 本身不是 Debug。
        let error = config.build().map(|_| ()).unwrap_err().to_string();
        assert!(error.contains("缺少 API key"), "{error}");
        assert!(error.contains("provider.json"), "错误里要指出改哪个文件: {error}");
    }

    #[test]
    fn mock_needs_no_key() {
        let provider = ProviderConfig::mock().build().unwrap();
        assert_eq!(provider.id(), "mock");
    }

    /// JSON 里写 kind 要能用连字符/下划线两种写法——手写配置文件的人不会查文档。
    #[test]
    fn kind_accepts_aliases() {
        for raw in [r#"{"kind":"openai"}"#, r#"{"kind":"openai_compat"}"#, r#"{"kind":"openai-compatible"}"#] {
            let config: ProviderConfig = serde_json::from_str(raw).unwrap();
            assert_eq!(config.kind, ProviderKind::OpenAi, "{raw}");
        }
    }

    /// 只写 kind 和 api_key 也要能跑，其余字段走默认。
    #[test]
    fn minimal_json_is_enough() {
        let config: ProviderConfig =
            serde_json::from_str(r#"{"kind":"anthropic","api_key":"sk-test"}"#).unwrap();
        assert_eq!(config.effective_model(), "claude-sonnet-4-5");
        assert_eq!(config.timeout_secs, 180);
        assert_eq!(config.max_retries, 2);
        assert!(config.base_url.is_none());
        assert_eq!(
            config.max_tokens, DEFAULT_MAX_TOKENS,
            "推理模型要留够思考的额度"
        );
    }

    /// 手写的配置里给了 max_tokens 就得认，别被默认值盖掉。
    #[test]
    fn max_tokens_from_json_is_respected() {
        let config: ProviderConfig =
            serde_json::from_str(r#"{"kind":"openai","api_key":"sk-test","max_tokens":16000}"#)
                .unwrap();
        assert_eq!(config.max_tokens, 16000);
    }
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    /// `parse` 与 `as_str` 必须互逆：界面拿 `as_str` 的值回传，
    /// 少认一个就会变成"保存不了自己刚显示的东西"。
    #[test]
    fn parse_roundtrips_with_as_str() {
        for kind in ProviderKind::ALL {
            assert_eq!(
                ProviderKind::parse(kind.as_str()),
                Some(kind),
                "{} 无法被 parse 回来",
                kind.as_str()
            );
        }
    }

    #[test]
    fn parse_accepts_handwritten_aliases() {
        for raw in ["openai", "open_ai", "openai_compat", "OpenAI-Compatible"] {
            assert_eq!(
                ProviderKind::parse(raw),
                Some(ProviderKind::OpenAi),
                "{raw} 应被认成 openai"
            );
        }
        assert_eq!(ProviderKind::parse("gemini"), None);
    }

    /// 只有 mock 不需要 key——表单据此决定 key 输入框要不要禁用。
    #[test]
    fn only_mock_needs_no_key() {
        assert!(ProviderKind::Mock.api_key_env().is_empty());
        assert!(!ProviderKind::Anthropic.api_key_env().is_empty());
        assert!(!ProviderKind::OpenAi.api_key_env().is_empty());
    }
}
