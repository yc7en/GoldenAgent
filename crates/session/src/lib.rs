//! 第 1 层：会话与上下文。
//!
//! 循环的唯一输入源（A-8）。循环本身不持有任何隐藏状态，
//! 想断点续跑，把这个结构存下来再读回来就行。
//!
//! 存储格式是 **JSONL：一行一条 `Message`**。刻意不加元信息头，
//! 这样 `tail -n 5 session.jsonl | jq` 这类外部工具能直接用。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use goldenagent_core::{ContentBlock, Error, Message, Result};

mod catalog;
mod space;
mod store;

pub use catalog::{Located, SessionCatalog};
pub use space::{
    Space, SpaceStore, DEFAULT_SPACE_DIR, MANIFEST_FILE, MAX_EXTRA_ROOTS, SESSIONS_DIR,
};
pub use store::{SearchHit, SessionHits, SessionStore, SessionSummary};

#[derive(Debug, Clone, Default)]
pub struct Session {
    messages: Vec<Message>,
    created_at_ms: u64,
}

impl Session {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            created_at_ms: now_ms(),
        }
    }

    pub fn with_messages(messages: Vec<Message>) -> Self {
        Self {
            messages,
            created_at_ms: now_ms(),
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// 粗估 token 数。
    ///
    /// 不是精确计数（那要引入 tokenizer 依赖），只用于判断"是不是该压缩了"。
    /// 中日韩字符按 1 token/字，其他按 4 字符/token，另加每条消息的固定开销。
    pub fn estimate_tokens(&self) -> usize {
        self.messages
            .iter()
            .map(|message| {
                let blocks: usize = message
                    .blocks
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => estimate_text(text),
                        ContentBlock::ToolCall(call) => {
                            8 + estimate_text(&call.name) + estimate_text(&call.input.to_string())
                        }
                        ContentBlock::ToolResult(result) => 8 + estimate_text(&result.output),
                    })
                    .sum();
                4 + blocks
            })
            .sum()
    }

    /// 保留最近 `keep_last` 条消息，返回被删除的条数。
    ///
    /// 有一个坑必须处理：如果切点正好落在 `tool_result` 上，
    /// 保留下来的第一条就是"没有对应 tool_call 的结果"，模型会直接报错。
    /// 所以切点会向后推，直到第一条不是孤立的工具结果。
    pub fn compact(&mut self, keep_last: usize) -> usize {
        if self.messages.len() <= keep_last {
            return 0;
        }

        let mut start = self.messages.len() - keep_last;
        while start < self.messages.len() && is_orphan_tool_result(&self.messages[start]) {
            start += 1;
        }

        self.messages.drain(..start);
        start
    }

    /// 一行一条消息的 JSONL。
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for message in &self.messages {
            match serde_json::to_string(message) {
                Ok(line) => {
                    out.push_str(&line);
                    out.push('\n');
                }
                Err(_) => continue,
            }
        }
        out
    }

    pub fn from_jsonl(text: &str) -> Result<Self> {
        let mut messages = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let message: Message = serde_json::from_str(line).map_err(|e| {
                Error::Session(format!("会话文件第 {} 行不是合法消息: {e}", index + 1))
            })?;
            messages.push(message);
        }
        Ok(Self::with_messages(messages))
    }

    /// 整份重写。文件小（一条消息一行），重写比增量维护简单且不会写坏。
    ///
    /// **先写 `.tmp` 再改名**：直接 `write` 会先截断文件，进程如果在写一半时被杀，
    /// 留下的就是半份会话——那是真的数据丢失，而不只是一行坏 JSON。
    pub async fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Session(format!("创建会话目录失败: {e}")))?;
        }

        let temp = path.with_extension("jsonl.tmp");
        tokio::fs::write(&temp, self.to_jsonl())
            .await
            .map_err(|e| Error::Session(format!("写入会话失败: {e}")))?;
        tokio::fs::rename(&temp, path)
            .await
            .map_err(|e| Error::Session(format!("替换会话文件失败: {e}")))
    }

    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| Error::Session(format!("读取会话 {} 失败: {e}", path.display())))?;
        Self::from_jsonl(&text)
    }

    /// 增量追加一条消息。适合边跑边落盘，进程被杀也不丢已完成轮次。
    pub async fn append(path: impl AsRef<Path>, message: &Message) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Session(format!("创建会话目录失败: {e}")))?;
        }

        let mut line = serde_json::to_string(message)
            .map_err(|e| Error::Session(format!("序列化消息失败: {e}")))?;
        line.push('\n');

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .map_err(|e| Error::Session(format!("打开会话文件失败: {e}")))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Session(format!("追加会话失败: {e}")))
    }
}

/// 整条消息只有工具结果 —— 它的 tool_call 若已被压缩掉，模型会拒绝这条历史。
fn is_orphan_tool_result(message: &Message) -> bool {
    !message.blocks.is_empty()
        && message
            .blocks
            .iter()
            .all(|block| matches!(block, ContentBlock::ToolResult(_)))
}

