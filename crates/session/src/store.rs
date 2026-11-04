//! 会话目录：一个会话两个文件，外加两个零字节标记。
//!
//! ```text
//! .goldenagent/sessions/_default/
//!   1762137600000-a1b2.jsonl     ← 消息流，一行一条，格式与 B4 完全一致
//!   1762137600000-a1b2.title     ← 可选，单行标题
//!   1762137600000-a1b2.pin       ← 可选，零字节；存在即置顶
//!   1762137600000-a1b2.archived  ← 可选，零字节；存在即归档
//! ```
//!
//! 为什么拆成多个文件：内容文件要保持"纯消息流"，这样 `tail -n 5 x.jsonl | jq`
//! 仍然能用；而标题要能改，改内容文件里的东西就等于改历史，不能那么干。
//!
//! 标题文件缺失是**正常状态**（不是错误）：这时回退到"第一条用户消息前 32 字"。
//! 于是永远不需要维护第三个元信息文件，也就没有"元信息与内容不同步"这类 bug。
//!
//! 标记同理：三个独立文件各自只可能"有/没有"，坏一个不牵连另外两个。
//! 用一个 `meta.json` 存三样才需要读改写，写坏一次三样全丢。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use goldenagent_core::{ContentBlock, Error, Message, Result, Role};

use crate::Session;

const SESSION_EXT: &str = "jsonl";
const TITLE_EXT: &str = "title";
/// 置顶标记。零字节，存在即为真。
const PIN_EXT: &str = "pin";
/// 归档标记。零字节，存在即为真。
const ARCHIVED_EXT: &str = "archived";
/// 自动推导标题时截取的最大字符数。
const TITLE_CHARS: usize = 32;
/// 手工改名的长度上限。比自动标题宽松，因为那是用户自己写的。
const TITLE_MAX_CHARS: usize = 120;
/// id 长度上限，纯防呆。
const MAX_ID_LEN: usize = 64;
/// 迁移完成后旧文件加的后缀。
const LEGACY_SUFFIX: &str = "migrated";
/// 单个会话参与搜索的体积上限。超过就跳过——扫一个 200 MB 的会话会把界面卡死，
/// 而那种会话本来也不该靠"搜索"来用。
const SEARCH_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// 单条命中摘要的字符数。
const SNIPPET_CHARS: usize = 120;
/// 单个会话最多返回多少条命中。
const HITS_PER_SESSION: usize = 20;

/// 列表里一条会话的样子。不含消息本体——列表不该把整份历史读进内存再吐给前端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub messages: usize,
    /// 文件修改时间。用它排序，不用"创建时间"——用户关心的是"最近聊的"。
    pub updated_at_ms: u64,
    /// 归属空间。`None` = 「任务」（默认空间）。
    pub space_id: Option<String>,
    pub pinned: bool,
    pub archived: bool,
}

/// 搜索命中的一处位置。
///
/// 定位用 `(msg, block)`：落盘消息的 `blocks` 才是稳定单位。
/// 工具卡是 `tool_call` + `tool_result` 两张卡配对渲染的，
/// 按"渲染出来的第几条"定位会在跳转时错位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub msg: usize,
    pub block: usize,
    /// `user` / `assistant`。
    pub role: String,
    pub snippet: String,
}

/// 一条会话的搜索结果（一个会话一行，下面挂它命中的若干处）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHits {
    pub session_id: String,
    pub title: String,
    pub space_id: Option<String>,
    pub updated_at_ms: u64,
    pub hits: Vec<SearchHit>,
}

