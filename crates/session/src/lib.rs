//! 第 1 层：会话与上下文。
//!
//! 循环的唯一输入源（A-8）。循环本身不持有任何隐藏状态，
//! 想断点续跑，把这个结构存下来再读回来就行。
//!
//! 存储格式是 **JSONL：一行一条 `Message`**。刻意不加元信息头，
//! 这样 `tail -n 5 session.jsonl | jq` 这类外部工具能直接用。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use goldenagent_core::{
    Artifact, ContentBlock, Error, Message, Result, SessionEvent, UIState,
};

mod catalog;
mod projections;
mod space;
mod store;

pub use catalog::{Located, SessionCatalog};
pub use projections::{
    derive_artifacts, derive_messages, derive_ui_state, message_to_events, migrate_lines_to_events,
};
pub use space::{
    Space, SpaceStore, DEFAULT_SPACE_DIR, MANIFEST_FILE, MAX_EXTRA_ROOTS, SESSIONS_DIR,
};
pub use store::{SearchHit, SessionHits, SessionStore, SessionSummary};

#[derive(Debug, Clone, Default)]
pub struct Session {
    events: Vec<SessionEvent>,
    messages: Vec<Message>,
    created_at_ms: u64,
}

/// 一段待处理的历史范围 `[start, end)`。
///
/// 单独定义是因为"选哪一段"和"怎么替换"是两个决定：现在的自动压缩只压头部，
/// 但按需压缩将来可能压中段。把范围显式化，替换逻辑就不用跟着改。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            messages: Vec::new(),
            created_at_ms: now_ms(),
        }
    }

    pub fn with_messages(messages: Vec<Message>) -> Self {
        let now = now_ms();
        let mut events = Vec::new();
        for (idx, msg) in messages.iter().enumerate() {
            events.extend(message_to_events(msg, now + (idx as u64) * 10, idx));
        }
        Self {
            events,
            messages,
            created_at_ms: now,
        }
    }

    pub fn with_events(events: Vec<SessionEvent>) -> Self {
        let created_at_ms = events
            .first()
            .map(|e| e.timestamp_ms())
            .unwrap_or_else(now_ms);
        let messages = derive_messages(&events);
        Self {
            events,
            messages,
            created_at_ms,
        }
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn derive_ui_state(&self) -> UIState {
        derive_ui_state(&self.events)
    }

    pub fn derive_artifacts(&self) -> Vec<Artifact> {
        derive_artifacts(&self.events)
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
        let now = now_ms();
        let seq = self.events.len();
        let new_events = message_to_events(&message, now, seq);
        self.events.extend(new_events);
        self.messages.push(message);
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        let text = text.into();
        let now = now_ms();
        let id = format!("usr-{}", self.events.len());
        self.events.push(SessionEvent::UserPrompt {
            id,
            text: text.clone(),
            timestamp_ms: now,
        });
        self.messages.push(Message::user(text));
    }

    pub fn record_event(&mut self, event: SessionEvent) {
        self.events.push(event);
        self.messages = derive_messages(&self.events);
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.messages.clear();
    }

    /// 粗估 token 数。
    ///
    /// 不是精确计数（那要引入 tokenizer 依赖），只用于判断"是不是该压缩了"。
    /// 中日韩字符按 1 token/字，其他按 4 字符/token，另加每条消息的固定开销。
    pub fn estimate_tokens(&self) -> usize {
        self.messages.iter().map(message_tokens).sum()
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

    /// 挑一段可以被摘要替换掉的历史。
    ///
    /// 与 [`Session::compact`] 的区别：那里是"保留最后 N 条"，这里是"保留一个
    /// **token 预算**"，因为一条 1 MiB 的工具结果和一条 `收到` 不该按同一条算。
    ///
    /// 两条边界必须守住，否则压完的历史模型根本发不出去：
    ///
    /// 1. **不能把工具配对切开**。切点落在纯 `tool_result` 消息上时向后推——
    ///    否则保留下来的第一条是"没有对应 tool_call 的结果"。
    /// 2. **至少留下 `min_keep` 条**，且绝不能把最后一条（本轮的提问）压掉。
    ///
    /// 返回 `None` 表示"没有可安全压缩的历史"：要么太短，要么整段是一个
    /// 不可分单元。调用方应如实上报，而不是反复重试。
    pub fn select_span(&self, keep_tokens: usize, min_keep: usize) -> Option<Span> {
        let len = self.messages.len();
        if len <= min_keep {
            return None;
        }

        // 先按条数保底，再按预算往前吞。
        let mut start = len - min_keep;
        let mut acc: usize = self.messages[start..].iter().map(message_tokens).sum();
        while start > 0 {
            let extra = message_tokens(&self.messages[start - 1]);
            if acc + extra > keep_tokens {
                break;
            }
            start -= 1;
            acc += extra;
        }

        // 切点落在孤立工具结果上就继续往后推，把这条结果一起压掉。
        while start < len && is_orphan_tool_result(&self.messages[start]) {
            start += 1;
        }

        if start == 0 || start >= len {
            return None;
        }
        Some(Span { start: 0, end: start })
    }

    /// 用一条消息替换 `span`，返回被替换掉的条数。
    ///
    /// 越界的 span 直接忽略并返回 0：宁可这次不压缩，也不把会话改坏。
    pub fn replace_span(&mut self, span: Span, replacement: Message) -> usize {
        if span.start > span.end || span.end > self.messages.len() {
            return 0;
        }
        let removed = span.end - span.start;
        // B24: 事件溯源机制 - 记录 CompactionCheckpoint 事件，而不抹除底层物理事件流
        let checkpoint_id = format!("ckpt-{}", now_ms());
        let summary_text = replacement.text();
        self.events.push(SessionEvent::CompactionCheckpoint {
            checkpoint_id,
            summary: summary_text,
            compacted_event_ids: Vec::new(),
            timestamp_ms: now_ms(),
        });
        self.messages
            .splice(span.start..span.end, std::iter::once(replacement));
        removed
    }

    /// 就地修剪超大的工具输出，返回被修剪的条数。
    ///
    /// 这一步**不花模型调用**：先把最占地方的部分削掉，很多时候削完就不用摘要了。
    /// 修剪必须说出来——模型看到半截输出却不知道被截过，会基于错误信息下结论。
    pub fn prune_tool_results(&mut self, max_bytes: usize) -> usize {
        let mut pruned = 0;
        for message in &mut self.messages {
            for block in &mut message.blocks {
                let ContentBlock::ToolResult(result) = block else {
                    continue;
                };
                if result.output.len() <= max_bytes {
                    continue;
                }
                let kept = take_bytes(&result.output, max_bytes);
                let omitted = result.output.len() - kept.len();
                result.output = format!("{kept}\n…（工具输出过长已修剪 {omitted} 字节）");
                pruned += 1;
            }
        }
        pruned
    }

    /// 一行一条事件的 JSONL（符合 B24 事件溯源规范）。
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for event in &self.events {
            match serde_json::to_string(event) {
                Ok(line) => {
                    out.push_str(&line);
                    out.push('\n');
                }
                Err(_) => continue,
            }
        }
        out
    }

    /// 从 JSONL 文本反序列化。支持 v1 (legacy Message) 与 v2 (SessionEvent) 世代迁移链。
    pub fn from_jsonl(text: &str) -> Result<Self> {
        let mut events = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<SessionEvent>(line) {
                events.push(event);
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<Message>(line) {
                let msg_events = message_to_events(&msg, now_ms(), index);
                events.extend(msg_events);
                continue;
            }
            return Err(Error::Session(format!(
                "会话文件第 {} 行不是合法消息: {line}",
                index + 1
            )));
        }
        if events.is_empty() {
            return Ok(Self::new());
        }
        let created_at_ms = events
            .first()
            .map(|e| e.timestamp_ms())
            .unwrap_or_else(now_ms);
        let messages = derive_messages(&events);
        Ok(Self {
            events,
            messages,
            created_at_ms,
        })
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

    /// 增量追加一条消息对应的事件。适合边跑边落盘，进程被杀也不丢已完成轮次。
    pub async fn append(path: impl AsRef<Path>, message: &Message) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Session(format!("创建会话目录失败: {e}")))?;
        }

        let now = now_ms();
        let events = message_to_events(message, now, 0);
        let mut buffer = String::new();
        for event in events {
            if let Ok(line) = serde_json::to_string(&event) {
                buffer.push_str(&line);
                buffer.push('\n');
            }
        }

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .map_err(|e| Error::Session(format!("打开会话文件失败: {e}")))?;
        file.write_all(buffer.as_bytes())
            .await
            .map_err(|e| Error::Session(format!("追加会话失败: {e}")))
    }

    /// 增量追加单个不可变物理事件。
    pub async fn append_event(path: impl AsRef<Path>, event: &SessionEvent) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Session(format!("创建会话目录失败: {e}")))?;
        }

        let mut line = serde_json::to_string(event)
            .map_err(|e| Error::Session(format!("序列化事件失败: {e}")))?;
        line.push('\n');

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .map_err(|e| Error::Session(format!("打开会话文件失败: {e}")))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Session(format!("追加事件失败: {e}")))
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

