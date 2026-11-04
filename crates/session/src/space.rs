//! 空间：一块可以切换的工作目录 + 一份会话存储分区。
//!
//! ```text
//! .goldenagent/
//!   spaces.json                      ← 唯一一份清单
//!   sessions/_default/               ← 「任务」：没有归属的会话
//!   sessions/sp-1762137600000-3f9a/  ← 一个空间一个目录
//! ```
//!
//! 三件事刻意不做：
//!
//! - **默认空间不入表**。界面里的「任务」就是它，路径取启动时解析出的默认工作区。
//!   写进清单只会多一个「用户能不能删掉任务」的伪问题。
//! - **不把会话存进用户的工作目录**。空间路径只是工具 jail 的根；会话集中在
//!   应用状态目录。否则删空间会碰用户的项目文件，换目录后历史还会失联。
//! - **清单读写不做增量**。整份 `tmp + rename`，与 `Session::save` 同法。
//!   清单只有几 KB，省下的那点 IO 不值得引入「写坏一半」的可能。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use goldenagent_core::{Error, Result};

use crate::catalog::SessionCatalog;

/// 空间清单文件名。
pub const MANIFEST_FILE: &str = "spaces.json";
/// 会话根目录名。
pub const SESSIONS_DIR: &str = "sessions";
/// 默认空间（「任务」）的目录名。下划线开头，与 id 白名单天然不冲突。
pub const DEFAULT_SPACE_DIR: &str = "_default";

const MANIFEST_VERSION: u32 = 1;
const NAME_MAX_CHARS: usize = 48;

/// 附加目录的上限（不含主目录）。
///
/// 不是省资源，是不想让设置界面变成一列输入框。真要更多，那说明
/// 用法已经变形了——该做的是分空间，不是往一个空间里塞。
pub const MAX_EXTRA_ROOTS: usize = 8;

/// 一个空间。`id` 进路径，`name` 只给人看。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub created_at_ms: u64,
    /// 主目录之外、同样允许工具读写的目录。
    ///
    /// `#[serde(default)]` 是本字段的全部兼容成本：B17 写下的 `spaces.json`
    /// 里没有它，读进来是空列表，行为与加这个字段之前一模一样。
    #[serde(default)]
    pub extra_paths: Vec<PathBuf>,
}

impl Space {
    /// 主根在前，附加根在后。顺序有意义：相对路径只挂第一个。
    pub fn roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::with_capacity(1 + self.extra_paths.len());
        roots.push(self.path.clone());
        roots.extend(self.extra_paths.iter().cloned());
        roots
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    spaces: Vec<Space>,
}

/// 空间清单 + 会话目录的句柄。`root` 是应用状态目录（`.goldenagent`）。
#[derive(Debug, Clone)]
pub struct SpaceStore {
    root: PathBuf,
    sessions: PathBuf,
}