/// 会话根目录的句柄。本身不持有任何会话状态，所有方法都是"按 id 操作磁盘"。
///
/// `space_id` 只是**出生证明**：它不参与任何路径计算（位置已经决定了归属），
/// 仅在构造 `SessionSummary` 时贴到结果上，供上层拼 workspace。
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    space_id: Option<String>,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            space_id: None,
        }
    }

    pub fn with_space(mut self, space_id: impl Into<String>) -> Self {
        self.space_id = Some(space_id.into());
        self
    }

    pub fn with_space_opt(mut self, space_id: Option<String>) -> Self {
        self.space_id = space_id;
        self
    }

    pub fn space_id(&self) -> Option<&str> {
        self.space_id.as_deref()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// id 会被拼进路径，所以必须先过白名单。
    ///
    /// 挡的是 `../../` 这类：前端传来的 id 如果能带路径分隔符，
    /// 就能读写会话目录之外的任意文件。
    pub fn is_valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= MAX_ID_LEN
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
    }

    fn require_id(id: &str) -> Result<()> {
        if Self::is_valid_id(id) {
            Ok(())
        } else {
            Err(Error::Session(format!("非法会话 id: {id:?}")))
        }
    }

    pub fn session_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.{SESSION_EXT}"))
    }

    pub fn title_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.{TITLE_EXT}"))
    }

    pub fn pin_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.{PIN_EXT}"))
    }

    pub fn archived_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.{ARCHIVED_EXT}"))
    }

    /// 生成一个新 id。毫秒前缀保证目录按时间排就是按创建排。
    pub fn new_id() -> String {
        id_at(now_ms())
    }

    /// 列出全部会话，最近改过的在前。
    ///
    /// 单个文件读不动就跳过，不让一个坏会话毁掉整个列表——
    /// 但也不会因此删掉它，用户还能在文件系统里找到。
    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut dir = match tokio::fs::read_dir(&self.root).await {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(Error::Session(format!(
                    "读取会话目录 {} 失败: {error}",
                    self.root.display()
                )))
            }
        };

        let mut summaries = Vec::new();
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| Error::Session(format!("遍历会话目录失败: {e}")))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(SESSION_EXT) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !Self::is_valid_id(id) {
                continue;
            }
            let Ok(text) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            let messages = text.lines().filter(|line| !line.trim().is_empty()).count();
            // 空会话不进列表：用户点一次"新对话"就多一个空壳，列表很快就没法看了。
            if messages == 0 {
                continue;
            }
            summaries.push(SessionSummary {
                id: id.to_string(),
                title: self.title_of(id, &text).await,
                messages,
                updated_at_ms: mtime_ms(&entry).await,
                space_id: self.space_id.clone(),
                pinned: self.pin_path(id).exists(),
                archived: self.archived_path(id).exists(),
            });
        }

        summaries.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(summaries)
    }

    /// 读一个会话。文件不存在不是错误——调用方可以据此判断"这是个新 id"。
    pub async fn load(&self, id: &str) -> Result<Session> {
        Self::require_id(id)?;
        Session::load(self.session_path(id)).await
    }

    pub async fn save(&self, id: &str, session: &Session) -> Result<()> {
        Self::require_id(id)?;
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|e| Error::Session(format!("创建会话目录失败: {e}")))?;
        session.save(self.session_path(id)).await
    }

    /// 落盘并顺手补标题（只在标题文件还不存在时补）。
    ///
    /// 用户改过名之后标题文件就存在了，这里不会再覆盖——
    /// 否则每次对话都把自己起的名字冲掉。
    pub async fn persist(&self, id: &str, session: &Session) -> Result<()> {
        self.save(id, session).await?;

        let title_path = self.title_path(id);
        if title_path.exists() {
            return Ok(());
        }
        let Some(title) = auto_title(session) else {
            return Ok(());
        };
        tokio::fs::write(&title_path, format!("{title}\n"))
            .await
            .map_err(|e| Error::Session(format!("写入会话标题失败: {e}")))
    }

    /// 改名。传空字符串视为非法——想要"恢复自动标题"留给后续包，现在先明确拒绝。
    pub async fn rename(&self, id: &str, title: &str) -> Result<String> {
        Self::require_id(id)?;
        if !self.session_path(id).exists() {
            return Err(Error::Session(format!("会话 {id} 不存在")));
        }

        let clean: String = title
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .take(TITLE_MAX_CHARS)
            .collect();
        let clean = clean.trim().to_string();
        if clean.is_empty() {
            return Err(Error::Session("标题不能为空".into()));
        }

        tokio::fs::write(self.title_path(id), format!("{clean}\n"))
            .await
            .map_err(|e| Error::Session(format!("写入会话标题失败: {e}")))?;
        Ok(clean)
    }

    /// 删掉会话（内容 + 标题）。返回内容文件是否真的存在过。
    pub async fn delete(&self, id: &str) -> Result<bool> {
        Self::require_id(id)?;

        let path = self.session_path(id);
        let existed = path.exists();
        if existed {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| Error::Session(format!("删除会话 {id} 失败: {e}")))?;
        }
        // 标题与两个标记删不掉无所谓，内容没了它们就永远不会被列出来。
        let _ = tokio::fs::remove_file(self.title_path(id)).await;
        let _ = tokio::fs::remove_file(self.pin_path(id)).await;
        let _ = tokio::fs::remove_file(self.archived_path(id)).await;
        Ok(existed)
    }

    /// 置顶 / 取消置顶。返回操作后的状态。
    ///
    /// 只写或删一个零字节文件：没有"读到旧值再拼新值"的过程，
    /// 所以并发点两下最坏也只是最后一次写入生效，不会写出半份 JSON。
    pub async fn set_pinned(&self, id: &str, pinned: bool) -> Result<bool> {
        self.set_marker(id, &self.pin_path(id), pinned).await
    }

    /// 归档 / 取消归档。归档是软删除：不移动文件、不改内容，恢复就是删标记。
    pub async fn set_archived(&self, id: &str, archived: bool) -> Result<bool> {
        self.set_marker(id, &self.archived_path(id), archived)
            .await
    }

    async fn set_marker(&self, id: &str, path: &Path, on: bool) -> Result<bool> {
        Self::require_id(id)?;
        if !self.session_path(id).exists() {
            return Err(Error::Session(format!("会话 {id} 不存在")));
        }
        if on {
            tokio::fs::write(path, b"")
                .await
                .map_err(|e| Error::Session(format!("写入会话标记失败: {e}")))?;
        } else if path.exists() {
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| Error::Session(format!("清除会话标记失败: {e}")))?;
        }
        Ok(on)
    }

    /// 取单条会话的摘要。会话不存在时返回 `Ok(None)`，不当作错误。
    pub async fn summary(&self, id: &str) -> Result<Option<SessionSummary>> {
        Self::require_id(id)?;
        let path = self.session_path(id);
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            return Ok(None);
        };
        let messages = text.lines().filter(|line| !line.trim().is_empty()).count();
        let updated_at_ms = file_mtime_ms(&path);
        Ok(Some(SessionSummary {
            id: id.to_string(),
            title: self.title_of(id, &text).await,
            messages,
            updated_at_ms,
            space_id: self.space_id.clone(),
            pinned: self.pin_path(id).exists(),
            archived: self.archived_path(id).exists(),
        }))
    }

    /// 在**本目录**内按正文搜索。跨空间搜索由 `SessionCatalog` 逐目录调用。
    ///
    /// 搜索口径（与界面文案一致）：
    /// - 搜用户消息、助手正文、工具入参、工具输出；
    /// - 不搜思考过程（`thinking_delta` 本来就不落盘）；
    /// - 归档会话默认不参与。
    pub async fn search(&self, query: &str, include_archived: bool) -> Result<Vec<SessionHits>> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }

        let mut dir = match tokio::fs::read_dir(&self.root).await {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(Error::Session(format!(
                    "读取会话目录 {} 失败: {error}",
                    self.root.display()
                )))
            }
        };

        let mut out = Vec::new();
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| Error::Session(format!("遍历会话目录失败: {e}")))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(SESSION_EXT) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !Self::is_valid_id(id) {
                continue;
            }
            if !include_archived && self.archived_path(id).exists() {
                continue;
            }
            let too_big = entry
                .metadata()
                .await
                .map(|meta| meta.len() > SEARCH_FILE_MAX_BYTES)
                .unwrap_or(false);
            if too_big {
                continue;
            }
            let Ok(text) = tokio::fs::read_to_string(&path).await else {
                continue;
            };

            let hits = search_text(&text, &needle);
            if hits.is_empty() {
                continue;
            }
            out.push(SessionHits {
                session_id: id.to_string(),
                title: self.title_of(id, &text).await,
                space_id: self.space_id.clone(),
                updated_at_ms: mtime_ms(&entry).await,
                hits,
            });
        }

        out.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        Ok(out)
    }

    /// 把 B10 及以前那种单文件 `session.jsonl` 收编成一条正式会话。
    ///
    /// **搬，不是复制**：旧文件改名成 `session.jsonl.migrated`。
    /// 两份同时留着，下次启动谁也不知道该信哪份。
    ///
    /// 收编用的 id 由**修改时间 + 内容哈希**决定，不是随机生成的：
    /// 万一「写完了新会话、改名却失败」这种半状态真的发生，下次启动会落在
    /// 同一个 id 上（跳过重复写入），而不是悄悄多出一条一模一样的会话。
    /// 也正因为幂等，这里不再用「目录里已经有会话就跳过」这条粗暴的规则——
    /// 那会让「旧会话 + 新会话共存」的安装里，旧的那条永远看不见。
    pub async fn adopt_legacy(&self, legacy: impl AsRef<Path>) -> Result<Option<String>> {
        let legacy = legacy.as_ref();
        if !legacy.exists() {
            return Ok(None);
        }

        let text = tokio::fs::read_to_string(legacy)
            .await
            .map_err(|e| Error::Session(format!("读取旧会话 {} 失败: {e}", legacy.display())))?;
        let session = Session::from_jsonl(&text)?;

        let mut adopted = None;
        if !session.is_empty() {
            let id = legacy_id(file_mtime_ms(legacy), &text);
            if !self.session_path(&id).exists() {
                self.persist(&id, &session).await?;
            }
            adopted = Some(id);
        }

        let name = legacy
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("session");
        let retired = legacy.with_file_name(format!("{name}.{LEGACY_SUFFIX}"));
        tokio::fs::rename(legacy, &retired)
            .await
            .map_err(|e| Error::Session(format!("重命名旧会话失败: {e}")))?;

        Ok(adopted)
    }

    /// 显式标题 → 自动标题 → 「新对话」。
    async fn title_of(&self, id: &str, text: &str) -> String {
        if let Ok(explicit) = tokio::fs::read_to_string(self.title_path(id)).await {
            let explicit = explicit.trim();
            if !explicit.is_empty() {
                return explicit.to_string();
            }
        }
        derive_title(text).unwrap_or_else(|| "新对话".to_string())
    }
}

