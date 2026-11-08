use goldenagent_tools::terminal::strip_ansi;
use goldenagent_tools::{Registry, ToolContext};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
            "goldenagent-term-{}-{stamp}-{seq}",
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

fn reg() -> Registry {
    Registry::with_defaults()
}

#[tokio::test]
async fn terminal_create_write_read_and_kill_flow() {
    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let r = reg();

    // 1. 创建终端会话
    let create_res = r
        .run("terminal_create", json!({ "name": "test-term" }), &ctx)
        .await
        .expect("创建终端应成功");
    let create_val: serde_json::Value = serde_json::from_str(&create_res).unwrap();
    let term_id = create_val["terminal_id"].as_str().unwrap();
    assert!(term_id.starts_with("term-"));
    assert_eq!(create_val["name"], "test-term");

    // 2. 写入命令并回车
    #[cfg(windows)]
    let cmd = "echo term_lifecycle_ok\r\n";
    #[cfg(not(windows))]
    let cmd = "echo term_lifecycle_ok\n";

    let write_res = r
        .run(
            "terminal_write",
            json!({ "terminal_id": term_id, "input": cmd }),
            &ctx,
        )
        .await
        .expect("写入命令应成功");
    let write_val: serde_json::Value = serde_json::from_str(&write_res).unwrap();
    assert!(write_val["bytes_written"].as_u64().unwrap() > 0);

    // 3. 读取终端输出增量（带 Inactivity Detection）
    let read_res = r
        .run(
            "terminal_read",
            json!({ "terminal_id": term_id, "timeout_ms": 3000, "inactivity_ms": 300 }),
            &ctx,
        )
        .await
        .expect("读取终端应成功");
    let read_val: serde_json::Value = serde_json::from_str(&read_res).unwrap();
    let output = read_val["output"].as_str().unwrap();
    println!("Read output: {:?}", output);
    assert!(output.contains("term_lifecycle_ok"));

    // 4. 终止终端会话
    let kill_res = r
        .run("terminal_kill", json!({ "terminal_id": term_id }), &ctx)
        .await
        .expect("终止终端应成功");
    let kill_val: serde_json::Value = serde_json::from_str(&kill_res).unwrap();
    assert_eq!(kill_val["killed"], true);

    // 5. 再次写入应报错
    let write_err = r
        .run(
            "terminal_write",
            json!({ "terminal_id": term_id, "input": "echo fail\r\n" }),
            &ctx,
        )
        .await
        .expect_err("已终止的终端写入必须报错");
    let write_err_msg = write_err.to_string();
    assert!(
        write_err_msg.contains("终端已退出") || write_err_msg.contains("未找到终端")
    );
}

#[tokio::test]
async fn terminal_state_persists_directory_across_calls() {
    let ws = TempWorkspace::new();
    let sub = ws.path.join("deep_folder");
    std::fs::create_dir_all(&sub).unwrap();

    let ctx = ws.ctx();
    let r = reg();

    let create_res = r
        .run("terminal_create", json!({}), &ctx)
        .await
        .expect("创建终端应成功");
    let create_val: serde_json::Value = serde_json::from_str(&create_res).unwrap();
    let term_id = create_val["terminal_id"].as_str().unwrap();

    // 1. cd 进子目录
    #[cfg(windows)]
    let cd_cmd = "cd deep_folder\r\n";
    #[cfg(not(windows))]
    let cd_cmd = "cd deep_folder\n";

    r.run(
        "terminal_write",
        json!({ "terminal_id": term_id, "input": cd_cmd }),
        &ctx,
    )
    .await
    .unwrap();

    // 读取一次排空
    let _ = r
        .run("terminal_read", json!({ "terminal_id": term_id }), &ctx)
        .await
        .unwrap();

    // 2. 在下一轮独立工具调用中执行查看当前路径命令，验证状态未丢失！
    #[cfg(windows)]
    let pwd_cmd = "echo CURRENT_DIR=%CD%\r\n";
    #[cfg(not(windows))]
    let pwd_cmd = "echo CURRENT_DIR=$(pwd)\n";

    r.run(
        "terminal_write",
        json!({ "terminal_id": term_id, "input": pwd_cmd }),
        &ctx,
    )
    .await
    .unwrap();

    let read_res = r
        .run(
            "terminal_read",
            json!({ "terminal_id": term_id, "timeout_ms": 3000, "inactivity_ms": 300 }),
            &ctx,
        )
        .await
        .unwrap();
    let read_val: serde_json::Value = serde_json::from_str(&read_res).unwrap();
    let output = read_val["output"].as_str().unwrap();
    println!("Directory output: {:?}", output);
    assert!(output.contains("CURRENT_DIR="));
    assert!(output.contains("deep_folder"), "必须保持在 deep_folder 目录中");

    r.run("terminal_kill", json!({ "terminal_id": term_id }), &ctx)
        .await
        .unwrap();
}

#[test]
fn test_strip_ansi_function() {
    let raw = "\u{1b}[?9001h\u{1b}[32mColored Text\u{1b}[0m\u{1b}]0;Title\u{7}Plain";
    let cleaned = strip_ansi(raw);
    assert_eq!(cleaned, "Colored TextPlain");
}
