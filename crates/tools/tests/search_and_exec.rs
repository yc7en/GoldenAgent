//! B13 / B14 新工具的集成验收：真文件系统 + 真进程。
//!
//! 重点仍然在**边界**上：搜不到的要说清为什么搜不到、改错地方的要拒绝、
//! 命令越界的要拒、超时的要真的被杀掉。

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
            "goldenagent-search-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn ctx(&self) -> ToolContext {
        ToolContext::new(&self.path)
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
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

// ───────────────────────────── grep ─────────────────────────────

#[tokio::test]
async fn grep_returns_path_line_and_content() {
    let ws = TempWorkspace::new();
    ws.write("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n");
    ws.write("src/lib.rs", "// 没有关键词\n");
    let reg = registry();

    let out = reg
        .run("grep", json!({"pattern": "println"}), &ws.ctx())
        .await
        .unwrap();

    assert!(out.contains("1 处命中"), "{out}");
    assert!(out.contains("src/main.rs:2:"), "要能定位到行: {out}");
    assert!(out.contains("println!"), "{out}");
}

#[tokio::test]
async fn grep_filters_by_glob_and_ignores_case() {
    let ws = TempWorkspace::new();
    ws.write("a.rs", "TODO: 改这里\n");
    ws.write("b.ts", "TODO: 还有这里\n");
    ws.write("c.rs", "todo: 小写的也算\n");
    let reg = registry();

    let only_rs = reg
        .run("grep", json!({"pattern": "TODO", "glob": "*.rs"}), &ws.ctx())
        .await
        .unwrap();
    assert!(only_rs.contains("a.rs"), "{only_rs}");
    assert!(!only_rs.contains("b.ts"), "glob 过滤没生效: {only_rs}");

    let case_insensitive = reg
        .run(
            "grep",
            json!({"pattern": "todo", "ignore_case": true, "glob": "*.rs"}),
            &ws.ctx(),
        )
        .await
        .unwrap();
    assert!(case_insensitive.contains("2 处命中"), "{case_insensitive}");
}

#[tokio::test]
async fn grep_reports_no_match_and_says_what_it_skipped() {
    let ws = TempWorkspace::new();
    ws.write("src/a.rs", "无关内容\n");
    ws.write("node_modules/big.js", "关键词在这里，但应该被跳过\n");
    let reg = registry();

    let out = reg
        .run("grep", json!({"pattern": "关键词"}), &ws.ctx())
        .await
        .unwrap();
    assert!(out.contains("没有匹配"), "{out}");
    assert!(out.contains("扫描 1 个文件"), "node_modules 应被跳过: {out}");
}

#[tokio::test]
async fn grep_caps_hits_and_says_it_did() {
    let ws = TempWorkspace::new();
    let body = (0..50).map(|i| format!("target {i}")).collect::<Vec<_>>().join("\n");
    ws.write("many.txt", &body);
    let reg = registry();

    let out = reg
        .run("grep", json!({"pattern": "target", "max_hits": 5}), &ws.ctx())
        .await
        .unwrap();
    assert!(out.contains("5 处命中"), "{out}");
    assert!(out.contains("上限"), "截断必须说出来: {out}");
}

#[tokio::test]
async fn grep_rejects_escape_path() {
    let ws = TempWorkspace::new();
    let err = registry()
        .run("grep", json!({"pattern": "x", "path": "../"}), &ws.ctx())
        .await
        .expect_err("越界必须拒绝");
    assert!(err.to_string().contains("越界"), "{err}");
}

// ───────────────────────────── glob ─────────────────────────────

#[tokio::test]
async fn glob_lists_matching_files_across_depths() {
    let ws = TempWorkspace::new();
    ws.write("README.md", "x");
    ws.write("src/main.rs", "x");
    ws.write("src/deep/util.rs", "x");
    ws.write("docs/guide.md", "x");
    let reg = registry();

    let rust = reg
        .run("glob", json!({"pattern": "**/*.rs"}), &ws.ctx())
        .await
        .unwrap();
    assert!(rust.contains("2 个文件"), "{rust}");
    assert!(rust.contains("src/main.rs"), "{rust}");
    assert!(rust.contains("src/deep/util.rs"), "{rust}");

    // 不含 / 的模式只比文件名，任意层级都能命中
    let any_md = reg
        .run("glob", json!({"pattern": "*.md"}), &ws.ctx())
        .await
        .unwrap();
    assert!(any_md.contains("README.md"), "{any_md}");
    assert!(any_md.contains("docs/guide.md"), "{any_md}");
}

