//! 多空间会话目录：一个 `sessions/` 根，下面按空间分目录。
//!
//! ```text
//! .goldenagent/sessions/
//!   _default/                        ← 「任务」：不归属任何空间的会话
//!   sp-1762137600000-3f9a/           ← 一个空间一个目录，目录名是 id 不是名字
//! ```
//!
//! **归属由存储位置决定**（A-13）：没有索引文件记「这条会话属于谁」，
//! 所以不存在「索引与目录不同步」这类永远修不干净的 bug。
//!
//! 目录名用 id 而不是空间名：名字可改、可重名、可能含路径分隔符；
//! id 走 `SessionStore::is_valid_id` 的白名单，拼路径永远安全。
//! `_default` 以下划线开头，而 id 白名单不含下划线，天然不会撞。

use std::path::{Path, PathBuf};

use goldenagent_core::{Error, Result};

use crate::store::{SessionStore, SessionSummary};
use crate::space::DEFAULT_SPACE_DIR;

/// 一条会话在磁盘上的位置。`space_id == None` 表示「任务」（默认空间）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub id: String,
    pub space_id: Option<String>,
    pub path: PathBuf,
}

/// 会话根目录的句柄。本身不持有状态，所有方法都是「按目录操作磁盘」。
#[derive(Debug, Clone)]
pub struct SessionCatalog {
    root: PathBuf,
}

impl SessionCatalog {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 默认空间（「任务」）的会话目录。
    pub fn default_dir(&self) -> PathBuf {
        self.root.join(DEFAULT_SPACE_DIR)
    }

    /// 某个空间的会话目录。`None` 就是默认空间。
    pub fn dir_of(&self, space_id: Option<&str>) -> PathBuf {
        match space_id {
            Some(id) => self.root.join(id),
            None => self.default_dir(),
        }
    }

    /// 取一个空间的会话存储句柄。
    pub fn store(&self, space_id: Option<&str>) -> SessionStore {
        match space_id {
            Some(id) => SessionStore::new(self.dir_of(Some(id))).with_space(id.to_string()),
            None => SessionStore::new(self.default_dir()),
        }
    }