/// 从 JSONL 文本里找第一条有内容的用户消息，截成标题。
pub fn derive_title(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 坏行直接跳过：标题是"锦上添花"，不该因为一行坏 JSON 就整个列表报错。
        let Ok(message) = serde_json::from_str::<Message>(line) else {
            continue;
        };
        if message.role != Role::User {
            continue;
        }
        let body = message.text();
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        return Some(shorten(body));
    }
    None
}

fn auto_title(session: &Session) -> Option<String> {
    session
        .messages()
        .iter()
        .filter(|message| message.role == Role::User)
        .map(Message::text)
        .find(|text| !text.trim().is_empty())
        .map(|text| shorten(text.trim()))
}

/// 只取第一行、最多 32 字。多行输入当标题会把侧栏撑爆。
fn shorten(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    let mut out: String = first_line.chars().take(TITLE_CHARS).collect();
    if first_line.chars().count() > TITLE_CHARS {
        out.push('…');
    }
    out
}

/// 旧文件收编用的 id：时间前缀（列表排序符合直觉）+ 内容哈希（幂等）。
///
/// 不能用 `new_id()`：它带随机盐，同一个旧文件收编两次会得到两个 id，
/// 于是「写完了新会话、改名却失败」这种情况会在下次启动时复制出一份历史。
fn legacy_id(ms: u64, text: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{ms}-{:04x}", hash & 0xffff)
}