#[tokio::test]
async fn glob_respects_limit_and_reports_empty() {
    let ws = TempWorkspace::new();
    for i in 0..10 {
        ws.write(&format!("f{i}.txt"), "x");
    }
    let reg = registry();

    let limited = reg
        .run("glob", json!({"pattern": "*.txt", "limit": 3}), &ws.ctx())
        .await
        .unwrap();
    assert!(limited.contains("3 个文件"), "{limited}");
    assert!(limited.contains("上限"), "{limited}");

    let none = reg
        .run("glob", json!({"pattern": "*.nope"}), &ws.ctx())
        .await
        .unwrap();
    assert!(none.contains("没有匹配"), "{none}");
}

// ─────────────────────────── apply_patch ───────────────────────────

#[tokio::test]
async fn apply_patch_replaces_unique_block() {
    let ws = TempWorkspace::new();
    ws.write("code.rs", "fn main() {\n    old_call();\n}\n");
    let reg = registry();

    let out = reg
        .run(
            "apply_patch",
            json!({
                "path": "code.rs",
                "edits": [{ "old": "fn main() {\n    old_call();", "new": "fn main() {\n    new_call();" }]
            }),
            &ws.ctx(),
        )
        .await
        .unwrap();

    assert!(out.contains("1 处替换"), "{out}");
    let after = std::fs::read_to_string(ws.path.join("code.rs")).unwrap();
    assert_eq!(after, "fn main() {\n    new_call();\n}\n");
}

#[tokio::test]
async fn apply_patch_refuses_ambiguous_match() {
    let ws = TempWorkspace::new();
    ws.write("dup.rs", "a();\na();\n");
    let reg = registry();

    let err = reg
        .run(
            "apply_patch",
            json!({"path": "dup.rs", "edits": [{"old": "a();", "new": "b();"}]}),
            &ws.ctx(),
        )
        .await
        .expect_err("多处命中必须拒绝");

    assert!(err.to_string().contains("出现了 2 次"), "{err}");
    // 拒绝时文件不能被改脏
    assert_eq!(
        std::fs::read_to_string(ws.path.join("dup.rs")).unwrap(),
        "a();\na();\n"
    );
}

#[tokio::test]
async fn apply_patch_all_flag_replaces_every_occurrence() {
    let ws = TempWorkspace::new();
    ws.write("dup.rs", "a();\na();\n");
    let reg = registry();

    let out = reg
        .run(
            "apply_patch",
            json!({"path": "dup.rs", "edits": [{"old": "a();", "new": "b();", "all": true}]}),
            &ws.ctx(),
        )
        .await
        .unwrap();

    assert!(out.contains("2 处替换"), "{out}");
    assert_eq!(
        std::fs::read_to_string(ws.path.join("dup.rs")).unwrap(),
        "b();\nb();\n"
    );
}

#[tokio::test]
async fn apply_patch_reports_missing_old_text() {
    let ws = TempWorkspace::new();
    ws.write("code.rs", "fn main() {}\n");
    let reg = registry();

    let err = reg
        .run(
            "apply_patch",
            json!({"path": "code.rs", "edits": [{"old": "fn 不存在()", "new": "x"}]}),
            &ws.ctx(),
        )
        .await
        .expect_err("找不到原文必须报错");

    assert!(err.to_string().contains("找不到原文"), "{err}");
    assert!(err.to_string().contains("read_file"), "要提示重读: {err}");
}

#[tokio::test]
async fn apply_patch_on_missing_file_suggests_write_file() {
    let ws = TempWorkspace::new();
    let err = registry()
        .run(
            "apply_patch",
            json!({"path": "nope.rs", "edits": [{"old": "a", "new": "b"}]}),
            &ws.ctx(),
        )
        .await
        .expect_err("文件不存在必须报错");
    assert!(err.to_string().contains("write_file"), "{err}");
}

// ───────────────────────────── bash ─────────────────────────────

#[cfg(windows)]
const ECHO_CMD: &str = "echo hello-from-bash";
#[cfg(not(windows))]
const ECHO_CMD: &str = "echo hello-from-bash";

#[cfg(windows)]
const FAIL_CMD: &str = "exit /b 3";
#[cfg(not(windows))]
const FAIL_CMD: &str = "exit 3";

#[cfg(windows)]
const SLEEP_CMD: &str = "ping -n 30 127.0.0.1 > nul";
#[cfg(not(windows))]
const SLEEP_CMD: &str = "sleep 30";

