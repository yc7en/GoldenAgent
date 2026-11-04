//! 启动时决定"这次在哪个目录里干活"。
//!
//! 开发时工作区就是仓库根（`npm run tauri dev` 的 cwd），所以这件事一直没被注意到。
//! 装成桌面应用之后问题就出来了：从开始菜单启动时，进程的 cwd 是**安装目录**，
//! 于是 Agent 的 jail 根变成了程序自己所在的文件夹——`list_dir` 列出一堆 dll，
//! `write_file` 往 exe 旁边写文件。
//!
//! 规则按"用户意图的明确程度"排：
//!
//! 1. `GOLDENAGENT_WORKSPACE` 显式指定 → 听它的，不猜；
//! 2. 当前目录像个项目（有 `.git` / `Cargo.toml` / `package.json` / `.goldenagent`）
//!    → 就用它。从项目目录里启动 exe 属于这一类；
//! 3. 都不像 → 用家目录下的 `GoldenAgent/`。宁可给一个空目录，也不要把安装目录
//!    当成用户的项目。
//!
//! 三条都不满足（没有家目录）时才退回 cwd——比 panic 强。

use std::path::{Path, PathBuf};

/// 显式指定工作区的环境变量。
pub const WORKSPACE_ENV: &str = "GOLDENAGENT_WORKSPACE";

/// 工作区是怎么定下来的。界面要显示它——
/// 用户发现"看到的文件不对"时，第一个要确认的就是这个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceOrigin {
    /// 环境变量指定。
    Env,
    /// 当前目录看起来是个项目。
    CurrentDir,
    /// 家目录下的默认目录（打包后的正常路径）。
    Home,
}

impl WorkspaceOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::CurrentDir => "cwd",
            Self::Home => "home",
        }
    }
}

/// 决定工作区。返回路径不做存在性保证——上层建 `.goldenagent/` 时会顺带创建。
pub fn resolve(cwd: &Path) -> (PathBuf, WorkspaceOrigin) {
    if let Some(raw) = std::env::var_os(WORKSPACE_ENV) {
        let path = PathBuf::from(raw);
        if !path.as_os_str().is_empty() {
            return (path, WorkspaceOrigin::Env);
        }
    }

    if looks_like_project(cwd) {
        return (cwd.to_path_buf(), WorkspaceOrigin::CurrentDir);
    }

    match home_dir() {
        Some(home) => (home.join("GoldenAgent"), WorkspaceOrigin::Home),
        None => (cwd.to_path_buf(), WorkspaceOrigin::CurrentDir),
    }
}

/// 判定"用户是不是在这个目录里干活"。
///
/// 只看几个强信号，不做递归搜索：`Cargo.toml` 可能是某个临时目录里手写的，
/// 但配合 `.git` 或 `package.json` 基本不会误判。
fn looks_like_project(dir: &Path) -> bool {
    const MARKERS: [&str; 4] = [".git", "Cargo.toml", "package.json", ".goldenagent"];
    MARKERS.iter().any(|marker| dir.join(marker).exists())
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "goldenagent-ws-{tag}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 这些用例会动进程级环境变量，彼此不能并行。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn env_overrides_everything() {
        let _guard = guard();
        let dir = temp_dir("env");
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        std::env::set_var(WORKSPACE_ENV, "/tmp/goldenagent-explicit");

        let (path, origin) = resolve(&dir);
        assert_eq!(origin, WorkspaceOrigin::Env);
        assert_eq!(path, PathBuf::from("/tmp/goldenagent-explicit"));

        std::env::remove_var(WORKSPACE_ENV);
    }

    #[test]
    fn empty_env_is_ignored() {
        let _guard = guard();
        let dir = temp_dir("empty-env");
        std::fs::write(dir.join(".git"), "").unwrap();
        std::env::set_var(WORKSPACE_ENV, "");

        let (_, origin) = resolve(&dir);
        assert_eq!(origin, WorkspaceOrigin::CurrentDir);

        std::env::remove_var(WORKSPACE_ENV);
    }

    #[test]
    fn project_dir_is_used_as_is() {
        let _guard = guard();
        let dir = temp_dir("project");
        std::fs::write(dir.join("package.json"), "{}").unwrap();

        let (path, origin) = resolve(&dir);
        assert_eq!(origin, WorkspaceOrigin::CurrentDir);
        assert_eq!(path, dir);
    }

    /// 打包后的正常路径：安装目录里没有项目标记，不能把程序目录当工作区。
    #[test]
    fn install_dir_falls_back_to_home() {
        let _guard = guard();
        let dir = temp_dir("install");

        let (path, origin) = resolve(&dir);
        assert_eq!(
            origin,
            WorkspaceOrigin::Home,
            "没有项目标记时不该用当前目录（那是安装目录）"
        );
        assert!(
            path.ends_with("GoldenAgent"),
            "应落到家目录下的 GoldenAgent: {}",
            path.display()
        );
        assert_ne!(path, dir);
    }
}