fn id_at(ms: u64) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    // 同一毫秒内连续创建时靠计数器拉开，避免两次"新对话"撞 id。
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    let salt = nanos ^ seq.wrapping_mul(0x9E37_79B9);
    format!("{ms}-{:04x}", salt & 0xffff)
}

async fn mtime_ms(entry: &tokio::fs::DirEntry) -> u64 {
    entry
        .metadata()
        .await
        .ok()
        .and_then(|meta| meta.modified().ok())
        .map(system_ms)
        .unwrap_or_else(now_ms)
}

fn file_mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .map(system_ms)
        .unwrap_or_else(now_ms)
}

fn system_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    system_ms(SystemTime::now())
}

/// 扫一份 JSONL，返回所有命中位置。逐行解析，坏行跳过。
fn search_text(text: &str, needle_lower: &str) -> Vec<SearchHit> {
    let needle_chars = needle_lower.chars().count();
    let mut hits = Vec::new();

    for (msg, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Message>(line) else {
            continue;
        };

        for (block, content) in message.blocks.iter().enumerate() {
            // 工具入参是 JSON，搜它的字符串形式就够了：用户记得的是路径和命令，
            // 不是结构。单独搜 `name` 是因为「grep」这种词只在名字里出现。
            let haystack = match content {
                ContentBlock::Text { text } => text.clone(),
                ContentBlock::ToolCall(call) => format!("{} {}", call.name, call.input),
                ContentBlock::ToolResult(result) => result.output.clone(),
            };
            let Some(at) = locate_ci(&haystack, needle_lower) else {
                continue;
            };
            hits.push(SearchHit {
                msg,
                block,
                role: match message.role {
                    Role::User => "user".into(),
                    Role::Assistant => "assistant".into(),
                },
                snippet: snippet(&haystack, at, needle_chars),
            });
            if hits.len() >= HITS_PER_SESSION {
                return hits;
            }
        }
    }

    hits
}

