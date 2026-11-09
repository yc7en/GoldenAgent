use std::path::PathBuf;
use serde_json::json;
use goldenagent_core::LspDiagnostic;
use goldenagent_tools::lsp::tools::{apply_text_edits, RawTextEdit};
use goldenagent_tools::lsp::{path_to_uri, LspClient};
use goldenagent_tools::{Registry, ToolContext};

fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ga_lsp_test_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    dir
}

#[test]
fn test_lsp_position_and_text_edits() {
    let content = "fn calculate_total() -> i32 {\n    let val = calculate_total();\n    val\n}";
    // 编辑 1: 第一行 calculate_total -> compute_sum
    // 编辑 2: 第二行 calculate_total -> compute_sum
    let edits = vec![
        RawTextEdit {
            start_line: 0,
            start_char: 3,
            end_line: 0,
            end_char: 18,
            new_text: "compute_sum".to_string(),
        },
        RawTextEdit {
            start_line: 1,
            start_char: 14,
            end_line: 1,
            end_char: 29,
            new_text: "compute_sum".to_string(),
        },
    ];

    let updated = apply_text_edits(content, &edits).expect("替换应当成功");
    assert_eq!(
        updated,
        "fn compute_sum() -> i32 {\n    let val = compute_sum();\n    val\n}"
    );
}