impl SpaceStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let sessions = root.join(SESSIONS_DIR);
        Self { root, sessions }
    }

    /// 会话目录不在 `<state>/sessions` 时用这个（测试里常见：`.sessions`）。
    /// 两者分开传是为了不让「清单在哪」和「会话在哪」被迫绑成同一个推导。
    pub fn with_sessions_root(
        root: impl Into<PathBuf>,
        sessions: impl Into<PathBuf>,
    ) -> Self {
        Self {
            root: root.into(),
            sessions: sessions.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_FILE)
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.sessions.clone()
    }

    pub fn default_dir(&self) -> PathBuf {
        self.sessions_root().join(DEFAULT_SPACE_DIR)
    }

    pub fn space_dir(&self, id: &str) -> PathBuf {
        self.sessions_root().join(id)
    }

    pub fn catalog(&self) -> SessionCatalog {
        SessionCatalog::new(self.sessions_root())
    }

    /// 启动时备好目录、补清单、收编旧布局。三步都幂等，每次启动都跑。
    ///
    /// 返回清单读取时遇到的问题（没有就是 `None`）。读坏不是致命错误：
    /// 会话文件还在磁盘上，用户不至于「历史凭空消失」，但界面要能告诉他。
    pub async fn bootstrap(&self) -> Result<Option<String>> {
        tokio::fs::create_dir_all(self.default_dir())
            .await
            .map_err(|e| Error::Session(format!("创建会话目录失败: {e}")))?;

        let (spaces, warning) = self.load().await;
        if !self.manifest_path().exists() {
            self.write(&spaces).await?;
        }

        // 旧布局：`sessions/` 根下的散落文件收进 `_default/`。
        // 放在清单就绪之后：即使清单读坏了，历史也还能被找到。
        self.catalog().migrate_flat().await?;

        Ok(warning)
    }

    /// 读清单。损坏时备份成 `spaces.json.broken-<ms>`，以空表启动。
    pub async fn load(&self) -> (Vec<Space>, Option<String>) {
        let path = self.manifest_path();
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), None),
            Err(error) => {
                return (
                    Vec::new(),
                    Some(format!("读取空间清单 {} 失败: {error}", path.display())),
                )
            }
        };

        match serde_json::from_str::<Manifest>(&text) {
            Ok(manifest) if manifest.version <= MANIFEST_VERSION => (manifest.spaces, None),
            Ok(manifest) => (
                manifest.spaces,
                Some(format!(
                    "空间清单版本 {} 高于本程序支持的 {MANIFEST_VERSION}，较新的空间可能显示不全",
                    manifest.version
                )),
            ),
            Err(error) => {
                let backup = self.backup_manifest().await;
                let hint = match backup {
                    Some(path) => format!("已备份到 {}", path.display()),
                    None => "备份失败，原文件仍在原处".to_string(),
                };
                (
                    Vec::new(),
                    Some(format!("空间清单读不出来（{error}），{hint}")),
                )
            }
        }
    }

    pub async fn list(&self) -> Result<Vec<Space>> {
        Ok(self.load().await.0)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Space>> {
        Ok(self.load().await.0.into_iter().find(|space| space.id == id))
    }

    /// 新建空间：建目录 → 写清单。
    ///
    /// 目录不存在就创建（含父级）。这是决策 1：让用户先去资源管理器建文件夹，
    /// 是把一个「新建」操作拆成两个应用程序才能完成的动作。
    pub async fn create(
        &self,
        name: &str,
        path: impl AsRef<Path>,
        extra: &[PathBuf],
    ) -> Result<Space> {
        let name = clean_name(name)?;
        let path = normalize_path(path.as_ref())?;
        let extra_paths = clean_extra_paths(&path, extra).await?;
        let (mut spaces, _) = self.load().await;

        if spaces
            .iter()
            .any(|space| space.name.eq_ignore_ascii_case(&name))
        {
            return Err(Error::Session(format!("已经有叫「{name}」的空间了")));
        }
        if spaces.iter().any(|space| same_path(&space.path, &path)) {
            return Err(Error::Session(format!(
                "已经有一个空间指向 {}",
                path.display()
            )));
        }

        ensure_dir(&path).await?;

        let space = Space {
            id: new_space_id(),
            name,
            path,
            created_at_ms: now_ms(),
            extra_paths,
        };

        // 先建会话目录再写清单：中间态是「清单里还没有、目录空的」，
        // 反过来会留下「清单里有、目录不存在」的半个空间。
        tokio::fs::create_dir_all(self.space_dir(&space.id))
            .await
            .map_err(|e| Error::Session(format!("创建空间会话目录失败: {e}")))?;

        spaces.push(space.clone());
        self.write(&spaces).await?;
        Ok(space)
    }

    /// 改名 / 换目录 / 换附加目录。只传要改的那一项，其余传 `None`。
    ///
    /// 换目录不动历史：会话还在原空间目录里，改的只是「在哪儿干活」。
    /// `extra` 传 `Some(&[])` 表示清空附加目录——`None` 与「空列表」必须能区分，
    /// 否则界面上删光附加目录这件事就没有表达方式。
    pub async fn update(
        &self,
        id: &str,
        name: Option<&str>,
        path: Option<&Path>,
        extra: Option<&[PathBuf]>,
    ) -> Result<Space> {
        let (mut spaces, _) = self.load().await;
        let index = spaces
            .iter()
            .position(|space| space.id == id)
            .ok_or_else(|| Error::Session(format!("空间 {id} 不存在")))?;

        if let Some(name) = name {
            let clean = clean_name(name)?;
            let duplicated = spaces
                .iter()
                .enumerate()
                .any(|(i, space)| i != index && space.name.eq_ignore_ascii_case(&clean));
            if duplicated {
                return Err(Error::Session(format!("已经有叫「{clean}」的空间了")));
            }
            spaces[index].name = clean;
        }

        if let Some(path) = path {
            let normalized = normalize_path(path)?;
            let duplicated = spaces
                .iter()
                .enumerate()
                .any(|(i, space)| i != index && same_path(&space.path, &normalized));
            if duplicated {
                return Err(Error::Session(format!(
                    "已经有一个空间指向 {}",
                    normalized.display()
                )));
            }
            ensure_dir(&normalized).await?;
            spaces[index].path = normalized;
        }

        if let Some(extra) = extra {
            let primary = spaces[index].path.clone();
            spaces[index].extra_paths = clean_extra_paths(&primary, extra).await?;
        }

        let updated = spaces[index].clone();
        self.write(&spaces).await?;
        Ok(updated)
    }

    /// 删除空间：**先搬会话，再改清单，最后删目录**。
    ///
    /// 顺序不能反。先改清单再搬会话，进程被杀就留下一堆「没有归属的孤儿会话目录」，
    /// 下次启动还得写扫描修复逻辑；反过来最坏只是「清单里还有这个空间、但目录空了」，
    /// 用户看到 0 个会话，再删一次即可。让中间态是「空」而不是「孤儿」。
    ///
    /// **只删应用状态目录里的东西**，不碰用户的项目目录。
    pub async fn delete(&self, id: &str) -> Result<usize> {
        let (mut spaces, _) = self.load().await;
        let index = spaces
            .iter()
            .position(|space| space.id == id)
            .ok_or_else(|| Error::Session(format!("空间 {id} 不存在")))?;

        let moved = self.catalog().move_space_sessions(id).await?;

        // 目录这时应该是空的；移不动的（被占用）就留着，不为了删干净而失败。
        let _ = tokio::fs::remove_dir(self.space_dir(id)).await;

        spaces.remove(index);
        self.write(&spaces).await?;
        Ok(moved)
    }

    /// 空间路径是否还在。盘符没挂载、目录被移走都算不可用——
    /// 不自动删清单项：那可能只是这块盘暂时没插。
    pub fn is_available(space: &Space) -> bool {
        space.path.is_dir()
    }

    async fn write(&self, spaces: &[Space]) -> Result<()> {
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            spaces: spaces.to_vec(),
        };
        let text = serde_json::to_string_pretty(&manifest)
            .map_err(|e| Error::Session(format!("序列化空间清单失败: {e}")))?;

        let path = self.manifest_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Session(format!("创建状态目录失败: {e}")))?;
        }
        let temp = path.with_extension("json.tmp");
        tokio::fs::write(&temp, text)
            .await
            .map_err(|e| Error::Session(format!("写入空间清单失败: {e}")))?;
        tokio::fs::rename(&temp, &path)
            .await
            .map_err(|e| Error::Session(format!("替换空间清单失败: {e}")))
    }

    async fn backup_manifest(&self) -> Option<PathBuf> {
        let source = self.manifest_path();
        let target = self
            .root
            .join(format!("{MANIFEST_FILE}.broken-{}", now_ms()));
        tokio::fs::rename(&source, &target).await.ok()?;
        Some(target)
    }
}

