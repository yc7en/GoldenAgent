use std::path::{Component, Path, PathBuf};

use goldenagent_core::{Error, Result};

/// 把用户给的路径解析到工作区内，逃逸就报错。
///
/// 这一层堵的是"模型自己编了个 ../../etc/passwd"这种情况，
/// 属于工具自身的硬边界，不依赖 policy 配置。
pub fn resolve(workspace: &Path, raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        return Err(Error::Tool("path 不能为空".into()));
    }

    let candidate = Path::new(raw);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    };

    let normalized = normalize(&absolute);
    let root = normalize(workspace);

    if !normalized.starts_with(&root) {
        return Err(Error::Tool(format!(
            "路径越界，已拒绝: {raw}（工作区: {}）",
            root.display()
        )));
    }

    Ok(normalized)
}

/// 消掉 `.` 和 `..`，避免 `a/../../b` 绕过 starts_with 检查。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// 用来展示的相对路径。
pub fn display_relative(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_escape() {
        let ws = Path::new("/work/project");
        assert!(resolve(ws, "../secret.txt").is_err());
        assert!(resolve(ws, "a/../../secret.txt").is_err());
    }

    #[test]
    fn allows_nested_path() {
        let ws = Path::new("/work/project");
        let got = resolve(ws, "src/main.rs").unwrap();
        assert_eq!(got, Path::new("/work/project/src/main.rs"));
    }
}
