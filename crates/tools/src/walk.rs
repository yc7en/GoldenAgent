//! 目录遍历：`grep` 与 `glob` 共用。
//!
//! 三个刻意的限制：
//!
//! - **不跟符号链接**。跟着走会绕出工作区（jail 只检查起点），也会撞上 `a -> ..` 这种环。
//! - **跳过已知的巨型目录**。在 `node_modules` 或 `target` 里搜东西，不是用户想要的。
//! - **有访问预算**。仓库里可能有个几万文件的目录，没有上限就是一次点开就卡死。

use std::path::{Path, PathBuf};

use goldenagent_core::{Error, Result};

use crate::jail::display_relative;

/// 遍历时整个跳过的目录名（按名字匹配，任意层级都跳）。
pub const SKIP_DIRS: [&str; 4] = [".git", "node_modules", "target", ".goldenagent"];

/// 最大递归深度。比它的路径一般意味着符号链接环或生成物。
pub const MAX_DEPTH: usize = 24;

/// 单次遍历最多看多少个目录项。
pub const MAX_VISITED: usize = 20_000;

#[derive(Debug, Clone)]
pub struct Found {
    /// 相对工作区的路径，分隔符统一成 `/`，这样输出跨平台稳定。
    pub relative: String,
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct Walk {
    pub files: Vec<Found>,
    /// 是否因为深度或预算而提前收手。要在输出里说清楚，否则用户以为"就这么多"。
    pub truncated: bool,
    /// 实际看过的目录项数量，输出里用来解释"扫了多少"。
    pub visited: usize,
}

/// 从 `start` 开始收集文件。`start` 本身是文件时就只返回它自己。
pub async fn walk_files(workspace: &Path, start: &Path) -> Result<Walk> {
    let meta = tokio::fs::metadata(start)
        .await
        .map_err(|e| Error::Tool(format!("打开 {} 失败: {e}", start.display())))?;

    if meta.is_file() {
        return Ok(Walk {
            files: vec![Found {
                relative: relative_of(workspace, start),
                path: start.to_path_buf(),
                size: meta.len(),
            }],
            truncated: false,
            visited: 1,
        });
    }
    if !meta.is_dir() {
        return Err(Error::Tool(format!(
            "{} 既不是文件也不是目录",
            start.display()
        )));
    }

    let mut walk = Walk::default();
    let mut stack = vec![(start.to_path_buf(), 0usize)];

    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            walk.truncated = true;
            continue;
        }

        // 读不动的目录直接跳过：一个权限不足的子目录不该让整次搜索失败。
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            walk.visited += 1;
            if walk.visited > MAX_VISITED {
                walk.truncated = true;
                break;
            }

            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();

            if file_type.is_dir() {
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            } else if file_type.is_file() {
                let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                walk.files.push(Found {
                    relative: relative_of(workspace, &entry.path()),
                    path: entry.path(),
                    size,
                });
            }
        }

        if walk.truncated && walk.visited > MAX_VISITED {
            break;
        }
    }

    // 排序保证输出稳定：同样的仓库，两次搜索必须给出同样的顺序。
    walk.files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(walk)
}

fn relative_of(workspace: &Path, path: &Path) -> String {
    display_relative(workspace, path).replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        // 时间戳在 Windows 上会重复（时钟粒度 100ns），并行用例撞同一个目录
        // 就会互相删文件。序号保证唯一。
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "goldenagent-walk-{tag}-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn walks_nested_files_and_skips_heavy_dirs() {
        let root = temp_dir("nested");
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/deep/a.rs"), "x").unwrap();
        std::fs::write(root.join("src/b.txt"), "y").unwrap();
        std::fs::write(root.join("node_modules/pkg/c.js"), "z").unwrap();
        std::fs::write(root.join(".git/config"), "q").unwrap();

        let walk = walk_files(&root, &root).await.unwrap();
        let names: Vec<_> = walk.files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(names, vec!["src/b.txt", "src/deep/a.rs"]);
        assert!(!walk.truncated);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn single_file_is_returned_as_is() {
        let root = temp_dir("single");
        std::fs::write(root.join("only.txt"), "hello").unwrap();

        let walk = walk_files(&root, &root.join("only.txt")).await.unwrap();
        assert_eq!(walk.files.len(), 1);
        assert_eq!(walk.files[0].relative, "only.txt");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn missing_start_is_an_error() {
        let root = temp_dir("missing");
        assert!(walk_files(&root, &root.join("nope")).await.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