fn clean_name(name: &str) -> Result<String> {
    let clean: String = name
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .take(NAME_MAX_CHARS)
        .collect();
    let clean = clean.trim().to_string();
    if clean.is_empty() {
        return Err(Error::Session("空间名不能为空".into()));
    }
    Ok(clean)
}

/// 空间路径必须是绝对路径。
///
/// 相对路径会相对**进程 cwd** 解析——打包后那就是安装目录，
/// 于是「我选的项目」会神秘地变成 exe 旁边的一个空文件夹。
fn normalize_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::Session(format!(
            "空间目录必须是绝对路径：{}",
            path.display()
        )));
    }
    // 只做词法清理（去掉 `.` 与末尾分隔符），不 canonicalize：
    // 目录此时可能还不存在，canonicalize 会直接失败。
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(Error::Session("空间目录不能为空".into()));
    }
    Ok(out)
}

/// 校验附加目录：绝对路径、去重、不能等于主目录、不超过上限，并顺手建出来。
///
/// 去重是必须的：同一个目录挂两次会让 jail 的"落在任一根内"判定跑两遍，
/// 更要紧的是界面上会出现两个一样的行，用户以为删掉一个就少一处授权。
async fn clean_extra_paths(primary: &Path, extra: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut cleaned: Vec<PathBuf> = Vec::with_capacity(extra.len());

    for raw in extra {
        let path = normalize_path(raw)?;
        if same_path(primary, &path) {
            // 主目录已经在里面了，再挂一次是重复。
            continue;
        }
        if cleaned.iter().any(|kept| same_path(kept, &path)) {
            continue;
        }
        if cleaned.len() >= MAX_EXTRA_ROOTS {
            return Err(Error::Session(format!(
                "附加目录最多 {MAX_EXTRA_ROOTS} 个"
            )));
        }
        cleaned.push(path);
    }

    for path in &cleaned {
        ensure_dir(path).await?;
    }
    Ok(cleaned)
}

