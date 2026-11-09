use goldenagent_tools::{InspectSpill, SpillManager, Tool, ToolContext};
use serde_json::json;

struct TempWorkspace {
    path: std::path::PathBuf,
}

impl TempWorkspace {
    fn new(name: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("goldenagent-spill-{name}-{stamp}"));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[tokio::test]
async fn test_spill_threshold_and_handle() {
    let ws = TempWorkspace::new("threshold");
    let manager = SpillManager::new(&ws.path);

    // 小输出：低于 32 KiB 不触发溢出
    let small_output = "hello world small output\n".repeat(10);
    let maybe_handle = manager.maybe_spill(&small_output).await.unwrap();
    assert!(maybe_handle.is_none());

    // 大输出：高于 32 KiB 触发溢出落盘
    let mut big_output = String::new();
    for i in 1..=2000 {
        big_output.push_str(&format!("Line {i:04}: compile log step information...\n"));
    }
    assert!(big_output.len() > 32 * 1024);

    let handle = manager
        .maybe_spill(&big_output)
        .await
        .unwrap()
        .expect("超过 32 KiB 必须触发 Spill 句柄");

    assert_eq!(handle.status, "spilled");
    assert!(handle.file.starts_with(".goldenagent/spill/"));
    assert_eq!(handle.total_lines, 2000);
    assert_eq!(handle.total_bytes, big_output.len());

    // 验证 Head 预览包含第 1 行
    assert!(handle.preview_head.contains("Line 0001:"));
    // 验证 Tail 预览包含最后 1 行
    assert!(handle.preview_tail.contains("Line 2000:"));

    // 验证磁盘上文件完整存在
    let disk_file = ws.path.join(&handle.file);
    assert!(disk_file.exists());
    let saved_content = tokio::fs::read_to_string(&disk_file).await.unwrap();
    assert_eq!(saved_content, big_output);
}

#[tokio::test]
async fn test_inspect_spill_tool() {
    let ws = TempWorkspace::new("inspect");
    let ctx = ToolContext::new(&ws.path);
    let manager = SpillManager::new(&ws.path);

    // 制造 1000 行大输出并落盘
    let mut big_output = String::new();
    for i in 1..=1000 {
        big_output.push_str(&format!("CompileMessage #{i:04}: check line\n"));
    }
    let handle = manager.maybe_spill(&big_output).await.unwrap().unwrap();

    let tool = InspectSpill;

    // 1. 查阅第 950 到 970 行
    let res = tool
        .run(
            json!({
                "file": handle.file,
                "start_line": 950,
                "count": 20
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(res.contains("950 | CompileMessage #0950:"));
    assert!(res.contains("969 | CompileMessage #0969:"));
    assert!(!res.contains("CompileMessage #0940:"));
    assert!(!res.contains("CompileMessage #0980:"));

    // 2. 查阅越界路径必须被拒绝（防穿透）
    let bad_res = tool
        .run(
            json!({
                "file": "../../secret.txt",
                "start_line": 1
            }),
            &ctx,
        )
        .await;
    assert!(bad_res.is_err(), "越界路径必须被拒绝");
}