    /// 列出某个空间（或全部空间）的会话。
    ///
    /// `space_id == None` 时扫全部目录：界面上的「任务 + 全部空间」是一棵树，
    /// 一次性取回来比按空间发 N 个请求简单，也不会出现半棵树。
    pub async fn list(
        &self,
        space_id: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionSummary>> {
        let mut out = Vec::new();
        for (id, dir) in self.dirs_to_scan(space_id).await? {
            let store = SessionStore::new(dir).with_space_opt(id);
            let mut listed = match store.list().await {
                Ok(listed) => listed,
                // 单个空间目录读不动（权限、被删）不该让整个列表失败：
                // 那会让侧栏整棵树消失，比少一个空间严重得多。
                Err(_) => continue,
            };
            if !include_archived {
                listed.retain(|summary| !summary.archived);
            }
            out.extend(listed);
        }

        out.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(out)
    }

    /// 跨空间找一条会话。id 全局唯一（毫秒时间戳 + 盐），找到即停。
    pub async fn locate(&self, id: &str) -> Result<Option<Located>> {
        if !SessionStore::is_valid_id(id) {
            return Err(Error::Session(format!("非法会话 id: {id:?}")));
        }
        for (space_id, dir) in self.dirs_to_scan(None).await? {
            let path = dir.join(format!("{id}.jsonl"));
            if path.exists() {
                return Ok(Some(Located {
                    id: id.to_string(),
                    space_id,
                    path,
                }));
            }
        }
        Ok(None)
    }

    /// 把某个空间里的会话全部搬进默认空间，返回搬走的条数。
    ///
    /// 删除空间时**先搬会话再改清单**：最坏情况是「清单里还有这个空间、但目录空了」，
    /// 用户看到 0 个会话，再删一次即可。反过来则留下一堆孤儿会话目录，
    /// 下次启动还得写扫描修复逻辑。
    pub async fn move_space_sessions(&self, space_id: &str) -> Result<usize> {
        let from = self.dir_of(Some(space_id));
        let to = self.default_dir();
        tokio::fs::create_dir_all(&to)
            .await
            .map_err(|e| Error::Session(format!("创建默认会话目录失败: {e}")))?;

        let mut dir = match tokio::fs::read_dir(&from).await {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(Error::Session(format!(
                    "读取空间会话目录 {} 失败: {error}",
                    from.display()
                )))
            }
        };

        let mut moved = 0usize;
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| Error::Session(format!("遍历空间会话目录失败: {e}")))?
        {
            let source = entry.path();
            if !source.is_file() {
                continue;
            }
            let Some(name) = source.file_name() else {
                continue;
            };
            let target = to.join(name);
            // 目标已存在就跳过并保留现场：宁可多一条「看得见打不开」，
            // 也不要覆盖掉一条真实的历史。
            if target.exists() {
                continue;
            }
            if tokio::fs::rename(&source, &target).await.is_ok() {
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// 把旧布局（`sessions/` 根下的散落文件）收编进 `_default/`。
    ///
    /// 只 rename，不复制、不删源；目标已存在就跳过。任何一步失败都不影响
    /// 旧文件仍可读，所以这个函数可以每次启动都跑一遍而不用担心「跑两次坏掉」。
    pub async fn migrate_flat(&self) -> Result<usize> {
        let default = self.default_dir();
        tokio::fs::create_dir_all(&default)
            .await
            .map_err(|e| Error::Session(format!("创建默认会话目录失败: {e}")))?;

        let mut dir = match tokio::fs::read_dir(&self.root).await {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(Error::Session(format!(
                    "读取会话根目录 {} 失败: {error}",
                    self.root.display()
                )))
            }
        };

        let mut moved = 0usize;
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| Error::Session(format!("遍历会话根目录失败: {e}")))?
        {
            let path = entry.path();
            if !entry
                .file_type()
                .await
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            let target = default.join(name);
            if target.exists() {
                continue;
            }
            if tokio::fs::rename(&path, &target).await.is_ok() {
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// 跨全部空间的正文搜索。归档会话默认不参与（与界面口径一致）。
    pub async fn search(
        &self,
        query: &str,
        include_archived: bool,
    ) -> Result<Vec<crate::store::SessionHits>> {
        let mut out = Vec::new();
        for (space_id, dir) in self.dirs_to_scan(None).await? {
            let store = SessionStore::new(dir).with_space_opt(space_id);
            let mut hits = store.search(query, include_archived).await?;
            out.append(&mut hits);
        }
        out.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        Ok(out)
    }

    /// 要扫的目录列表。指定空间时只扫那一个（含不存在→空列表）。
    async fn dirs_to_scan(&self, space_id: Option<&str>) -> Result<Vec<(Option<String>, PathBuf)>> {
        if let Some(id) = space_id {
            if !SessionStore::is_valid_id(id) {
                return Err(Error::Session(format!("非法空间 id: {id:?}")));
            }
            return Ok(vec![(Some(id.to_string()), self.dir_of(Some(id)))]);
        }

        let mut dirs = vec![(None, self.default_dir())];
        let mut reader = match tokio::fs::read_dir(&self.root).await {
            Ok(reader) => reader,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(dirs),
            Err(error) => {
                return Err(Error::Session(format!(
                    "读取会话根目录 {} 失败: {error}",
                    self.root.display()
                )))
            }
        };

        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| Error::Session(format!("遍历会话根目录失败: {e}")))?
        {
            if !entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name == DEFAULT_SPACE_DIR || !name.starts_with("sp-") || !SessionStore::is_valid_id(&name) {
                continue;
            }
            dirs.push((Some(name), entry.path()));
        }
        Ok(dirs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use goldenagent_core::Message;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "goldenagent-catalog-{tag}-{}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn catalog(&self) -> SessionCatalog {
            SessionCatalog::new(self.path.join("sessions"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn chat(first: &str) -> Session {
        Session::with_messages(vec![Message::user(first)])
    }

    #[tokio::test]
    async fn migrate_flat_moves_root_files_and_is_idempotent() {
        let dir = TempDir::new("migrate");
        let catalog = dir.catalog();
        std::fs::create_dir_all(catalog.default_dir()).unwrap();

        // 旧布局：会话直接躺在 sessions/ 根下（B10 及以前）
        for (id, text) in [
            ("1700000000000-aaaa", "旧一"),
            ("1700000000001-bbbb", "旧二"),
        ] {
            std::fs::write(
                catalog.root().join(format!("{id}.jsonl")),
                format!("{{\"role\":\"user\",\"blocks\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}\n"),
            )
            .unwrap();
            std::fs::write(catalog.root().join(format!("{id}.title")), "标题\n").unwrap();
        }

        assert_eq!(catalog.migrate_flat().await.unwrap(), 4);
        assert!(!catalog.root().join("1700000000000-aaaa.jsonl").exists());
        assert!(catalog.default_dir().join("1700000000000-aaaa.jsonl").exists());
        assert_eq!(catalog.list(None, true).await.unwrap().len(), 2);

        // 第二次什么都不做
        assert_eq!(catalog.migrate_flat().await.unwrap(), 0);

        // 目标已存在时不覆盖
        std::fs::write(catalog.root().join("1700000000000-aaaa.jsonl"), "{}\n").unwrap();
        assert_eq!(catalog.migrate_flat().await.unwrap(), 0);
        let kept = std::fs::read_to_string(catalog.default_dir().join("1700000000000-aaaa.jsonl"))
            .unwrap();
        assert!(kept.contains("旧一"), "不能覆盖已有会话");
    }

    #[tokio::test]
    async fn list_spans_spaces_and_locate_finds_owner() {
        let dir = TempDir::new("locate");
        let catalog = dir.catalog();

        catalog
            .store(None)
            .persist("1700000000000-aaaa", &chat("任务里的"))
            .await
            .unwrap();
        catalog
            .store(Some("sp-1700000000000-1111"))
            .persist("1700000000001-bbbb", &chat("空间里的"))
            .await
            .unwrap();

        let all = catalog.list(None, true).await.unwrap();
        assert_eq!(all.len(), 2);

        let only_space = catalog.list(Some("sp-1700000000000-1111"), true).await.unwrap();
        assert_eq!(only_space.len(), 1);
        assert_eq!(only_space[0].space_id.as_deref(), Some("sp-1700000000000-1111"));

        let located = catalog.locate("1700000000001-bbbb").await.unwrap().unwrap();
        assert_eq!(located.space_id.as_deref(), Some("sp-1700000000000-1111"));

        let default_one = catalog.locate("1700000000000-aaaa").await.unwrap().unwrap();
        assert_eq!(default_one.space_id, None);

        assert!(catalog.locate("1700000009999-ffff").await.unwrap().is_none());
        assert!(catalog.locate("../escape").await.is_err());
    }

    #[tokio::test]
    async fn move_space_sessions_lands_in_default_without_overwrite() {
        let dir = TempDir::new("move");
        let catalog = dir.catalog();
        let space = "sp-1700000000000-2222";

        catalog
            .store(Some(space))
            .persist("1700000000002-cccc", &chat("要搬的"))
            .await
            .unwrap();
        catalog
            .store(None)
            .persist("1700000000005-dddd", &chat("默认里本来就有"))
            .await
            .unwrap();
        // 同名冲突：默认空间先占位
        catalog
            .store(Some(space))
            .save("1700000000005-dddd", &chat("冲突的"))
            .await
            .unwrap();

        let moved = catalog.move_space_sessions(space).await.unwrap();
        assert_eq!(moved, 2, "只有不冲突的那条（jsonl + title）搬过去");

        let default_titles: Vec<_> = catalog
            .list(None, true)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert!(default_titles.contains(&"1700000000002-cccc".to_string()));
    }

    #[tokio::test]
    async fn archived_is_hidden_by_default() {
        let dir = TempDir::new("archived");
        let catalog = dir.catalog();
        let store = catalog.store(None);

        store.persist("1700000000003-eeee", &chat("留着")).await.unwrap();
        store.persist("1700000000004-ffff", &chat("归档")).await.unwrap();
        store.set_archived("1700000000004-ffff", true).await.unwrap();

        assert_eq!(catalog.list(None, false).await.unwrap().len(), 1);
        assert_eq!(catalog.list(None, true).await.unwrap().len(), 2);
    }
}