async fn ensure_dir(path: &Path) -> Result<()> {    tokio::fs::create_dir_all(path).await.map_err(|e| {
        Error::Session(format!("创建目录 {} 失败: {e}", path.display()))
    })?;
    let is_dir = tokio::fs::metadata(path)
        .await
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    if !is_dir {
        return Err(Error::Session(format!(
            "{} 不是一个目录",
            path.display()
        )));
    }
    Ok(())
}

/// 两个路径是不是同一个地方。
///
/// 能 canonicalize 就比 canonicalize 后的结果（能认 `C:\a\..\b` 这种）；
/// 不能就用原文比，Windows 下忽略大小写。
fn same_path(a: &Path, b: &Path) -> bool {
    if let (Ok(left), Ok(right)) = (a.canonicalize(), b.canonicalize()) {
        return same_text(&left, &right);
    }
    same_text(a, b)
}

fn same_text(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

fn new_space_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    let salt = nanos ^ seq.wrapping_mul(0x9E37_79B9);
    format!("sp-{}-{:04x}", now_ms(), salt & 0xffff)
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
    use crate::Session;
    use goldenagent_core::Message;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(super) struct TempDir {
        pub(super) path: PathBuf,
    }

    impl TempDir {
        pub(super) fn new(tag: &str) -> Self {
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "goldenagent-space-{tag}-{}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        pub(super) fn store(&self) -> SpaceStore {
            SpaceStore::new(self.path.join(".goldenagent"))
        }

        pub(super) fn project(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn bootstrap_creates_manifest_and_dirs_idempotently() {
        let dir = TempDir::new("boot");
        let store = dir.store();

        assert!(store.bootstrap().await.unwrap().is_none());
        assert!(store.manifest_path().exists());
        assert!(store.default_dir().is_dir());

        // 第二次不报错、不重置已写的清单
        let space = store.create("项目", dir.project("repo"), &[]).await.unwrap();
        store.bootstrap().await.unwrap();
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, space.id);
    }

    #[tokio::test]
    async fn create_makes_missing_directory_and_rejects_duplicates() {
        let dir = TempDir::new("create");
        let store = dir.store();
        store.bootstrap().await.unwrap();

        let nested = dir.project("a/b/c");
        let space = store.create("A", &nested, &[]).await.unwrap();
        assert!(nested.is_dir(), "目录不存在时要顺手建出来");
        assert!(space.id.starts_with("sp-"));
        assert!(store.space_dir(&space.id).is_dir());

        let dup_name = store.create("a", dir.project("other"), &[]).await;
        assert!(dup_name.is_err(), "空间名查重忽略大小写");

        let dup_path = store.create("B", &nested, &[]).await;
        assert!(dup_path.is_err(), "同一目录不能建两个空间");

        // 相对路径直接拒
        assert!(store.create("C", Path::new("relative/dir"), &[]).await.is_err());
        assert!(store.create("   ", dir.project("blank"), &[]).await.is_err());
    }

    #[tokio::test]
    async fn update_renames_and_repoints_without_touching_sessions() {
        let dir = TempDir::new("update");
        let store = dir.store();
        store.bootstrap().await.unwrap();

        let space = store.create("旧名", dir.project("old"), &[]).await.unwrap();
        store
            .catalog()
            .store(Some(&space.id))
            .persist("1700000000000-aaaa", &Session::with_messages(vec![Message::user("hi")]))
            .await
            .unwrap();

        let renamed = store.update(&space.id, Some("新名"), None, None).await.unwrap();
        assert_eq!(renamed.name, "新名");
        assert_eq!(renamed.path, space.path, "改名不该动路径");

        let moved = store
            .update(&space.id, None, Some(&dir.project("new")), None)
            .await
            .unwrap();
        assert_eq!(moved.path, dir.project("new"));
        assert!(dir.project("new").is_dir());
        assert_eq!(
            store.catalog().list(Some(&space.id), true).await.unwrap().len(),
            1,
            "换目录不该搬历史"
        );
    }

    #[tokio::test]
    async fn delete_moves_sessions_then_drops_manifest_entry() {
        let dir = TempDir::new("delete");
        let store = dir.store();
        store.bootstrap().await.unwrap();

        let space = store.create("要删的", dir.project("gone"), &[]).await.unwrap();
        store
            .catalog()
            .store(Some(&space.id))
            .persist("1700000000000-aaaa", &Session::with_messages(vec![Message::user("hi")]))
            .await
            .unwrap();

        let moved = store.delete(&space.id).await.unwrap();
        assert_eq!(moved, 2, "jsonl + title 两个文件都要搬走");
        assert!(store.list().await.unwrap().is_empty());
        assert!(!store.space_dir(&space.id).exists());

        let default_list = store.catalog().list(None, true).await.unwrap();
        assert_eq!(default_list.len(), 1);
        assert_eq!(default_list[0].space_id, None, "会话变成「任务」");

        assert!(store.delete(&space.id).await.is_err(), "删两次要报错");
    }

    #[tokio::test]
    async fn broken_manifest_is_backed_up_not_fatal() {
        let dir = TempDir::new("broken");
        let store = dir.store();
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(store.manifest_path(), "{ 这不是 json").unwrap();

        let (spaces, warning) = store.load().await;
        assert!(spaces.is_empty());
        assert!(warning.is_some(), "读坏必须有话说");
        assert!(!store.manifest_path().exists(), "坏文件要被挪走");
        assert!(
            std::fs::read_dir(store.root())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".broken-")),
            "要留下一份备份"
        );
    }
}

