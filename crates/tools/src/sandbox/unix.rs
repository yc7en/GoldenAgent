use std::path::Path;

/// 检查系统是否安装了 Bubblewrap (bwrap)。
pub fn has_bwrap() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// 使用 bwrap 包装待执行的命令与工作区读写绑定。
pub fn wrap_with_bwrap(
    cmd: &mut tokio::process::Command,
    workspace: &Path,
    extra_roots: &[impl AsRef<Path>],
) {
    if !has_bwrap() {
        return;
    }

    // 采用 bwrap 进行命名空间只读隔离
    // 根目录只读绑定，仅工作区目录和 /tmp 具有读写权限
    let mut args = vec![
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
        "--bind".to_string(),
        workspace.to_string_lossy().to_string(),
        workspace.to_string_lossy().to_string(),
    ];

    for extra in extra_roots {
        let p = extra.as_ref().to_string_lossy().to_string();
        args.push("--bind".to_string());
        args.push(p.clone());
        args.push(p);
    }

    args.push("--die-with-parent".to_string());
    // 接入原有参数
}