fn estimate_text(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4) + 1
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x30FF      // 日文标点、平假名、片假名
        | 0x3400..=0x4DBF    // 扩展 A
        | 0x4E00..=0x9FFF    // 基本汉字
        | 0xF900..=0xFAFF    // 兼容汉字
        | 0xFF00..=0xFFEF    // 全角标点
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use goldenagent_core::{Role, ToolCall, ToolResult};
    use serde_json::json;

    fn tool_call_msg(id: &str) -> Message {
        Message::assistant(vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "read_file".into(),
            input: json!({"path": "a.txt"}),
        })])
    }

    #[test]
    fn push_and_read_back_in_order() {
        let mut session = Session::new();
        session.push_user("你好");
        session.push(Message::assistant(vec![ContentBlock::Text {
            text: "在".into(),
        }]));

        assert_eq!(session.len(), 2);
        assert_eq!(session.messages()[0].role, Role::User);
        assert_eq!(session.messages()[1].text(), "在");
        assert!(session.created_at_ms() > 0);
    }

    #[test]
    fn estimate_tokens_scales_with_content() {
        let short = Session::with_messages(vec![Message::user("hi")]);
        let long = Session::with_messages(vec![Message::user("hi".repeat(400))]);
        assert!(long.estimate_tokens() > short.estimate_tokens());

        // 中文按字计，理应比同字符数的英文更"贵"
        let cn = Session::with_messages(vec![Message::user("一二三四五六七八".repeat(10))]);
        let en = Session::with_messages(vec![Message::user("abcdefgh".repeat(10))]);
        assert!(cn.estimate_tokens() > en.estimate_tokens());
    }

    #[test]
    fn compact_keeps_tail_and_reports_removed() {
        let mut session = Session::with_messages(
            (0..10).map(|i| Message::user(format!("m{i}"))).collect(),
        );

        let removed = session.compact(4);
        assert_eq!(removed, 6);
        assert_eq!(session.len(), 4);
        assert_eq!(session.messages()[0].text(), "m6");
        assert_eq!(session.messages()[3].text(), "m9");
    }

    #[test]
    fn compact_is_noop_when_short_enough() {
        let mut session = Session::with_messages(vec![Message::user("a")]);
        assert_eq!(session.compact(4), 0);
        assert_eq!(session.len(), 1);
    }

    /// 切点落在 tool_result 上时必须往后推，否则留下"没有 tool_call 的结果"。
    #[test]
    fn compact_never_leaves_orphan_tool_result() {
        let mut session = Session::with_messages(vec![
            Message::user("开始"),
            tool_call_msg("c1"),
            Message::tool_results(vec![ToolResult {
                id: "c1".into(),
                ok: true,
                output: "内容".into(),
            }]),
            Message::assistant(vec![ContentBlock::Text {
                text: "读完了".into(),
            }]),
        ]);

        // 只保 2 条的话，落点正好是 tool_result（index 2）
        let removed = session.compact(2);
        assert_eq!(removed, 3, "应把孤立的 tool_result 一起丢掉");
        assert_eq!(session.len(), 1);
        assert_eq!(session.messages()[0].text(), "读完了");
    }

    #[test]
    fn jsonl_roundtrip_preserves_sequence() {
        let session = Session::with_messages(vec![
            Message::user("读一下 a.txt"),
            tool_call_msg("c1"),
            Message::tool_results(vec![ToolResult {
                id: "c1".into(),
                ok: false,
                output: "越界".into(),
            }]),
            Message::assistant(vec![ContentBlock::Text {
                text: "被拒了".into(),
            }]),
        ]);

        let text = session.to_jsonl();
        assert_eq!(text.lines().count(), 4);

        let back = Session::from_jsonl(&text).unwrap();
        assert_eq!(back.messages(), session.messages());
    }

    #[test]
    fn from_jsonl_skips_blank_lines_and_reports_bad_line() {
        let text = "\n\n{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"a\"}]}\n\n";
        assert_eq!(Session::from_jsonl(text).unwrap().len(), 1);

        let err = Session::from_jsonl("{not json}").unwrap_err();
        assert!(err.to_string().contains("第 1 行"), "{err}");
    }

    #[tokio::test]
    async fn save_load_append_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "goldenagent-session-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let path = dir.join("nested/session.jsonl");

        let mut session = Session::new();
        session.push_user("第一条");
        session.save(&path).await.unwrap();

        let mut loaded = Session::load(&path).await.unwrap();
        assert_eq!(loaded.len(), 1);

        Session::append(&path, &Message::user("第二条"))
            .await
            .unwrap();
        loaded = Session::load(&path).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.messages()[1].text(), "第二条");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_missing_file_is_an_error() {
        let err = Session::load(std::env::temp_dir().join("goldenagent-does-not-exist.jsonl"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("读取会话"), "{err}");
    }
}