#[cfg(test)]
mod extra_root_tests {
    use super::tests::TempDir;
    use super::*;


// ---------------------------------------------------------------------------

#[tokio::test]
async fn extra_paths_are_created_persisted_and_deduped() {
    let dir = TempDir::new("extra");
    let store = dir.store();
    let primary = dir.project("repo");
    let assets = dir.project("assets");
    let deep = dir.project("deep").join("nested"); // 父级还不存在

    let space = store
        .create(
            "项目",
            &primary,
            &[
                assets.clone(),
                deep.clone(),
                assets.clone(),  // 重复：只该留一份
                primary.clone(), // 等于主目录：不该入表
            ],
        )
        .await
        .unwrap();

    assert_eq!(
        space.extra_paths,
        vec![assets.clone(), deep.clone()],
        "附加目录要去重、且排除主目录"
    );
    assert!(assets.is_dir(), "附加目录不存在时应自动创建");
    assert!(deep.is_dir(), "父级不存在也要建出来");

    // 落盘了没有——换一个 store 重新读。
    let reopened = dir.store().list().await.unwrap();
    assert_eq!(reopened[0].extra_paths.len(), 2);
    assert_eq!(reopened[0].roots().len(), 3, "主根 + 两个附加根");
    assert_eq!(reopened[0].roots()[0], primary, "主根必须排第一");
}

#[tokio::test]
async fn extra_paths_reject_relative_and_cap_the_count() {
    let dir = TempDir::new("extra-rules");
    let store = dir.store();

    let relative = store
        .create("甲", dir.project("a"), &[PathBuf::from("relative/dir")])
        .await;
    assert!(relative.is_err(), "相对路径不该被接受");

    let mut too_many: Vec<PathBuf> = (0..=MAX_EXTRA_ROOTS)
        .map(|i| dir.project(&format!("extra-{i}")))
        .collect();
    let err = store
        .create("乙", dir.project("b"), &too_many)
        .await
        .expect_err("超过上限应报错");
    assert!(err.to_string().contains("最多"), "{err}");

    // 恰好等于上限是可以的。
    too_many.truncate(MAX_EXTRA_ROOTS);
    assert!(store
        .create("丙", dir.project("c"), &too_many)
        .await
        .is_ok());
}

#[tokio::test]
async fn update_replaces_and_clears_extra_paths() {
    let dir = TempDir::new("extra-update");
    let store = dir.store();
    let first = dir.project("first");
    let second = dir.project("second");

    let space = store
        .create("项目", dir.project("repo"), &[first.clone()])
        .await
        .unwrap();

    // None = 不动
    let untouched = store.update(&space.id, Some("改名"), None, None).await.unwrap();
    assert_eq!(untouched.extra_paths, vec![first.clone()]);

    // Some = 整份替换
    let replaced = store
        .update(&space.id, None, None, Some(&[second.clone()]))
        .await
        .unwrap();
    assert_eq!(replaced.extra_paths, vec![second]);

    // Some(&[]) = 清空。必须能和 None 区分开。
    let cleared = store.update(&space.id, None, None, Some(&[])).await.unwrap();
    assert!(cleared.extra_paths.is_empty(), "应能清空");
    assert_eq!(cleared.roots().len(), 1);
}

/// B17 写下的清单里没有 `extra_paths` 这个字段。
/// 读它必须得到空列表，而不是解析失败。
#[tokio::test]
async fn manifest_without_extra_paths_still_loads() {
    let dir = TempDir::new("legacy-manifest");
    let store = dir.store();
    std::fs::create_dir_all(dir.path.join(".goldenagent")).unwrap();
    std::fs::write(
        store.manifest_path(),
        r#"{"version":1,"spaces":[
            {"id":"sp-1-aaaa","name":"老空间","path":"P://old","created_at_ms":1}
        ]}"#,
    )
    .unwrap();

    let (spaces, warning) = store.load().await;
    assert!(warning.is_none(), "旧清单不该被当成坏文件: {warning:?}");
    assert_eq!(spaces.len(), 1);
    assert!(spaces[0].extra_paths.is_empty());
    assert_eq!(spaces[0].roots().len(), 1);
}
}
