use std::path::{Component, Path, PathBuf};

use goldenagent_core::{Error, Result};

/// 把用户给的路径解析到工作区内，逃逸就报错。
///
/// 这一层堵的是"模型自己编了个 ../../etc/passwd"这种情况，
/// 属于工具自身的硬边界，不依赖 policy 配置。
pub fn resolve(workspace: &Path, raw: &str) -> Result<PathBuf> {
    let roots = [workspace.to_path_buf()];
    resolve_in(&roots, raw)
}

/// 多根版本：`roots[0]` 是主根，其余是空间附加的目录。
///
/// **相对路径只挂主根**，不在多个根里挨个试。同一个相对路径若在两个根下
/// 都能命中（`a.txt` 两边都有），落到哪个取决于遍历顺序——这种错不报错，
/// 只读错内容，是最难查的一类。绝对路径才做多根判定，因为它本身唯一。
pub fn resolve_in(roots: &[PathBuf], raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        return Err(Error::Tool("path 不能为空".into()));
    }
    let Some(primary) = roots.first().filter(|p| !p.as_os_str().is_empty()) else {
        return Err(Error::Tool("工作区没有可用的根目录".into()));
    };

    let candidate = Path::new(raw);

    if candidate.is_absolute() {
        let normalized = normalize(candidate);
        for root in roots.iter().filter(|root| !root.as_os_str().is_empty()) {
            if normalized.starts_with(normalize(root)) {
                return Ok(normalized);
            }
        }
        return Err(Error::Tool(format!(
            "路径越界，已拒绝: {raw}（允许的目录: {}）",
            describe_roots(roots)
        )));
    }

    let normalized = normalize(&primary.join(candidate));
    let root = normalize(primary);
    if !normalized.starts_with(&root) {
        return Err(Error::Tool(format!(
            "路径越界，已拒绝: {raw}（工作区: {}）",
            root.display()
        )));
    }

    Ok(normalized)
}

fn describe_roots(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(" / ")
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

    /// 主根 + 附加根。用 temp_dir 拼：`/data/...` 在 Windows 上没有盘符，
    /// 会被 `is_absolute()` 判成相对路径，测试会变成在验另一件事。
    fn roots() -> (PathBuf, PathBuf, Vec<PathBuf>) {
        let base = std::env::temp_dir();
        let primary = base.join("ga-b18-project");
        let extra = base.join("ga-b18-assets");
        (primary.clone(), extra.clone(), vec![primary, extra])
    }

    #[test]
    fn absolute_path_inside_an_extra_root_is_allowed() {
        let (_, extra, all) = roots();
        let target = extra.join("spec.md");
        let got = resolve_in(&all, &target.to_string_lossy()).unwrap();
        assert_eq!(got, target);
    }

    #[test]
    fn absolute_path_outside_every_root_is_rejected() {
        let (_, extra, all) = roots();
        let elsewhere = std::env::temp_dir().join("ga-b18-elsewhere").join("secret.txt");
        let err = resolve_in(&all, &elsewhere.to_string_lossy()).unwrap_err();
        assert!(err.to_string().contains("越界"), "{err}");
        // 报错要把允许的目录列出来，否则用户不知道该改哪儿。
        assert!(err.to_string().contains(&extra.display().to_string()), "{err}");
    }

    /// 相对路径只挂主根——**不**去附加根里找。
    ///
    /// 这条是 B18 的关键决定 1。如果哪天有人把实现改成"挨个根试一遍"，
    /// 这条会红：`spec.md` 在附加根里存在，但相对路径仍必须落到主根下。
    #[test]
    fn relative_path_only_follows_the_primary_root() {
        let (primary, _, all) = roots();
        let got = resolve_in(&all, "spec.md").unwrap();
        assert_eq!(got, primary.join("spec.md"));
    }

    /// 附加根的前缀不能被当成"包含关系"。
    ///
    /// `<extra>-evil` 与 `<extra>` 只差一个后缀，而 Path 的 `starts_with`
    /// 是按组件比较的，所以不该误判——这条把它钉住。
    #[test]
    fn extra_root_prefix_is_not_a_containment_relation() {
        let (_, extra, all) = roots();
        let sibling = format!("{}-evil/x.md", extra.display());
        assert!(resolve_in(&all, &sibling).is_err());
    }

    #[test]
    fn empty_root_set_is_an_error_not_a_panic() {
        assert!(resolve_in(&[], "a.txt").is_err());
        // 空串也是空根：任何路径都以空前缀开头，放行就等于取消 jail。
        assert!(resolve_in(&[PathBuf::new()], "a.txt").is_err());
    }
}