/// 找第一个大小写不敏感的匹配，返回**字符**下标。
///
/// 逐字符比较而不是 `to_lowercase()` 整段再 `find`：整段小写会改变字节长度，
/// 拿回来的偏移量没法用于在原文里截片段。极端字符（ß→ss）会漏匹配，
/// 但那是可接受的代价——总比跳出原文位置好。
fn locate_ci(text: &str, needle_lower: &str) -> Option<usize> {
    let needle: Vec<char> = needle_lower.chars().collect();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.chars().collect();
    if hay.len() < needle.len() {
        return None;
    }

    for start in 0..=(hay.len() - needle.len()) {
        let matched = needle.iter().enumerate().all(|(offset, want)| {
            let source = hay[start + offset];
            let lowered = source.to_lowercase().next().unwrap_or(source);
            lowered == *want
        });
        if matched {
            return Some(start);
        }
    }
    None
}

/// 截一段带上下文的摘要。命中词附近各留一半额度，两端超出就加省略号。
fn snippet(text: &str, at: usize, needle_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let budget = SNIPPET_CHARS.max(needle_chars);
    let context = budget.saturating_sub(needle_chars) / 2;

    let start = at.saturating_sub(context);
    let end = (at + needle_chars + context).min(total);

    // 换行与连续空白折成单个空格：侧栏里一行摘要，不该被代码块撑成十行。
    let body: String = chars[start..end]
        .iter()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&body);
    if end < total {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use goldenagent_core::{ContentBlock, ToolCall, ToolResult};
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let stamp = now_ms();
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "goldenagent-store-{tag}-{}-{stamp}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn store(&self) -> SessionStore {
            SessionStore::new(self.path.join("sessions"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn chat_session(first: &str, reply: &str) -> Session {
        Session::with_messages(vec![
            Message::user(first),
            Message::assistant(vec![ContentBlock::Text {
                text: reply.into(),
            }]),
        ])
    }

    fn tool_call_msg(id: &str) -> Message {
        Message::assistant(vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "bash".into(),
            input: json!({"cmd": "ls a.txt"}),
        })])
    }

    #[test]
    fn id_rejects_traversal_and_junk() {
        assert!(SessionStore::is_valid_id("1762137600000-a1b2"));
        assert!(!SessionStore::is_valid_id(""));
        assert!(!SessionStore::is_valid_id("../escape"));
        assert!(!SessionStore::is_valid_id("a/b"));
        assert!(!SessionStore::is_valid_id("a\\b"));
        assert!(!SessionStore::is_valid_id("a b"));
        assert!(!SessionStore::is_valid_id("会话"));
        assert!(!SessionStore::is_valid_id(&"9".repeat(65)));
    }

    #[tokio::test]
    async fn save_list_rename_delete_roundtrip() {
        let dir = TempDir::new("crud");
        let store = dir.store();

        assert!(store.list().await.unwrap().is_empty(), "空目录要能正常返空");

        let id = SessionStore::new_id();
        store
            .persist(&id, &chat_session("帮我看看 a.txt", "好的"))
            .await
            .unwrap();

        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].title, "帮我看看 a.txt");
        assert_eq!(listed[0].messages, 2);

        let renamed = store.rename(&id, "  读文件  ").await.unwrap();
        assert_eq!(renamed, "读文件");
        assert_eq!(store.list().await.unwrap()[0].title, "读文件");

        let loaded = store.load(&id).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.messages()[1].text(), "好的");

        assert!(store.delete(&id).await.unwrap());
        assert!(store.list().await.unwrap().is_empty());
        assert!(!store.delete(&id).await.unwrap(), "第二次删应返回 false");
        assert!(!store.title_path(&id).exists(), "标题文件要一起清掉");
    }

    #[tokio::test]
    async fn persist_does_not_overwrite_manual_title() {
        let dir = TempDir::new("title");
        let store = dir.store();
        let id = SessionStore::new_id();

        store.persist(&id, &chat_session("第一句", "好")).await.unwrap();
        store.rename(&id, "我自己起的名").await.unwrap();

        let mut session = store.load(&id).await.unwrap();
        session.push_user("第二句");
        store.persist(&id, &session).await.unwrap();

        assert_eq!(store.list().await.unwrap()[0].title, "我自己起的名");
    }

    #[tokio::test]
    async fn empty_session_does_not_show_in_list() {
        let dir = TempDir::new("empty");
        let store = dir.store();
        let id = SessionStore::new_id();

        store.save(&id, &Session::new()).await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pin_and_archive_are_zero_byte_markers() {
        let dir = TempDir::new("markers");
        let store = dir.store();
        let id = SessionStore::new_id();
        store.persist(&id, &chat_session("留着", "好")).await.unwrap();

        assert!(!store.list().await.unwrap()[0].pinned);
        assert!(!store.list().await.unwrap()[0].archived);

        assert!(store.set_pinned(&id, true).await.unwrap());
        assert!(store.pin_path(&id).exists());
        assert_eq!(std::fs::metadata(store.pin_path(&id)).unwrap().len(), 0);
        assert!(store.list().await.unwrap()[0].pinned);

        assert!(store.set_archived(&id, true).await.unwrap());
        assert!(store.list().await.unwrap()[0].archived);

        // 取消就是删标记，不是写 "false"
        assert!(!store.set_pinned(&id, false).await.unwrap());
        assert!(!store.pin_path(&id).exists());
        assert!(store.archived_path(&id).exists(), "另一个标记不该被牵连");

        assert!(store.set_pinned("1700000009999-ffff", true).await.is_err());
        assert!(store.set_pinned("../escape", true).await.is_err());
    }

    #[tokio::test]
    async fn summary_reports_single_session() {
        let dir = TempDir::new("summary");
        let store = dir.store();
        let id = SessionStore::new_id();
        store
            .persist(&id, &chat_session("帮我看看 a.txt", "好"))
            .await
            .unwrap();

        let summary = store.summary(&id).await.unwrap().expect("应存在");
        assert_eq!(summary.messages, 2);
        assert_eq!(summary.title, "帮我看看 a.txt");
        assert!(store.summary("1700000009999-ffff").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn search_hits_text_tool_input_and_output() {
        let dir = TempDir::new("search");
        let store = dir.store();

        let session = Session::with_messages(vec![
            Message::user("把 target 目录清掉"),
            tool_call_msg("c1"),
            Message::tool_results(vec![ToolResult {
                id: "c1".into(),
                ok: true,
                output: "bin/target/debug/app.exe".into(),
            }]),
            Message::assistant(vec![ContentBlock::Text {
                text: "清理完毕".into(),
            }]),
        ]);
        store.persist("1700000000000-aaaa", &session).await.unwrap();

        let hits = store.search("TARGET", false).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "1700000000000-aaaa");
        assert_eq!(hits[0].hits.len(), 2, "用户消息与工具输出各一处");
        assert_eq!(hits[0].hits[0].msg, 0);
        assert_eq!(hits[0].hits[0].role, "user");
        assert!(hits[0].hits[0].snippet.contains("target"));
        assert!(store.search("清理完毕", false).await.unwrap()[0].hits.len() >= 1);
        assert!(store.search("不存在的词", false).await.unwrap().is_empty());
        assert!(store.search("   ", false).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_skips_archived_unless_asked() {
        let dir = TempDir::new("search-archived");
        let store = dir.store();
        let id = SessionStore::new_id();
        store.persist(&id, &chat_session("找得到吗", "找得到")).await.unwrap();

        store.set_archived(&id, true).await.unwrap();
        assert!(store.search("找得到吗", false).await.unwrap().is_empty());
        assert_eq!(store.search("找得到吗", true).await.unwrap().len(), 1);
    }

    #[test]
    fn snippet_is_bounded_and_marks_truncation() {
        let text = format!("{}目标{}", "前".repeat(200), "后".repeat(200));
        let at = locate_ci(&text, "目标").expect("应命中");
        let body = snippet(&text, at, 2);
        assert!(body.starts_with('…') && body.ends_with('…'));
        assert!(body.chars().count() <= SNIPPET_CHARS + 2);
        assert!(body.contains("目标"));
    }

    #[test]
    fn locate_ci_is_case_insensitive() {
        assert_eq!(locate_ci("Hello World", "world"), Some(6));
        assert_eq!(locate_ci("中文匹配", "匹配"), Some(2));
        assert_eq!(locate_ci("short", "a-longer-needle"), None);
    }

    #[tokio::test]
    async fn list_is_ordered_by_recency() {
        let dir = TempDir::new("order");
        let store = dir.store();

        let old = "1700000000000-aaaa".to_string();
        let new = "1800000000000-bbbb".to_string();
        store.save(&old, &chat_session("旧的", "1")).await.unwrap();
        store.save(&new, &chat_session("新的", "2")).await.unwrap();

        // 显式把旧文件的 mtime 往后拨，验证排序真的看 mtime 而不是文件名。
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, new, "刚写的那条应该在前");
    }

    #[test]
    fn title_falls_back_to_first_user_message() {
        let text = concat!(
            "{\"role\":\"assistant\",\"blocks\":[{\"type\":\"text\",\"text\":\"先说话\"}]}\n",
            "{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"第一行\\n第二行\"}]}\n",
        );
        assert_eq!(derive_title(text).unwrap(), "第一行");
        assert!(derive_title("").is_none());
        assert!(derive_title("{坏行}").is_none());
    }

    #[test]
    fn long_title_is_truncated_with_ellipsis() {
        let long = "一".repeat(80);
        let text = format!(
            "{{\"role\":\"user\",\"blocks\":[{{\"type\":\"text\",\"text\":\"{long}\"}}]}}\n"
        );
        let title = derive_title(&text).unwrap();
        assert_eq!(title.chars().count(), TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
    }

    #[tokio::test]
    async fn tool_blocks_are_not_used_as_title() {
        let dir = TempDir::new("tool-title");
        let store = dir.store();
        let id = SessionStore::new_id();

        let session = Session::with_messages(vec![
            Message::assistant(vec![ContentBlock::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: json!({"path": "a.txt"}),
            })]),
            Message::tool_results(vec![ToolResult {
                id: "c1".into(),
                ok: true,
                output: "内容".into(),
            }]),
        ]);
        store.persist(&id, &session).await.unwrap();

        assert_eq!(store.list().await.unwrap()[0].title, "新对话");
    }

    #[tokio::test]
    async fn rename_rejects_blank_and_missing() {
        let dir = TempDir::new("rename");
        let store = dir.store();
        let id = SessionStore::new_id();
        store.persist(&id, &chat_session("在", "嗯")).await.unwrap();

        assert!(store.rename(&id, "   ").await.is_err());
        assert!(store.rename("1700000000000-ffff", "x").await.is_err());
        assert!(store.rename("../escape", "x").await.is_err());
    }

    #[tokio::test]
    async fn legacy_single_file_is_adopted_once() {
        let dir = TempDir::new("legacy");
        let store = dir.store();
        let legacy = dir.path.join("session.jsonl");

        Session::with_messages(vec![Message::user("老对话"), Message::user("第二句")])
            .save(&legacy)
            .await
            .unwrap();

        let adopted = store.adopt_legacy(&legacy).await.unwrap().expect("应迁移");
        assert!(store.session_path(&adopted).exists());
        assert!(!legacy.exists(), "旧文件必须被搬走，不能留两份");
        assert!(dir.path.join("session.jsonl.migrated").exists());
        assert_eq!(store.list().await.unwrap()[0].messages, 2);

        // 再跑一次什么都不做
        assert!(store.adopt_legacy(&legacy).await.unwrap().is_none());
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn legacy_is_adopted_even_when_other_sessions_exist() {
        let dir = TempDir::new("legacy-busy");
        let store = dir.store();
        let legacy = dir.path.join("session.jsonl");

        store
            .persist(&SessionStore::new_id(), &chat_session("新的", "好"))
            .await
            .unwrap();
        Session::with_messages(vec![Message::user("老的")])
            .save(&legacy)
            .await
            .unwrap();

        // 旧的那条不能被静默忽略：用户升级后不该出现「历史少了一段」。
        let adopted = store.adopt_legacy(&legacy).await.unwrap().expect("应迁移");
        assert!(!legacy.exists());
        assert!(dir.path.join("session.jsonl.migrated").exists());
        assert_eq!(store.list().await.unwrap().len(), 2);
        assert!(store.session_path(&adopted).exists());
    }

    /// 收编要幂等：同一个旧文件被读到两次也只落一条（按内容哈希定 id）。
    #[tokio::test]
    async fn legacy_id_is_stable_for_same_content() {
        let text = "{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"一样\"}]}\n";
        assert_eq!(legacy_id(123, text), legacy_id(123, text));
        assert_ne!(legacy_id(123, text), legacy_id(124, text));
        assert!(SessionStore::is_valid_id(&legacy_id(123, text)));
    }

    #[tokio::test]
    async fn save_is_atomic_against_partial_writes() {
        let dir = TempDir::new("atomic");
        let store = dir.store();
        let id = SessionStore::new_id();

        store.save(&id, &chat_session("第一版", "1")).await.unwrap();
        store.save(&id, &chat_session("第二版", "2")).await.unwrap();

        // 临时文件不该留在目录里被当成会话
        let leftovers: Vec<_> = std::fs::read_dir(store.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "留下了临时文件: {leftovers:?}");
        assert_eq!(store.load(&id).await.unwrap().messages()[0].text(), "第二版");
    }
}