#[tokio::test]
async fn bash_runs_command_and_returns_output() {
    let ws = TempWorkspace::new();
    let out = registry()
        .run("bash", json!({ "command": ECHO_CMD }), &ws.ctx())
        .await
        .unwrap();

    assert!(out.contains("退出码 0"), "{out}");
    assert!(out.contains("hello-from-bash"), "{out}");
}

#[tokio::test]
async fn bash_reports_nonzero_exit_code() {
    let ws = TempWorkspace::new();
    let out = registry()
        .run("bash", json!({ "command": FAIL_CMD }), &ws.ctx())
        .await
        .unwrap();

    assert!(out.contains("退出码 3"), "非零退出码要让模型看见: {out}");
}

#[tokio::test]
async fn bash_stderr_is_separated() {
    let ws = TempWorkspace::new();
    #[cfg(windows)]
    let command = "echo oops 1>&2";
    #[cfg(not(windows))]
    let command = "echo oops 1>&2";

    let out = registry()
        .run("bash", json!({ "command": command }), &ws.ctx())
        .await
        .unwrap();

    assert!(out.contains("--- stderr ---"), "{out}");
    assert!(out.contains("oops"), "{out}");
}

#[tokio::test]
async fn bash_kills_command_on_timeout() {
    let ws = TempWorkspace::new();
    let started = std::time::Instant::now();

    let err = registry()
        .run(
            "bash",
            json!({ "command": SLEEP_CMD, "timeout_secs": 1 }),
            &ws.ctx(),
        )
        .await
        .expect_err("超时必须报错");

    assert!(err.to_string().contains("超过 1 秒"), "{err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "超时后必须真的收手，不能等命令自己结束"
    );
}

#[tokio::test]
async fn bash_cwd_cannot_escape_workspace() {
    let ws = TempWorkspace::new();
    let err = registry()
        .run("bash", json!({ "command": ECHO_CMD, "cwd": "../.." }), &ws.ctx())
        .await
        .expect_err("工作目录越界必须拒绝");
    assert!(err.to_string().contains("越界"), "{err}");
}

#[tokio::test]
async fn bash_missing_command_argument_is_reported() {
    let ws = TempWorkspace::new();
    let err = registry()
        .run("bash", json!({}), &ws.ctx())
        .await
        .expect_err("缺 command 必须报错");
    assert!(err.to_string().contains("command"), "{err}");
}

// ─────────────────────── 输出编码（B19） ───────────────────────

/// 中文文件名过一轮 `dir` 回来必须还是原样。
///
/// 这条走的是 cmd 最顽固的那条路径：`dir /b` 不带通配符时按 ANSI 代码页写字节，
/// 光靠 `chcp 65001` 治不好，必须靠解码侧兜底。
#[cfg(windows)]
#[tokio::test]
async fn bash_decodes_chinese_filenames_from_dir() {
    let ws = TempWorkspace::new();
    ws.write("中文文件.md", "x");
    ws.write("子目录/说明.txt", "x");

    let out = registry()
        .run("bash", json!({ "command": "dir /b 2>&1" }), &ws.ctx())
        .await
        .unwrap();

    assert!(out.contains("中文文件.md"), "文件名不能被解成乱码: {out}");
    assert!(out.contains("子目录"), "目录名不能被解成乱码: {out}");
    assert!(!out.contains('\u{FFFD}'), "输出里不该出现替换字符: {out}");
}

/// stdout 与 stderr 各自解码：两段可能是不同编码，不能拼起来一起判。
#[tokio::test]
async fn bash_keeps_chinese_output_and_stderr_intact() {
    let ws = TempWorkspace::new();
    #[cfg(windows)]
    let command = "echo 中文输出 & echo 中文错误 1>&2";
    #[cfg(not(windows))]
    let command = "echo 中文输出; echo 中文错误 1>&2";

    let out = registry()
        .run("bash", json!({ "command": command }), &ws.ctx())
        .await
        .unwrap();

    assert!(out.contains("中文输出"), "{out}");
    assert!(out.contains("中文错误"), "{out}");
    assert!(!out.contains('\u{FFFD}'), "输出里不该出现替换字符: {out}");
}

/// 前缀是 `&` 不是 `&&`，所以用户自己串联的多条命令语义不变。
#[cfg(windows)]
#[tokio::test]
async fn bash_prefix_does_not_eat_the_command_chain() {
    let ws = TempWorkspace::new();
    let out = registry()
        .run("bash", json!({ "command": "echo first & echo second" }), &ws.ctx())
        .await
        .unwrap();

    assert!(out.contains("退出码 0"), "{out}");
    assert!(out.contains("first"), "{out}");
    assert!(out.contains("second"), "{out}");
}
