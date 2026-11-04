//! 第 1 层：模型接入层。
//!
//! 只做一件事：把统一的 [`CompletionRequest`] 翻译成某家厂商的 wire format，
//! 再把结果翻译回 [`Message`]。不知道有没有 UI、不知道有没有 Tauri。
//!
//! # 现在有哪几种 provider
//!
//! | kind | 打哪个接口 | 覆盖范围 |
//! |---|---|---|
//! | `mock` | 不出网 | 离线回显，全链路测试用 |
//! | `anthropic` | `POST /v1/messages` | Anthropic 官方 |
//! | `openai` | `POST {base}/chat/completions` | OpenAI 以及一切兼容实现（DeepSeek / Moonshot / vLLM / Ollama `/v1` / 自建网关） |
//!
//! 装配走 [`ProviderConfig::build`]；从哪读配置（环境变量 / 文件）由上层决定，
//! 这一层不碰文件系统。
//!
//! # 加一家新厂商
//!
//! 在 `config.rs` 的 [`ProviderKind`] 加一个变体、写一个模块实现 [`Provider`]，
//! 再在 `ProviderConfig::build` 里接一行。循环、事件枚举、外壳全都不用改。
//!
//! # 流式
//!
//! [`Provider::stream`] 在语法上只是多了一个 `mpsc::Sender<StreamChunk>`，
//! 对外行为契约（[`goldenagent_core::Event`]）一个字都没改——
//! 循环把收到的片段原样转成 `Event::TextDelta`，前端照旧拼接。
//!
//! 有一条默认实现：**只实现 `complete` 的 provider 自动获得流式能力**，
//! 代价是"整段一次性发出"。所以加新厂商不必先想流式。

mod anthropic;
mod config;
mod http;
mod mock;
mod openai;
mod sse;

pub use config::{
    ProviderConfig, ProviderKind, DEFAULT_MAX_TOKENS, MAX_MAX_TOKENS, MIN_MAX_TOKENS,
};
pub use mock::MockProvider;

use async_trait::async_trait;
use goldenagent_core::{ContentBlock, Message, Result, StopReason, ToolSpec};
use tokio::sync::mpsc;

/// 流式片段。
///
/// 用枚举而不是 `String`：正文与思考必须能分开（见
/// [`StreamChunk::ThinkingDelta`]），以后再加工具参数增量也不必改签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamChunk {
    TextDelta(String),
    /// 推理模型的思考过程（OpenAI 兼容的 `reasoning_content`、
    /// Anthropic 的 `thinking_delta`）。
    ///
    /// **不是回答内容**：它不进入消息、不落盘、不参与工具判断，
    /// 只用来让界面在"正文迟迟不来"的那几十秒里有点东西可显示。
    /// 把它并进 [`StreamChunk::TextDelta`] 会让思考混进历史，
    /// 下一轮再发回去——那是另一个模型的思考，既没用也费 token。
    ThinkingDelta(String),
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub message: Message,
    pub stop_reason: StopReason,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;

    async fn complete(&self, request: CompletionRequest) -> Result<Completion>;

    /// 流式补全：边收边把片段推给 `tx`，最后仍然返回整条 [`Completion`]。
    ///
    /// 默认实现退化为「先拿完整结果，再当作一个 delta 发出」——
    /// 这样只实现 `complete` 的 provider 也能跑流式路径，循环层不需要分支。
    ///
    /// 约定：**凡是出现在返回消息里的文本，都必须已经通过 `tx` 发过一遍**。
    /// 违反这条会导致界面上一片空白——循环不再从 `Completion` 里补发文本。
    async fn stream(
        &self,
        request: CompletionRequest,
        tx: mpsc::Sender<StreamChunk>,
    ) -> Result<Completion> {
        let completion = self.complete(request).await?;
        pump_text(&completion, &tx).await;
        Ok(completion)
    }
}

/// 把整条消息里的文本当作一个片段推出去。只给"不支持真流式"的实现复用。
pub(crate) async fn pump_text(completion: &Completion, tx: &mpsc::Sender<StreamChunk>) {
    for block in &completion.message.blocks {
        if let ContentBlock::Text { text } = block {
            if !text.is_empty()
                && tx.send(StreamChunk::TextDelta(text.clone())).await.is_err()
            {
                // 接收端没了：用户关窗口或按了停止，没必要再往下推。
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goldenagent_core::{ContentBlock, Message, StopReason};

    fn request(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest {
            model: "mock".into(),
            system: String::new(),
            messages,
            tools: Vec::new(),
            max_tokens: 1024,
        }
    }

    /// 脚本命中：按顺序返回预设回复，不碰网络。
    #[tokio::test]
    async fn returns_scripted_reply_in_order() {
        let provider = MockProvider::new(vec![Message::assistant(vec![ContentBlock::Text {
            text: "第一轮".into(),
        }])]);
        provider.push(Message::assistant(vec![ContentBlock::Text {
            text: "第二轮".into(),
        }]));

        let req = || request(vec![Message::user("你好")]);

        let first = provider.complete(req()).await.unwrap();
        assert_eq!(first.message.text(), "第一轮");
        assert_eq!(first.stop_reason, StopReason::EndTurn);
        assert_eq!(first.message.role, goldenagent_core::Role::Assistant);

        let second = provider.complete(req()).await.unwrap();
        assert_eq!(second.message.text(), "第二轮");
    }

    /// 脚本耗尽：回显最后一条 user 消息。这是全链路离线可跑的兜底。
    #[tokio::test]
    async fn falls_back_to_echo_when_script_exhausted() {
        let provider = MockProvider::default();
        let completion = provider
            .complete(request(vec![
                Message::user("第一条"),
                Message::assistant(vec![ContentBlock::Text {
                    text: "回复".into(),
                }]),
                Message::user("最后一条"),
            ]))
            .await
            .unwrap();

        assert_eq!(completion.message.text(), "（离线回显）最后一条");
        assert_eq!(completion.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn reports_stable_id() {
        assert_eq!(MockProvider::default().id(), "mock");
    }
}

