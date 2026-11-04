//! 工具层集成测试：真文件系统，临时目录隔离。
//!
//! 这一层的验收重点是**边界**，不是"能读能写"：
//! 越界必须拒绝、超大必须拒绝、写进去能原样读回来。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use goldenagent_tools::{Registry, ToolContext};
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "goldenagent-tools-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn ctx(&self) -> ToolContext {
        ToolContext::new(&self.path)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn registry() -> Registry {
    Registry::with_defaults()
}

#[tokio::test]
async fn write_then_read_roundtrip() {
    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let reg = registry();
    let body = "第一行\n第二行\n";

    let written = reg
        .run("write_file", json!({"path": "a.txt", "content": body}), &ctx)
        .await
        .unwrap();
    assert!(written.contains("已创建"), "unexpected: {written}");

    let read = reg
        .run("read_file", json!({"path": "a.txt"}), &ctx)
        .await
        .unwrap();
    assert!(read.contains("第一行"));
    assert!(read.contains("第二行"));
    assert!(read.contains("     1\t"), "应带行号: {read}");

    // 覆盖写要报"已覆盖"，不是"已创建"
    let again = reg
        .run("write_file", json!({"path": "a.txt", "content": "x"}), &ctx)
        .await
        .unwrap();
    assert!(again.contains("已覆盖"), "unexpected: {again}");
}

#[tokio::test]
async fn write_creates_parent_dirs() {
    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    registry()
        .run(
            "write_file",
            json!({"path": "deep/nested/dir/file.txt", "content": "ok"}),
            &ctx,
        )
        .await
        .unwrap();

    assert!(ws.path.join("deep/nested/dir/file.txt").is_file());
}

#[tokio::test]
async fn read_supports_offset_and_limit() {
    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let reg = registry();
    let body = (1..=10)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    reg.run("write_file", json!({"path": "n.txt", "content": body}), &ctx)
        .await
        .unwrap();

    let out = reg
        .run("read_file", json!({"path": "n.txt", "offset": 3, "limit": 2}), &ctx)
        .await
        .unwrap();
    assert!(out.contains("line3"));
    assert!(out.contains("line4"));
    assert!(!out.contains("line2"));
    assert!(!out.contains("line5"));
    assert!(out.contains("全文共 10 行"));
}

#[tokio::test]
async fn rejects_parent_escape() {
    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let reg = registry();

    for raw in ["../secret.txt", "a/../../secret.txt", "../../../../etc/passwd"] {
        let err = reg
            .run("read_file", json!({"path": raw}), &ctx)
            .await
            .expect_err("必须拒绝越界路径");
        assert!(err.to_string().contains("越界"), "{raw} -> {err}");

        let err = reg
            .run("write_file", json!({"path": raw, "content": "x"}), &ctx)
            .await
            .expect_err("必须拒绝越界写入");
        assert!(err.to_string().contains("越界"), "{raw} -> {err}");
    }
}

#[tokio::test]
async fn rejects_oversized_file() {
    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let reg = registry();

    // 直接落到磁盘上，绕过 write_file 的上限，验证 read_file 自己也挡。
    let big = vec![b'a'; (goldenagent_tools::MAX_READ_BYTES + 1) as usize];
    std::fs::write(ws.path.join("big.txt"), &big).unwrap();

    let err = reg
        .run("read_file", json!({"path": "big.txt"}), &ctx)
        .await
        .expect_err("必须拒绝超大文件");
    assert!(err.to_string().contains("上限"), "{err}");
}

#[tokio::test]
async fn rejects_oversized_write() {
    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let huge = "x".repeat(1024 * 1024 + 1);
    let err = registry()
        .run("write_file", json!({"path": "huge.txt", "content": huge}), &ctx)
        .await
        .expect_err("必须拒绝超大写入");
    assert!(err.to_string().contains("上限"), "{err}");
}

#[tokio::test]
async fn list_dir_marks_directories_and_files() {
    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let reg = registry();
    reg.run("write_file", json!({"path": "src/main.rs", "content": "fn main() {}"}), &ctx)
        .await
        .unwrap();

    let out = reg.run("list_dir", json!({}), &ctx).await.unwrap();
    assert!(out.contains("src/"), "{out}");
    assert!(out.contains("1 项"), "{out}");

    let nested = reg
        .run("list_dir", json!({"path": "src"}), &ctx)
        .await
        .unwrap();
    assert!(nested.contains("main.rs"), "{nested}");
}

#[tokio::test]
async fn missing_required_argument_is_reported() {
    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let err = registry()
        .run("read_file", json!({}), &ctx)
        .await
        .expect_err("缺 path 必须报错");
    assert!(err.to_string().contains("path"), "{err}");
}

#[tokio::test]
async fn unknown_tool_is_reported() {
    let ws = TempWorkspace::new();
    let err = registry()
        .run("rm_rf", json!({}), &ws.ctx())
        .await
        .expect_err("未知工具必须报错");
    assert!(err.to_string().contains("未知工具"), "{err}");
}

#[test]
fn default_registry_exposes_all_tools() {
    let mut names = registry().names();
    names.sort();
    assert_eq!(
        names,
        vec![
            "apply_patch",
            "bash",
            "glob",
            "grep",
            "list_dir",
            "read_file",
            "write_file"
        ]
    );

    // policy 靠 kind 判断，不靠工具名——这里锁死每个工具的 kind。
    let reg = registry();
    for name in ["read_file", "list_dir", "grep", "glob"] {
        assert_eq!(
            reg.spec(name).unwrap().kind,
            goldenagent_core::ToolKind::Read,
            "{name} 应是读类"
        );
    }
    for name in ["write_file", "apply_patch"] {
        assert_eq!(
            reg.spec(name).unwrap().kind,
            goldenagent_core::ToolKind::Write,
            "{name} 应是写类"
        );
    }
    assert_eq!(
        reg.spec("bash").unwrap().kind,
        goldenagent_core::ToolKind::Exec,
        "bash 必须是执行类，不能被当成写类按路径放行"
    );
}