#[tokio::test]
async fn test_lsp_definition_and_references_tool() {
    let ws = temp_workspace("def_ref");
    let main_rs = ws.join("src").join("main.rs");
    let lib_rs = ws.join("src").join("lib.rs");

    std::fs::write(&main_rs, "use crate::calc;\nfn main() { calc(); }").unwrap();
    std::fs::write(&lib_rs, "pub fn calc() {\n    println!(\"ok\");\n}").unwrap();

    let lib_uri = path_to_uri(&lib_rs);
    let main_uri = path_to_uri(&main_rs);

    let mock_client = LspClient::mock(ws.clone(), move |req| {
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        match method {
            "initialize" => Some(json!({"result": {"capabilities": {}}})),
            "textDocument/definition" => Some(json!({
                "result": [
                    {
                        "uri": lib_uri.clone(),
                        "range": {
                            "start": { "line": 0, "character": 7 },
                            "end": { "line": 0, "character": 11 }
                        }
                    }
                ]
            })),
            "textDocument/references" => Some(json!({
                "result": [
                    {
                        "uri": lib_uri.clone(),
                        "range": {
                            "start": { "line": 0, "character": 7 },
                            "end": { "line": 0, "character": 11 }
                        }
                    },
                    {
                        "uri": main_uri.clone(),
                        "range": {
                            "start": { "line": 1, "character": 12 },
                            "end": { "line": 1, "character": 16 }
                        }
                    }
                ]
            })),
            _ => None,
        }
    });

    let ctx = ToolContext::new(&ws);
    ctx.lsp.register_mock_client("rust", mock_client).await;
    let reg = Registry::with_defaults();

    // 1. 测试 lsp_definition
    let def_res = reg.run("lsp_definition", json!({
        "path": "src/main.rs",
        "line": 2,
        "character": 13,
    }), &ctx).await.expect("lsp_definition 应当成功返回");

    assert!(def_res.contains("找到 1 处定义"));
    assert!(def_res.contains("src/lib.rs:1:8") || def_res.contains("lib.rs:1:8"));
    assert!(def_res.contains("pub fn calc()"));

    // 2. 测试 lsp_references
    let ref_res = reg.run("lsp_references", json!({
        "path": "src/lib.rs",
        "line": 1,
        "character": 8,
    }), &ctx).await.expect("lsp_references 应当成功返回");

    assert!(ref_res.contains("共找到 2 处引用"));
    assert!(ref_res.contains("lib.rs"));
    assert!(ref_res.contains("main.rs"));

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn test_lsp_diagnostics_tool() {
    let ws = temp_workspace("diag");
    let main_rs = ws.join("src").join("main.rs");
    std::fs::write(&main_rs, "fn main() { let x: i32 = \"abc\"; }").unwrap();

    let mock_client = LspClient::mock(ws.clone(), |_| None);
    mock_client.inject_diagnostics(&main_rs, vec![
        LspDiagnostic::new(
            main_rs.to_string_lossy().to_string(),
            1,
            24,
            "Error",
            "mismatched types: expected i32, found &str",
            Some("E0308".to_string()),
        )
    ]).await;

    let ctx = ToolContext::new(&ws);
    ctx.lsp.register_mock_client("rust", mock_client).await;
    let reg = Registry::with_defaults();

    let diag_res = reg.run("lsp_diagnostics", json!({
        "path": "src/main.rs"
    }), &ctx).await.expect("lsp_diagnostics 应当成功返回");

    assert!(diag_res.contains("发现 1 条编译器诊断信息"));
    assert!(diag_res.contains("[Error]"));
    assert!(diag_res.contains("mismatched types"));
    assert!(diag_res.contains("[E0308]"));

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn test_lsp_rename_tool_modifies_files() {
    let ws = temp_workspace("rename");
    let main_rs = ws.join("src").join("main.rs");
    let lib_rs = ws.join("src").join("lib.rs");

    std::fs::write(&main_rs, "fn main() { old_func(); }").unwrap();
    std::fs::write(&lib_rs, "pub fn old_func() {}").unwrap();

    let main_uri = path_to_uri(&main_rs);
    let lib_uri = path_to_uri(&lib_rs);

    let mock_client = LspClient::mock(ws.clone(), move |req| {
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if method == "textDocument/rename" {
            Some(json!({
                "result": {
                    "changes": {
                        main_uri.clone(): [
                            {
                                "range": {
                                    "start": { "line": 0, "character": 12 },
                                    "end": { "line": 0, "character": 20 }
                                },
                                "newText": "new_func"
                            }
                        ],
                        lib_uri.clone(): [
                            {
                                "range": {
                                    "start": { "line": 0, "character": 7 },
                                    "end": { "line": 0, "character": 15 }
                                },
                                "newText": "new_func"
                            }
                        ]
                    }
                }
            }))
        } else {
            None
        }
    });

    let ctx = ToolContext::new(&ws);
    ctx.lsp.register_mock_client("rust", mock_client).await;
    let reg = Registry::with_defaults();

    let rename_res = reg.run("lsp_rename", json!({
        "path": "src/lib.rs",
        "line": 1,
        "character": 8,
        "new_name": "new_func"
    }), &ctx).await.expect("lsp_rename 应当成功应用修改");

    assert!(rename_res.contains("成功将符号重命名为 'new_func'"));
    assert!(rename_res.contains("共涉及 2 个文件"));

    // 验证文件在磁盘上已被修改
    let main_content = std::fs::read_to_string(&main_rs).unwrap();
    assert_eq!(main_content, "fn main() { new_func(); }");

    let lib_content = std::fs::read_to_string(&lib_rs).unwrap();
    assert_eq!(lib_content, "pub fn new_func() {}");

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn test_lsp_fallback_when_binary_missing() {
    let ws = temp_workspace("fallback");
    let fake_go = ws.join("src").join("main.go");
    std::fs::write(&fake_go, "package main").unwrap();

    let ctx = ToolContext::new(&ws);
    // 不注册 mock，让系统尝试启动 gopls
    let reg = Registry::with_defaults();
    let res = reg.run("lsp_definition", json!({
        "path": "src/main.go",
        "line": 1,
        "character": 1
    }), &ctx).await;

    // 若机器上无 gopls，返回友好的降级提示
    match res {
        Err(e) => {
            let err_str = e.to_string();
            assert!(
                err_str.contains("gopls") || err_str.contains("grep") || err_str.contains("read_file"),
                "应当返回包含建议降级到 grep/read_file 的提示: {err_str}"
            );
        }
        Ok(_) => {
            // 如果机器刚好装了 gopls，也是允许的
        }
    }

    let _ = std::fs::remove_dir_all(&ws);
}
