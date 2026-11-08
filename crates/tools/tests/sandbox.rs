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
            "goldenagent-sandbox-{}-{stamp}-{seq}",
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
async fn test_env_secret_masking_in_bash() {
    // 1. 在当前宿主进程环境变量中植入假敏感 Key
    std::env::set_var("OPENAI_API_KEY", "sk-mock-secret-key-12345");
    std::env::set_var("ANTHROPIC_AUTH_TOKEN", "anthropic-token-xyz");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "aws-secret-access-999");
    std::env::set_var("GOLDENAGENT_INTERNAL_PASSWORD", "super-secret-pass");

    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let r = reg();

    // 2. 在 bash 中执行 dump 全量环境变量的系统命令
    #[cfg(windows)]
    let cmd = "set";
    #[cfg(not(windows))]
    let cmd = "env";

    let res = r
        .run("bash", json!({ "command": cmd }), &ctx)
        .await
        .expect("执行环境命令应成功");

    // 3. 严格断言：输出中绝不包含任何植入的敏感变量名与值！
    assert!(
        !res.contains("OPENAI_API_KEY"),
        "bash 输出中绝不能泄露 OPENAI_API_KEY"
    );
    assert!(
        !res.contains("sk-mock-secret-key-12345"),
        "bash 输出中绝不能泄露 API Key 密钥值"
    );
    assert!(
        !res.contains("ANTHROPIC_AUTH_TOKEN"),
        "bash 输出中绝不能泄露 ANTHROPIC_AUTH_TOKEN"
    );
    assert!(
        !res.contains("AWS_SECRET_ACCESS_KEY"),
        "bash 输出中绝不能泄露 AWS 凭据"
    );
    assert!(
        !res.contains("GOLDENAGENT_INTERNAL_PASSWORD"),
        "bash 输出中绝不能泄露内部密码"
    );

    // 清理测试环境变量
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
    std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    std::env::remove_var("GOLDENAGENT_INTERNAL_PASSWORD");
}

#[tokio::test]
async fn test_env_secret_masking_in_terminal() {
    std::env::set_var("OPENAI_API_KEY", "sk-terminal-secret-test");
    std::env::set_var("SSH_PRIVATE_KEY", "fake-ssh-private-key-material");

    let ws = TempWorkspace::new();
    let ctx = ws.ctx();
    let r = reg();

    let create_res = r
        .run("terminal_create", json!({}), &ctx)
        .await
        .expect("创建终端应成功");
    let create_val: serde_json::Value = serde_json::from_str(&create_res).unwrap();
    let term_id = create_val["terminal_id"].as_str().unwrap();

    #[cfg(windows)]
    let cmd = "set\r\n";
    #[cfg(not(windows))]
    let cmd = "env\n";

    r.run(
        "terminal_write",
        json!({ "terminal_id": term_id, "input": cmd }),
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

    assert!(
        !output.contains("OPENAI_API_KEY"),
        "terminal 输出中绝不能泄露 OPENAI_API_KEY"
    );
    assert!(
        !output.contains("SSH_PRIVATE_KEY"),
        "terminal 输出中绝不能泄露 SSH 密钥"
    );

    r.run("terminal_kill", json!({ "terminal_id": term_id }), &ctx)
        .await
        .unwrap();

    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("SSH_PRIVATE_KEY");
}

#[cfg(windows)]
#[test]
fn test_windows_job_object_limits() {
    use goldenagent_tools::sandbox::windows::get_global_job;

    let job = get_global_job().expect("获取全局 Job Object 应成功");
    let current_proc = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
    let assign_res = job.assign_process(current_proc as _);
    assert!(assign_res.is_ok(), "关联 Job Object 应当成功: {:?}", assign_res);
}