/// 单条消息的 token 估算。压缩要按"预算"而不是"条数"选范围，就得能单独算。
pub fn message_tokens(message: &Message) -> usize {
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

/// 取前 `max_bytes` 个字节，并退到最近的字符边界。
///
/// 直接按字节切会把一个汉字劈成两半，写回去就是非法 UTF-8，整份会话读不出来。
fn take_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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

    #[test]
    fn select_span_keeps_the_minimum_tail() {
        let session =
            Session::with_messages((0..10).map(|i| Message::user(format!("m{i}"))).collect());

        // 预算给得很小，仍要留下 4 条
        let span = session.select_span(1, 4).expect("应能选出范围");
        assert_eq!(span, Span { start: 0, end: 6 });
        assert_eq!(span.len(), 6);
    }

    #[test]
    fn select_span_spends_the_budget_on_more_history() {
        let session =
            Session::with_messages((0..10).map(|i| Message::user(format!("m{i}"))).collect());
        let per = message_tokens(&session.messages()[0]);

        // 预算正好够保底那 2 条，再往前一条就超 → 只保留 2 条
        let span = session.select_span(per * 2, 2).expect("应能选出范围");
        assert_eq!(session.len() - span.end, 2);
    }

    #[test]
    fn select_span_refuses_when_too_short() {
        let session = Session::with_messages(vec![Message::user("a"), Message::user("b")]);
        assert!(session.select_span(0, 2).is_none());
    }

    /// 切点落在孤立 tool_result 上时必须跨过去，否则压完的历史发不出去。
    #[test]
    fn select_span_never_cuts_a_tool_pair() {
        let session = Session::with_messages(vec![
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

        // 保底 2 条 → 切点落在 index 2，正好是一个孤立工具结果。
        let span = session.select_span(0, 2).expect("应能选出范围");
        assert_eq!(span.end, 3, "切点必须跨过孤立工具结果");
        assert!(!is_orphan_tool_result(&session.messages()[span.end]));
    }

    #[test]
    fn replace_span_swaps_and_reports_count() {
        let mut session =
            Session::with_messages((0..5).map(|i| Message::user(format!("m{i}"))).collect());

        let removed = session.replace_span(Span { start: 0, end: 3 }, Message::user("摘要"));
        assert_eq!(removed, 3);
        assert_eq!(session.len(), 3);
        assert_eq!(session.messages()[0].text(), "摘要");
        assert_eq!(session.messages()[1].text(), "m3");
    }

    #[test]
    fn replace_span_out_of_range_is_a_noop() {
        let mut session = Session::with_messages(vec![Message::user("a")]);
        assert_eq!(
            session.replace_span(Span { start: 0, end: 9 }, Message::user("x")),
            0
        );
        assert_eq!(session.len(), 1);
        assert_eq!(session.messages()[0].text(), "a");
    }

    #[test]
    fn prune_shrinks_oversized_tool_output_and_says_so() {
        let big = "长".repeat(400);
        let mut session = Session::with_messages(vec![Message::tool_results(vec![ToolResult {
            id: "c1".into(),
            ok: true,
            output: big,
        }])]);

        let pruned = session.prune_tool_results(120);
        assert_eq!(pruned, 1);

        let ContentBlock::ToolResult(result) = &session.messages()[0].blocks[0] else {
            panic!("应仍是工具结果");
        };
        assert!(result.output.len() < 400, "必须真的变小");
        assert!(result.output.contains("已修剪"), "必须说明被修剪过");
    }

    /// 按字节切会把汉字劈成两半，写回去就是非法 UTF-8。
    #[test]
    fn prune_never_splits_a_character() {
        let mut session = Session::with_messages(vec![Message::tool_results(vec![ToolResult {
            id: "c1".into(),
            ok: true,
            output: "一二三四五六七八九十".into(),
        }])]);

        session.prune_tool_results(7);
        let ContentBlock::ToolResult(result) = &session.messages()[0].blocks[0] else {
            panic!("应仍是工具结果");
        };
        // 能读出完整字符串就说明没切坏
        assert!(result.output.starts_with('一'), "{}", result.output);
    }

    #[test]
    fn prune_leaves_small_results_alone() {
        let mut session = Session::with_messages(vec![Message::tool_results(vec![ToolResult {
            id: "c1".into(),
            ok: true,
            output: "短".into(),
        }])]);
        assert_eq!(session.prune_tool_results(1024), 0);
    }
}
